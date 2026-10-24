//! Disc image model: CUE sheets, CloneCD control files and raw images
//! (ISO/BIN/IMG/MM2), with sector access normalised to 2352-byte raw frames.

use crate::cd::{self, RAW, Sector};
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Audio,
    Mode1,
    Mode2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Cue,
    CloneCd,
    Raw,
}

#[derive(Clone, Debug)]
pub struct ImageFile {
    pub path: PathBuf,
    /// "BINARY", "MOTOROLA" or "WAVE".
    pub ftype: String,
    /// Byte offset of the first sector (non-zero for WAVE files).
    pub data_offset: u64,
    pub data_len: u64,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub number: u8,
    pub kind: Kind,
    pub sector_size: usize,
    /// Mode string exactly as it appeared in the descriptor (e.g. "MODE2/2352").
    pub mode_text: String,
    pub file: usize,
    /// (index number, frame offset inside the file).
    pub indexes: Vec<(u8, u32)>,
    /// PREGAP/POSTGAP lengths that are not stored in the file.
    pub pregap: u32,
    pub postgap: u32,
    /// Lines carried through unchanged when the cue is rewritten (FLAGS, ISRC, TITLE...).
    pub extra: Vec<String>,
    /// Region of the file owned by this track: from its first index (or the
    /// file start for the first track of a file) to the next track.
    pub region_frame: u32,
    pub region_byte: u64,
    pub region_sectors: u32,
    pub region_lba: u32,
}

impl Track {
    pub fn index(&self, n: u8) -> Option<u32> {
        self.indexes.iter().find(|i| i.0 == n).map(|i| i.1)
    }

    /// Disc LBA of INDEX 01 (where the track officially starts).
    pub fn start_lba(&self) -> u32 {
        let i1 = self.index(1).unwrap_or(self.region_frame);
        self.region_lba + (i1 - self.region_frame)
    }

    pub fn end_lba(&self) -> u32 {
        self.region_lba + self.region_sectors
    }

    pub fn is_data(&self) -> bool {
        self.kind != Kind::Audio
    }

    pub fn raw_mode_text(&self) -> &'static str {
        match self.kind {
            Kind::Audio => "AUDIO",
            Kind::Mode1 => "MODE1/2352",
            Kind::Mode2 => "MODE2/2352",
        }
    }

    pub fn mode_byte(&self) -> Option<u8> {
        match self.kind {
            Kind::Audio => None,
            Kind::Mode1 => Some(1),
            Kind::Mode2 => Some(2),
        }
    }
}

pub struct Image {
    pub path: PathBuf,
    pub format: ImageFormat,
    pub files: Vec<ImageFile>,
    pub tracks: Vec<Track>,
    /// Disc-level cue lines (CATALOG, REM, TITLE...) preserved on save.
    pub header: Vec<String>,
    handles: Vec<File>,
}

fn parse_mode(text: &str) -> Result<(Kind, usize)> {
    let t = text.to_ascii_uppercase();
    Ok(match t.as_str() {
        "AUDIO" => (Kind::Audio, 2352),
        "CDG" => (Kind::Audio, 2448),
        "MODE1/2048" => (Kind::Mode1, 2048),
        "MODE1/2352" => (Kind::Mode1, 2352),
        "MODE1/2368" => (Kind::Mode1, 2368),
        "MODE1/2448" => (Kind::Mode1, 2448),
        "MODE2/2336" | "CDI/2336" => (Kind::Mode2, 2336),
        "MODE2/2352" | "CDI/2352" => (Kind::Mode2, 2352),
        "MODE2/2368" => (Kind::Mode2, 2368),
        "MODE2/2448" => (Kind::Mode2, 2448),
        _ => bail!("modo de trilha não suportado: {text}"),
    })
}

fn parse_msf(s: &str) -> Result<u32> {
    let p: Vec<u32> = s.split(':').map(|x| x.trim().parse()).collect::<Result<_, _>>().with_context(|| format!("tempo inválido: {s}"))?;
    match p[..] {
        [m, s, f] => Ok(m * 4500 + s * 75 + f),
        _ => bail!("tempo inválido: {s}"),
    }
}

/// Splits a cue line into keyword and the rest, honouring quoted strings.
fn tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut had = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                had = true;
            }
            c if c.is_whitespace() && !quoted => {
                if had || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    had = false;
                }
            }
            c => cur.push(c),
        }
    }
    if had || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Case-insensitive lookup next to the descriptor; cue sheets from Windows
/// often disagree with the on-disk case of the file name.
fn resolve(dir: &Path, name: &str) -> PathBuf {
    let name = name.replace('\\', "/");
    let direct = dir.join(&name);
    if direct.exists() {
        return direct;
    }
    let base = Path::new(&name).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or(name.clone());
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().eq_ignore_ascii_case(&base) {
                return e.path();
            }
        }
    }
    direct
}

fn wave_data(f: &File) -> Result<(u64, u64)> {
    let mut hdr = [0u8; 12];
    f.read_exact_at(&mut hdr, 0)?;
    if &hdr[..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
        bail!("arquivo WAVE inválido");
    }
    let mut pos = 12u64;
    loop {
        let mut ch = [0u8; 8];
        f.read_exact_at(&mut ch, pos).context("WAVE sem bloco 'data'")?;
        let len = u32::from_le_bytes(ch[4..8].try_into().unwrap()) as u64;
        if &ch[..4] == b"data" {
            return Ok((pos + 8, len));
        }
        pos += 8 + len + (len & 1);
    }
}

impl Image {
    pub fn open(path: &Path) -> Result<Image> {
        let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let mut img = match ext.as_str() {
            "cue" => Self::parse_cue(path)?,
            "ccd" => Self::parse_ccd(path)?,
            _ => {
                // A .bin/.img with a sibling cue or ccd is better opened through it.
                for sib in ["cue", "ccd"] {
                    let p = path.with_extension(sib);
                    if p.exists() && ext != "iso" {
                        return Self::open(&p);
                    }
                }
                Self::parse_raw(path)?
            }
        };
        img.layout()?;
        Ok(img)
    }

    fn parse_cue(path: &Path) -> Result<Image> {
        let text = std::fs::read(path).with_context(|| format!("não foi possível ler {}", path.display()))?;
        let text = String::from_utf8(text.clone()).unwrap_or_else(|_| text.iter().map(|&b| b as char).collect());
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut img = Image { path: path.to_owned(), format: ImageFormat::Cue, files: vec![], tracks: vec![], header: vec![], handles: vec![] };
        for (ln, raw) in text.lines().enumerate() {
            let line = raw.trim().trim_start_matches('\u{feff}');
            let t = tokens(line);
            let Some(kw) = t.first() else { continue };
            let ctx = || format!("{}:{}: {line}", path.display(), ln + 1);
            match kw.to_ascii_uppercase().as_str() {
                "FILE" => {
                    let name = t.get(1).with_context(ctx)?;
                    let ftype = t.get(2).map(|s| s.to_ascii_uppercase()).unwrap_or_else(|| "BINARY".into());
                    img.files.push(ImageFile { path: resolve(dir, name), ftype, data_offset: 0, data_len: 0 });
                }
                "TRACK" => {
                    if img.files.is_empty() {
                        bail!("TRACK antes de FILE ({})", ctx());
                    }
                    let number: u8 = t.get(1).with_context(ctx)?.parse().with_context(ctx)?;
                    let mode_text = t.get(2).with_context(ctx)?.to_ascii_uppercase();
                    let (kind, sector_size) = parse_mode(&mode_text)?;
                    img.tracks.push(Track {
                        number,
                        kind,
                        sector_size,
                        mode_text,
                        file: img.files.len() - 1,
                        indexes: vec![],
                        pregap: 0,
                        postgap: 0,
                        extra: vec![],
                        region_frame: 0,
                        region_byte: 0,
                        region_sectors: 0,
                        region_lba: 0,
                    });
                }
                "INDEX" => {
                    let tr = img.tracks.last_mut().with_context(ctx)?;
                    let n: u8 = t.get(1).with_context(ctx)?.parse().with_context(ctx)?;
                    tr.indexes.push((n, parse_msf(t.get(2).with_context(ctx)?)?));
                }
                "PREGAP" => img.tracks.last_mut().with_context(ctx)?.pregap = parse_msf(t.get(1).with_context(ctx)?)?,
                "POSTGAP" => img.tracks.last_mut().with_context(ctx)?.postgap = parse_msf(t.get(1).with_context(ctx)?)?,
                _ => match img.tracks.last_mut() {
                    Some(tr) => tr.extra.push(line.to_string()),
                    None => img.header.push(line.to_string()),
                },
            }
        }
        if img.tracks.is_empty() {
            bail!("nenhuma trilha encontrada no cue");
        }
        Ok(img)
    }

    fn parse_ccd(path: &Path) -> Result<Image> {
        let text = std::fs::read_to_string(path)?;
        let imgpath = ["img", "IMG", "bin"].iter().map(|e| path.with_extension(e)).find(|p| p.exists()).context("arquivo .img do CloneCD não encontrado")?;
        let mut img = Image { path: path.to_owned(), format: ImageFormat::CloneCd, files: vec![ImageFile { path: imgpath, ftype: "BINARY".into(), data_offset: 0, data_len: 0 }], tracks: vec![], header: vec![], handles: vec![] };
        let mut cur: Option<Track> = None;
        for line in text.lines().map(str::trim) {
            if let Some(n) = line.strip_prefix("[TRACK ").and_then(|s| s.strip_suffix(']')) {
                img.tracks.extend(cur.take());
                cur = Some(Track {
                    number: n.trim().parse()?,
                    kind: Kind::Audio,
                    sector_size: 2352,
                    mode_text: "AUDIO".into(),
                    file: 0,
                    indexes: vec![],
                    pregap: 0,
                    postgap: 0,
                    extra: vec![],
                    region_frame: 0,
                    region_byte: 0,
                    region_sectors: 0,
                    region_lba: 0,
                });
            } else if line.starts_with('[') {
                img.tracks.extend(cur.take());
            } else if let (Some(tr), Some((k, v))) = (cur.as_mut(), line.split_once('=')) {
                let k = k.trim().to_ascii_uppercase();
                let v = v.trim();
                if k == "MODE" {
                    (tr.kind, tr.mode_text) = match v {
                        "0" => (Kind::Audio, "AUDIO".into()),
                        "1" => (Kind::Mode1, "MODE1/2352".into()),
                        _ => (Kind::Mode2, "MODE2/2352".into()),
                    };
                } else if let Some(n) = k.strip_prefix("INDEX ") {
                    tr.indexes.push((n.trim().parse()?, v.parse()?));
                }
            }
        }
        img.tracks.extend(cur);
        if img.tracks.is_empty() {
            bail!("nenhuma trilha encontrada no .ccd");
        }
        Ok(img)
    }

    fn parse_raw(path: &Path) -> Result<Image> {
        let f = File::open(path).with_context(|| format!("não foi possível abrir {}", path.display()))?;
        let len = f.metadata()?.len();
        let mut head = [0u8; RAW];
        let n = f.read_at(&mut head, 0)?;
        let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let mode_text = if n >= 16 && head[..12] == cd::SYNC && len % 2352 == 0 {
            let mut s16 = [0u8; RAW];
            let mode = if f.read_at(&mut s16, 16 * 2352).unwrap_or(0) == RAW { s16[15] } else { head[15] };
            if mode == 1 { "MODE1/2352" } else { "MODE2/2352" }
        } else if ext == "mm2" || (len % 2336 == 0 && len % 2048 != 0) {
            "MODE2/2336"
        } else if len % 2048 == 0 {
            "MODE1/2048"
        } else if len % 2352 == 0 {
            "AUDIO"
        } else {
            bail!("tamanho de imagem não reconhecido ({len} bytes)");
        };
        let (kind, sector_size) = parse_mode(mode_text)?;
        Ok(Image {
            path: path.to_owned(),
            format: ImageFormat::Raw,
            files: vec![ImageFile { path: path.to_owned(), ftype: "BINARY".into(), data_offset: 0, data_len: 0 }],
            tracks: vec![Track {
                number: 1,
                kind,
                sector_size,
                mode_text: mode_text.into(),
                file: 0,
                indexes: vec![(1, 0)],
                pregap: 0,
                postgap: 0,
                extra: vec![],
                region_frame: 0,
                region_byte: 0,
                region_sectors: 0,
                region_lba: 0,
            }],
            header: vec![],
            handles: vec![],
        })
    }

    /// Opens the data files and assigns every track its file region and LBA.
    fn layout(&mut self) -> Result<()> {
        self.handles.clear();
        for file in &mut self.files {
            let h = File::open(&file.path).with_context(|| format!("arquivo da imagem não encontrado: {}", file.path.display()))?;
            if file.ftype == "WAVE" {
                (file.data_offset, file.data_len) = wave_data(&h)?;
            } else {
                file.data_len = h.metadata()?.len();
            }
            self.handles.push(h);
        }
        let mut shift = 0u32;
        let mut file_base = 0u32;
        for fi in 0..self.files.len() {
            let ids: Vec<usize> = (0..self.tracks.len()).filter(|&i| self.tracks[i].file == fi).collect();
            let mut byte = 0u64;
            let mut prev_frame = 0u32;
            let mut prev_size = ids.first().map(|&i| self.tracks[i].sector_size).unwrap_or(RAW);
            for (k, &ti) in ids.iter().enumerate() {
                let tr = &mut self.tracks[ti];
                tr.indexes.sort();
                let first = tr.indexes.first().map(|i| i.1).with_context(|| format!("trilha {} sem INDEX", tr.number))?;
                let frame = if k == 0 { 0 } else { first };
                byte += (frame - prev_frame) as u64 * prev_size as u64;
                shift += tr.pregap;
                tr.region_frame = frame;
                tr.region_byte = byte;
                tr.region_lba = file_base + shift + frame;
                prev_frame = frame;
                prev_size = tr.sector_size;
            }
            let end = self.files[fi].data_len;
            let mut file_sectors = 0;
            for (k, &ti) in ids.iter().enumerate() {
                let next = ids.get(k + 1).map(|&n| self.tracks[n].region_byte).unwrap_or(end);
                let tr = &mut self.tracks[ti];
                if next < tr.region_byte {
                    bail!("índices da trilha {} além do fim do arquivo", tr.number);
                }
                tr.region_sectors = ((next - tr.region_byte) / tr.sector_size as u64) as u32;
                file_sectors = tr.region_frame + tr.region_sectors;
                shift += tr.postgap;
            }
            file_base += file_sectors;
        }
        Ok(())
    }

    pub fn total_sectors(&self) -> u32 {
        self.tracks.last().map(|t| t.end_lba()).unwrap_or(0)
    }

    pub fn track_at(&self, lba: u32) -> Option<usize> {
        self.tracks.iter().position(|t| lba >= t.region_lba && lba < t.end_lba())
    }

    /// Reads sector `i` of a track region as a 2352-byte raw frame, synthesising
    /// sync/header/EDC/ECC for cooked (2048/2336) tracks.
    pub fn read_track_sector(&self, ti: usize, i: u32) -> Result<Sector> {
        let tr = &self.tracks[ti];
        let file = &self.files[tr.file];
        let off = file.data_offset + tr.region_byte + i as u64 * tr.sector_size as u64;
        let lba = tr.region_lba + i;
        let mut buf = [0u8; RAW];
        let n = tr.sector_size.min(RAW);
        self.handles[tr.file].read_exact_at(&mut buf[..n], off).with_context(|| format!("erro de leitura no setor {lba}"))?;
        Ok(match (tr.kind, tr.sector_size) {
            (Kind::Mode1, 2048) => cd::make_mode1(lba, &buf[..2048]),
            (Kind::Mode2, 2336) => cd::make_mode2(lba, &buf[..2336]),
            (Kind::Audio, _) if file.ftype == "MOTOROLA" => {
                for p in buf.chunks_exact_mut(2) {
                    p.swap(0, 1);
                }
                buf
            }
            _ => buf,
        })
    }

    pub fn read_lba(&self, lba: u32) -> Result<Sector> {
        let ti = self.track_at(lba).with_context(|| format!("setor {lba} fora da imagem"))?;
        self.read_track_sector(ti, lba - self.tracks[ti].region_lba)
    }

    /// File location of a sector, for in-place writing. Only raw (>=2352) and
    /// 2048-byte tracks are writable.
    pub fn locate(&self, lba: u32) -> Option<(PathBuf, u64, usize)> {
        let ti = self.track_at(lba)?;
        let tr = &self.tracks[ti];
        let f = &self.files[tr.file];
        Some((f.path.clone(), f.data_offset + tr.region_byte + (lba - tr.region_lba) as u64 * tr.sector_size as u64, tr.sector_size))
    }

    /// Writes a raw sector back to the image (in place), converting for cooked tracks.
    pub fn write_lba(&self, lba: u32, sector: &Sector) -> Result<()> {
        let (path, off, size) = self.locate(lba).context("setor fora da imagem")?;
        let f = std::fs::OpenOptions::new().write(true).open(&path).with_context(|| format!("não foi possível abrir {} para escrita", path.display()))?;
        let ti = self.track_at(lba).unwrap();
        let data: &[u8] = match size {
            2048 => &sector[16..16 + 2048],
            2336 => &sector[16..16 + 2336],
            _ if self.tracks[ti].kind == Kind::Audio && self.files[self.tracks[ti].file].ftype == "MOTOROLA" => bail!("escrita em áudio MOTOROLA não suportada"),
            _ => &sector[..],
        };
        f.write_all_at(data, off)?;
        Ok(())
    }

    pub fn first_data_track(&self) -> Option<usize> {
        self.tracks.iter().position(|t| t.is_data())
    }

    pub fn name(&self) -> String {
        self.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    }
}

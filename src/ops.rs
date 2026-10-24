//! Long-running operations: scan, rebuild, save (merge/split), extraction.
//! All of them report through a shared `Progress` so the GUI can poll it and
//! cancel, and the CLI can print it.

use crate::cd::{self, Form, Problems, RAW, RebuildFields};
use crate::image::Image;
use crate::iso9660::{Entry, Filesystem};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
    pub stage: Mutex<String>,
}

impl Progress {
    pub fn start(&self, stage: &str, total: u64) {
        *self.stage.lock().unwrap() = stage.to_string();
        self.total.store(total, Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
    }

    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }

    pub fn fraction(&self) -> f32 {
        let t = self.total.load(Ordering::Relaxed);
        if t == 0 { 0.0 } else { self.done.load(Ordering::Relaxed) as f32 / t as f32 }
    }

    fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            bail!("operação cancelada");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SectorError {
    pub lba: u32,
    pub track: u8,
    pub form: Form,
    pub problems: Problems,
}

#[derive(Clone, Debug, Default)]
pub struct ScanReport {
    pub scanned: u64,
    pub skipped_tracks: Vec<u8>,
    pub errors: Vec<SectorError>,
}

/// Checks sync, header address, mode, EDC and ECC of every data sector
/// (CDmage "Scan for corruption"). Runs in parallel across CPU cores.
pub fn scan(img: &Image, p: &Progress) -> Result<ScanReport> {
    let mut rep = ScanReport::default();
    let data: Vec<usize> = (0..img.tracks.len()).filter(|&i| img.tracks[i].is_data()).collect();
    let total: u64 = data.iter().map(|&i| img.tracks[i].region_sectors as u64).sum();
    p.start("Verificando setores", total);
    for ti in data {
        let tr = &img.tracks[ti];
        if tr.sector_size < RAW {
            // Cooked tracks carry no EDC/ECC to verify.
            rep.skipped_tracks.push(tr.number);
            p.add(tr.region_sectors as u64);
            continue;
        }
        let chunk = 1024u32;
        let mut errs: Vec<SectorError> = (0..tr.region_sectors.div_ceil(chunk))
            .into_par_iter()
            .map(|c| -> Result<Vec<SectorError>> {
                p.check()?;
                let mut v = Vec::new();
                let end = ((c + 1) * chunk).min(tr.region_sectors);
                for i in c * chunk..end {
                    let s = img.read_track_sector(ti, i)?;
                    let lba = tr.region_lba + i;
                    let (form, problems) = cd::check(&s, Some(lba), tr.mode_byte());
                    if problems.any() {
                        v.push(SectorError { lba, track: tr.number, form, problems });
                    }
                }
                p.add((end - c * chunk) as u64);
                Ok(v)
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        errs.sort_by_key(|e| e.lba);
        rep.errors.extend(errs);
        rep.scanned += tr.region_sectors as u64;
    }
    Ok(rep)
}

/// Rewrites the selected fields of the given sectors in the image files
/// (CDmage "Rebuild sector fields"). Modifies the image in place.
pub fn rebuild_in_place(img: &Image, lbas: &[u32], fields: RebuildFields, p: &Progress) -> Result<u64> {
    p.start("Reconstruindo setores", lbas.len() as u64);
    let mut n = 0;
    for &lba in lbas {
        p.check()?;
        let ti = img.track_at(lba).context("setor fora da imagem")?;
        let tr = &img.tracks[ti];
        if tr.is_data() && tr.sector_size >= RAW {
            let mut s = img.read_lba(lba)?;
            cd::rebuild(&mut s, lba, tr.mode_byte(), fields);
            img.write_lba(lba, &s)?;
            n += 1;
        }
        p.add(1);
    }
    Ok(n)
}

pub fn all_data_lbas(img: &Image) -> Vec<u32> {
    img.tracks.iter().filter(|t| t.is_data() && t.sector_size >= RAW).flat_map(|t| t.region_lba..t.end_lba()).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveMode {
    /// Every track in one .bin plus a .cue (what PSIO, emulators and most burners want).
    Merge,
    /// One .bin per track plus a .cue (Redump layout).
    Split,
}

#[derive(Clone, Debug)]
pub struct SaveOptions {
    pub mode: SaveMode,
    pub out_dir: PathBuf,
    pub name: String,
    /// Naming pattern for split tracks: ?i image name, ?n track number, ?e extension.
    pub pattern: String,
    /// Regenerate sync/header/EDC/ECC of every data sector while copying.
    pub regenerate: bool,
    /// Zero-pad ?n to two digits (Redump does so only for discs with 10+ tracks).
    pub pad: bool,
}

impl SaveOptions {
    pub fn new(img: &Image, out_dir: PathBuf, mode: SaveMode) -> Self {
        Self { mode, out_dir, name: img.name(), pattern: "?i (Track ?n).?e".into(), regenerate: false, pad: img.tracks.len() >= 10 }
    }

    pub fn track_file_name(&self, number: u8, single: bool) -> String {
        if single {
            return format!("{}.bin", self.name);
        }
        self.pattern.replace("?i", &self.name).replace("?n", &if self.pad { format!("{number:02}") } else { number.to_string() }).replace("?e", "bin")
    }
}

/// Copies a track region as raw 2352-byte sectors.
fn write_region(img: &Image, ti: usize, w: &mut dyn Write, regenerate: bool, p: &Progress) -> Result<()> {
    let tr = &img.tracks[ti];
    let f = &img.files[tr.file];
    let fast = tr.sector_size == RAW && f.ftype != "MOTOROLA" && !(regenerate && tr.is_data());
    if fast {
        let fh = File::open(&f.path)?;
        let mut buf = vec![0u8; RAW * 512];
        let mut left = tr.region_sectors as u64 * RAW as u64;
        let mut off = f.data_offset + tr.region_byte;
        while left > 0 {
            p.check()?;
            let n = left.min(buf.len() as u64) as usize;
            std::os::unix::fs::FileExt::read_exact_at(&fh, &mut buf[..n], off)?;
            w.write_all(&buf[..n])?;
            off += n as u64;
            left -= n as u64;
            p.add((n / RAW) as u64);
        }
        return Ok(());
    }
    for i in 0..tr.region_sectors {
        if i % 256 == 0 {
            p.check()?;
        }
        let mut s = img.read_track_sector(ti, i)?;
        if regenerate && tr.is_data() {
            cd::rebuild(&mut s, tr.region_lba + i, tr.mode_byte(), RebuildFields::default());
        }
        w.write_all(&s)?;
        p.add(1);
    }
    Ok(())
}

/// Saves the image as raw BIN/CUE (CDmage "Save As"), merging all tracks into a
/// single BIN or splitting one BIN per track. Cooked tracks (2048/2336) are
/// converted to 2352 raw sectors. Returns the path of the written cue.
pub fn save(img: &Image, o: &SaveOptions, p: &Progress) -> Result<PathBuf> {
    std::fs::create_dir_all(&o.out_dir)?;
    let cue_path = o.out_dir.join(format!("{}.cue", o.name));
    let inputs: Vec<PathBuf> = img.files.iter().map(|f| f.path.canonicalize().unwrap_or(f.path.clone())).collect();
    let single = o.mode == SaveMode::Merge;
    let outs: Vec<PathBuf> = if single { vec![o.out_dir.join(o.track_file_name(0, true))] } else { img.tracks.iter().map(|t| o.out_dir.join(o.track_file_name(t.number, false))).collect() };
    for out in outs.iter().chain([&cue_path]) {
        let c = out.canonicalize().unwrap_or(out.clone());
        if inputs.contains(&c) || c == img.path.canonicalize().unwrap_or_default() {
            bail!("o destino {} é um arquivo da própria imagem de origem; escolha outra pasta ou nome", out.display());
        }
    }
    p.start("Salvando imagem", img.tracks.iter().map(|t| t.region_sectors as u64).sum());

    let mut cue = String::new();
    for h in &img.header {
        cue.push_str(h);
        cue.push('\n');
    }
    let mut writer: Option<BufWriter<File>> = None;
    let mut pos = 0u32; // sectors already written to the current output file
    for (k, tr) in img.tracks.iter().enumerate() {
        if writer.is_none() || !single {
            if let Some(mut w) = writer.take() {
                w.flush()?;
            }
            let out = &outs[if single { 0 } else { k }];
            writer = Some(BufWriter::with_capacity(4 << 20, File::create(out).with_context(|| format!("não foi possível criar {}", out.display()))?));
            pos = 0;
            cue.push_str(&format!("FILE \"{}\" BINARY\n", out.file_name().unwrap().to_string_lossy()));
        }
        cue.push_str(&format!("  TRACK {:02} {}\n", tr.number, tr.raw_mode_text()));
        for x in &tr.extra {
            cue.push_str(&format!("    {x}\n"));
        }
        if tr.pregap > 0 {
            cue.push_str(&format!("    PREGAP {}\n", cd::msf_string(tr.pregap)));
        }
        for &(n, frame) in &tr.indexes {
            cue.push_str(&format!("    INDEX {n:02} {}\n", cd::msf_string(pos + frame - tr.region_frame)));
        }
        if tr.postgap > 0 {
            cue.push_str(&format!("    POSTGAP {}\n", cd::msf_string(tr.postgap)));
        }
        write_region(img, k, writer.as_mut().unwrap(), o.regenerate, p)?;
        pos += tr.region_sectors;
    }
    if let Some(mut w) = writer.take() {
        w.flush()?;
    }
    // CRLF like CDRWin and Redump cue sheets.
    std::fs::write(&cue_path, cue.replace('\n', "\r\n"))?;
    Ok(cue_path)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackFormat {
    /// 2352-byte raw sectors (.bin)
    Raw,
    /// 2048-byte user data (.iso), data tracks only
    Iso,
    /// RIFF WAVE 44.1 kHz stereo, audio tracks only
    Wav,
}

pub fn extract_track(img: &Image, ti: usize, fmt: TrackFormat, out: &Path, p: &Progress) -> Result<()> {
    let tr = &img.tracks[ti];
    let mut w = BufWriter::with_capacity(4 << 20, File::create(out)?);
    p.start(&format!("Extraindo trilha {:02}", tr.number), tr.region_sectors as u64);
    match fmt {
        TrackFormat::Raw => write_region(img, ti, &mut w, false, p)?,
        TrackFormat::Iso => {
            if !tr.is_data() {
                bail!("a trilha {:02} é de áudio; use WAV ou BIN", tr.number);
            }
            // ISO starts at INDEX 01; the pregap is not part of the filesystem.
            let skip = tr.start_lba() - tr.region_lba;
            for i in skip..tr.region_sectors {
                if i % 256 == 0 {
                    p.check()?;
                }
                let s = img.read_track_sector(ti, i)?;
                let (o, _) = match cd::detect_form(&s) {
                    Form::Mode1 => (16, 0),
                    _ => (24, 0),
                };
                w.write_all(&s[o..o + 2048])?;
                p.add(1);
            }
        }
        TrackFormat::Wav => {
            if tr.is_data() {
                bail!("a trilha {:02} é de dados; use ISO ou BIN", tr.number);
            }
            w.write_all(&wav_header(tr.region_sectors * RAW as u32))?;
            write_region(img, ti, &mut w, false, p)?;
        }
    }
    w.flush()?;
    Ok(())
}

/// 44-byte RIFF WAVE header for CD audio (44.1 kHz, 16-bit, stereo).
pub fn wav_header(len: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(len + 36).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&2u16.to_le_bytes()); // stereo
    h.extend_from_slice(&44100u32.to_le_bytes());
    h.extend_from_slice(&(44100u32 * 4).to_le_bytes());
    h.extend_from_slice(&4u16.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&len.to_le_bytes());
    h
}

fn system_time(e: &Entry) -> Option<std::time::SystemTime> {
    let d = e.date;
    let (y, m, day) = (1900 + d[0] as i64, d[1] as i64, d[2] as i64);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let gmt = (d[6] as i8) as i64 * 15 * 60;
    let secs = days * 86400 + d[3] as i64 * 3600 + d[4] as i64 * 60 + d[5] as i64 - gmt;
    (secs >= 0).then(|| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

pub fn clean_name(name: &str) -> String {
    name.split(';').next().unwrap_or(name).trim_end_matches('.').replace(['/', '\\', '\0'], "_")
}

/// Extracts one file or a whole directory tree. Returns warnings.
pub fn extract_entries(img: &Image, fs: &Filesystem, entries: &[&Entry], out_dir: &Path, raw: bool, p: &Progress) -> Result<Vec<String>> {
    fn collect<'a>(e: &'a Entry, dir: PathBuf, v: &mut Vec<(PathBuf, &'a Entry)>) {
        let mut name = clean_name(&e.name);
        if e.is_cdda() {
            name.push_str(".wav");
        }
        let path = dir.join(name);
        if e.is_dir {
            for c in &e.children {
                collect(c, path.clone(), v);
            }
        }
        v.push((path, e));
    }
    let mut list = Vec::new();
    for e in entries {
        if e.name.is_empty() {
            for c in &e.children {
                collect(c, out_dir.to_owned(), &mut list);
            }
        } else {
            collect(e, out_dir.to_owned(), &mut list);
        }
    }
    p.start("Extraindo arquivos", list.iter().filter(|(_, e)| !e.is_dir).map(|(_, e)| e.sectors() as u64).sum());
    let mut warnings = Vec::new();
    for (path, e) in &list {
        p.check()?;
        if e.is_dir {
            std::fs::create_dir_all(path)?;
            continue;
        }
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let f = File::create(path).with_context(|| format!("não foi possível criar {}", path.display()))?;
        let mut w = BufWriter::new(&f);
        match fs.extract_to(img, e, raw, &mut w) {
            Ok(0) => {}
            Ok(n) => warnings.push(format!("{}: {n} setor(es) Form 2 dentro de arquivo de dados", e.name)),
            Err(err) => warnings.push(format!("{}: {err:#}", e.name)),
        }
        w.flush()?;
        drop(w);
        if let Some(t) = system_time(e) {
            let _ = f.set_modified(t);
        }
        if e.is_cdda() {
            warnings.push(format!("{}: arquivo CD-DA salvo como WAV", e.name));
        } else if e.is_stream() || raw {
            warnings.push(format!("{}: salvo como RIFF CDXA (setores brutos de 2352 bytes)", e.name));
        }
        p.add(e.sectors() as u64);
    }
    Ok(warnings)
}

/// Saves the 16-sector system area (PlayStation license and logo data).
pub fn save_boot(img: &Image, out: &Path) -> Result<()> {
    let ti = img.first_data_track().context("a imagem não tem trilha de dados")?;
    let base = img.tracks[ti].start_lba();
    let mut w = BufWriter::new(File::create(out)?);
    for i in 0..16 {
        let s = img.read_lba(base + i)?;
        w.write_all(crate::iso9660::user_data(&s))?;
    }
    w.flush()?;
    Ok(())
}

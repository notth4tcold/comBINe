//! ISO9660 filesystem reader with CD-ROM XA attributes (used by PlayStation discs).

use crate::cd::{self, Form, Sector};
use crate::image::Image;
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::io::Write;

pub const XA_FORM1: u16 = 0x0800;
pub const XA_FORM2: u16 = 0x1000;
pub const XA_INTERLEAVED: u16 = 0x2000;
pub const XA_CDDA: u16 = 0x4000;

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub lba: u32,
    pub size: u32,
    pub is_dir: bool,
    pub xa: Option<u16>,
    /// Recording date: years since 1900, month, day, hour, minute, second, GMT offset.
    pub date: [u8; 7],
    /// Where this entry's directory record lives (for size updates on import).
    pub record_lba: u32,
    pub record_offset: usize,
    pub children: Vec<Entry>,
}

impl Entry {
    pub fn sectors(&self) -> u32 {
        self.size.div_ceil(2048)
    }

    /// Files stored as Form 2 / interleaved streams (STR, XA audio) carry
    /// 2336 bytes per sector, so they are extracted raw instead of as 2048-byte data.
    pub fn is_stream(&self) -> bool {
        self.xa.is_some_and(|a| a & (XA_FORM2 | XA_INTERLEAVED) != 0)
    }

    pub fn is_cdda(&self) -> bool {
        self.xa.is_some_and(|a| a & XA_CDDA != 0)
    }

    pub fn date_string(&self) -> String {
        let d = self.date;
        format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", 1900 + d[0] as u32, d[1], d[2], d[3], d[4], d[5])
    }

    pub fn kind_string(&self) -> &'static str {
        if self.is_dir {
            "Pasta"
        } else if self.is_cdda() {
            "CD-DA"
        } else if self.xa.is_some_and(|a| a & XA_INTERLEAVED != 0) {
            "Mode2 Form2 (intercalado)"
        } else if self.xa.is_some_and(|a| a & XA_FORM2 != 0) {
            "Mode2 Form2"
        } else {
            "Dados"
        }
    }

    /// Depth-first walk yielding (path, entry).
    pub fn walk<'a>(&'a self, prefix: &str, out: &mut Vec<(String, &'a Entry)>) {
        for c in &self.children {
            let p = if prefix.is_empty() { c.name.clone() } else { format!("{prefix}/{}", c.name) };
            out.push((p.clone(), c));
            if c.is_dir {
                c.walk(&p, out);
            }
        }
    }

    pub fn find(&self, path: &str) -> Option<&Entry> {
        let mut cur = self;
        for part in path.split(['/', '\\']).filter(|s| !s.is_empty()) {
            cur = cur.children.iter().find(|c| c.name.eq_ignore_ascii_case(part) || c.name.split(';').next().is_some_and(|n| n.eq_ignore_ascii_case(part)))?;
        }
        Some(cur)
    }
}

#[derive(Clone, Debug, Default)]
pub struct VolumeInfo {
    pub system_id: String,
    pub volume_id: String,
    pub volume_set_id: String,
    pub publisher_id: String,
    pub preparer_id: String,
    pub application_id: String,
    pub volume_sectors: u32,
    pub creation_date: String,
    /// PlayStation boot executable from SYSTEM.CNF (e.g. SLUS_005.94).
    pub boot: Option<String>,
    /// PlayStation license text from sector 4 (region hint).
    pub license: Option<String>,
}

pub struct Filesystem {
    pub info: VolumeInfo,
    pub root: Entry,
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim_end_matches([' ', '\0']).to_string()
}

/// User data area (2048 bytes) of a raw data sector, whatever its mode.
pub fn user_data(s: &Sector) -> &[u8] {
    match cd::detect_form(s) {
        Form::Mode1 => &s[16..16 + 2048],
        _ => &s[24..24 + 2048],
    }
}

fn user_offset(s: &Sector) -> usize {
    if cd::detect_form(s) == Form::Mode1 { 16 } else { 24 }
}

/// Reads the PlayStation license banner (sector 4), whose text is laid out in
/// fixed-width lines that split words, and names the region it implies.
fn license_region(data: &[u8]) -> Option<String> {
    let squeezed: String = String::from_utf8_lossy(&data[..256.min(data.len())]).chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.').collect();
    if !squeezed.contains("SonyComputerEntertainment") {
        return None;
    }
    Some(
        if squeezed.contains("Amer") {
            "Sony Computer Entertainment America (NTSC-U)"
        } else if squeezed.contains("Euro") {
            "Sony Computer Entertainment Europe (PAL)"
        } else {
            "Sony Computer Entertainment Inc. (NTSC-J)"
        }
        .to_string(),
    )
}

fn parse_records(img: &Image, lba: u32, size: u32, seen: &mut HashSet<u32>, depth: usize) -> Result<Vec<Entry>> {
    if depth > 32 || !seen.insert(lba) {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for i in 0..size.div_ceil(2048) {
        let sec = img.read_lba(lba + i)?;
        let data = user_data(&sec);
        let mut pos = 0usize;
        while pos < 2048 {
            let len = data[pos] as usize;
            if len == 0 {
                break;
            }
            if pos + len > 2048 || len < 34 {
                break;
            }
            let r = &data[pos..pos + len];
            let name_len = r[32] as usize;
            let raw_name = &r[33..33 + name_len.min(len - 33)];
            let flags = r[25];
            if !(name_len == 1 && (raw_name[0] == 0 || raw_name[0] == 1)) {
                let mut su = 33 + name_len;
                if su % 2 == 1 {
                    su += 1;
                }
                let xa = (su + 14 <= len && &r[su + 6..su + 8] == b"XA").then(|| u16::from_be_bytes([r[su + 4], r[su + 5]]));
                let mut e = Entry {
                    name: String::from_utf8_lossy(raw_name).to_string(),
                    lba: u32::from_le_bytes(r[2..6].try_into().unwrap()),
                    size: u32::from_le_bytes(r[10..14].try_into().unwrap()),
                    is_dir: flags & 0x02 != 0,
                    xa,
                    date: r[18..25].try_into().unwrap(),
                    record_lba: lba + i,
                    record_offset: pos,
                    children: vec![],
                };
                if e.is_dir {
                    e.children = parse_records(img, e.lba, e.size, seen, depth + 1).unwrap_or_default();
                }
                out.push(e);
            }
            pos += len;
        }
    }
    Ok(out)
}

impl Filesystem {
    pub fn read(img: &Image) -> Result<Filesystem> {
        let ti = img.first_data_track().context("a imagem não tem trilha de dados")?;
        let base = img.tracks[ti].start_lba();
        let mut pvd_lba = None;
        for i in 16..32 {
            let s = img.read_lba(base + i)?;
            let d = user_data(&s);
            if &d[1..6] != b"CD001" {
                break;
            }
            match d[0] {
                1 => {
                    pvd_lba = Some(base + i);
                    break;
                }
                255 => break,
                _ => {}
            }
        }
        let pvd_lba = pvd_lba.context("sistema de arquivos ISO9660 não encontrado")?;
        let s = img.read_lba(pvd_lba)?;
        let d = user_data(&s);
        let info = VolumeInfo {
            system_id: text(&d[8..40]),
            volume_id: text(&d[40..72]),
            volume_sectors: u32::from_le_bytes(d[80..84].try_into().unwrap()),
            volume_set_id: text(&d[190..318]),
            publisher_id: text(&d[318..446]),
            preparer_id: text(&d[446..574]),
            application_id: text(&d[574..702]),
            creation_date: {
                let c = text(&d[813..827]);
                if c.len() == 14 { format!("{}-{}-{} {}:{}:{}", &c[0..4], &c[4..6], &c[6..8], &c[8..10], &c[10..12], &c[12..14]) } else { c }
            },
            boot: None,
            license: None,
        };
        let r = &d[156..190];
        let root_lba = u32::from_le_bytes(r[2..6].try_into().unwrap());
        let root_size = u32::from_le_bytes(r[10..14].try_into().unwrap());
        let mut seen = HashSet::new();
        let root = Entry {
            name: String::new(),
            lba: root_lba,
            size: root_size,
            is_dir: true,
            xa: None,
            date: r[18..25].try_into().unwrap(),
            record_lba: pvd_lba,
            record_offset: 156,
            children: parse_records(img, root_lba, root_size, &mut seen, 0)?,
        };
        let mut fs = Filesystem { info, root };
        fs.info.license = img.read_lba(base + 4).ok().and_then(|s| license_region(user_data(&s)));
        if let Some(cnf) = fs.root.find("SYSTEM.CNF").cloned() {
            if let Ok(data) = fs.read_file(img, &cnf) {
                let t = String::from_utf8_lossy(&data);
                fs.info.boot = t.lines().find_map(|l| {
                    let (k, v) = l.split_once('=')?;
                    (k.trim().eq_ignore_ascii_case("BOOT")).then(|| v.trim().rsplit(['\\', ':']).next().unwrap_or("").split(';').next().unwrap_or("").to_string())
                });
            }
        }
        Ok(fs)
    }

    /// Reads a regular (Form 1) file's contents.
    pub fn read_file(&self, img: &Image, e: &Entry) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(e.size as usize);
        self.extract_to(img, e, false, &mut out)?;
        Ok(out)
    }

    /// Writes a file's contents. Stream files (or `raw`) are written as a RIFF
    /// CDXA container with full 2352-byte sectors, like CDmage does, so XA/STR
    /// tools can read them; everything else is written as plain user data.
    /// Returns the number of sectors that were not Form 1 inside a data file.
    pub fn extract_to(&self, img: &Image, e: &Entry, raw: bool, w: &mut dyn Write) -> Result<u32> {
        if e.is_dir {
            bail!("{} é uma pasta", e.name);
        }
        let sectors = e.sectors();
        let mut odd = 0;
        if e.is_cdda() {
            // The entry points at an audio track; its size counts 2048-byte units.
            let data_len = sectors * 2352;
            w.write_all(&crate::ops::wav_header(data_len))?;
            for i in 0..sectors {
                w.write_all(&img.read_lba(e.lba + i)?)?;
            }
        } else if raw || e.is_stream() {
            let data_len = sectors * 2352;
            let mut hdr = Vec::with_capacity(44);
            hdr.extend_from_slice(b"RIFF");
            hdr.extend_from_slice(&(data_len + 36).to_le_bytes());
            hdr.extend_from_slice(b"CDXAfmt ");
            hdr.extend_from_slice(&16u32.to_le_bytes());
            // XA system-use block as stored in the directory record.
            hdr.extend_from_slice(&[0, 0, 0, 0]);
            hdr.extend_from_slice(&e.xa.unwrap_or(XA_FORM1).to_be_bytes());
            hdr.extend_from_slice(b"XA");
            hdr.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
            hdr.extend_from_slice(b"data");
            hdr.extend_from_slice(&data_len.to_le_bytes());
            w.write_all(&hdr)?;
            for i in 0..sectors {
                w.write_all(&img.read_lba(e.lba + i)?)?;
            }
        } else {
            let mut left = e.size as usize;
            for i in 0..sectors {
                let s = img.read_lba(e.lba + i)?;
                if cd::detect_form(&s) == Form::Mode2Form2 {
                    odd += 1;
                }
                let d = user_data(&s);
                let n = left.min(2048);
                w.write_all(&d[..n])?;
                left -= n;
            }
        }
        Ok(odd)
    }

    /// Replaces a file's contents in place (CDmage "Import file"). The new data
    /// must fit in the sectors already allocated to the file. Subheaders are
    /// kept, EDC/ECC are regenerated and the directory record size is updated.
    pub fn import(&self, img: &Image, e: &Entry, data: &[u8]) -> Result<()> {
        if e.is_dir || e.is_stream() {
            bail!("só é possível importar sobre arquivos de dados comuns (Form 1)");
        }
        let cap = e.sectors() as usize * 2048;
        if data.len() > cap {
            bail!("o novo arquivo tem {} bytes, mas só cabem {} bytes ({} setores) no espaço do original", data.len(), cap, e.sectors());
        }
        for i in 0..e.sectors() {
            let lba = e.lba + i;
            let mut s = img.read_lba(lba)?;
            let off = user_offset(&s);
            let chunk = data.get(i as usize * 2048..).unwrap_or(&[]);
            let n = chunk.len().min(2048);
            s[off..off + n].copy_from_slice(&chunk[..n]);
            s[off + n..off + 2048].fill(0);
            cd::rebuild(&mut s, lba, None, cd::RebuildFields { sync: false, address: false, subheader: false, edc: true, ecc: true });
            img.write_lba(lba, &s)?;
        }
        let mut s = img.read_lba(e.record_lba)?;
        let off = user_offset(&s) + e.record_offset;
        let size = data.len() as u32;
        s[off + 10..off + 14].copy_from_slice(&size.to_le_bytes());
        s[off + 14..off + 18].copy_from_slice(&size.to_be_bytes());
        cd::rebuild(&mut s, e.record_lba, None, cd::RebuildFields { sync: false, address: false, subheader: false, edc: true, ecc: true });
        img.write_lba(e.record_lba, &s)?;
        Ok(())
    }
}

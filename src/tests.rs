//! End-to-end tests on a synthetic PlayStation-style disc: a Mode 2 XA data
//! track with an ISO9660 filesystem in one BIN, and an audio track with a
//! two-second INDEX 00 pregap in a second BIN.

use crate::cd::{self, RAW};
use crate::image::Image;
use crate::iso9660::Filesystem;
use crate::ops::{self, Progress, SaveMode, SaveOptions, TrackFormat};
use std::path::{Path, PathBuf};

const DATA_SECTORS: u32 = 40;
const AUDIO_SECTORS: u32 = 300;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("combine-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn both16(v: u16) -> [u8; 4] {
    let mut b = [0; 4];
    b[..2].copy_from_slice(&v.to_le_bytes());
    b[2..].copy_from_slice(&v.to_be_bytes());
    b
}

fn both32(v: u32) -> [u8; 8] {
    let mut b = [0; 8];
    b[..4].copy_from_slice(&v.to_le_bytes());
    b[4..].copy_from_slice(&v.to_be_bytes());
    b
}

fn record(name: &[u8], lba: u32, size: u32, dir: bool, xa_attr: u16) -> Vec<u8> {
    let mut r = vec![0u8; 33];
    r[2..10].copy_from_slice(&both32(lba));
    r[10..18].copy_from_slice(&both32(size));
    r[18..25].copy_from_slice(&[97, 1, 2, 3, 4, 5, 0]);
    r[25] = if dir { 2 } else { 0 };
    r[28..32].copy_from_slice(&both16(1));
    r[32] = name.len() as u8;
    r.extend_from_slice(name);
    if r.len() % 2 == 1 {
        r.push(0);
    }
    r.extend_from_slice(&[0, 0, 0, 0]);
    r.extend_from_slice(&xa_attr.to_be_bytes());
    r.extend_from_slice(b"XA");
    r.extend_from_slice(&[0; 6]);
    r[0] = r.len() as u8;
    r
}

pub fn file_data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

/// Builds the fixture and returns the cue path.
fn make_disc(dir: &Path) -> PathBuf {
    let cnf = b"BOOT = cdrom:\\SLUS_123.45;1\r\nTCB = 4\r\n".to_vec();
    let data = file_data(5000);
    let mut user: Vec<Vec<u8>> = vec![vec![0u8; 2048]; DATA_SECTORS as usize];
    user[4][..32].copy_from_slice(b"          Licensed  by          ");
    user[4][32..64].copy_from_slice(b"Sony Computer Entertainment Amer");
    // PVD
    let p = &mut user[16];
    p[0] = 1;
    p[1..6].copy_from_slice(b"CD001");
    p[6] = 1;
    p[8..40].copy_from_slice(b"PLAYSTATION                     ");
    p[40..72].copy_from_slice(b"TESTDISC                        ");
    p[80..88].copy_from_slice(&both32(DATA_SECTORS));
    let root = record(&[0], 18, 2048, true, 0x8d55);
    p[156..156 + 34].copy_from_slice(&root[..34]);
    p[813..827].copy_from_slice(b"19970102030405");
    // Terminator
    user[17][0] = 255;
    user[17][1..6].copy_from_slice(b"CD001");
    // Root directory
    let mut d = Vec::new();
    d.extend(record(&[0], 18, 2048, true, 0x8d55));
    d.extend(record(&[1], 18, 2048, true, 0x8d55));
    d.extend(record(b"DATA.BIN;1", 20, data.len() as u32, false, 0x0d55));
    d.extend(record(b"MOVIE.STR;1", 24, 4 * 2048, false, 0x2555 | 0x1000));
    d.extend(record(b"SYSTEM.CNF;1", 19, cnf.len() as u32, false, 0x0d55));
    user[18][..d.len()].copy_from_slice(&d);
    user[19][..cnf.len()].copy_from_slice(&cnf);
    for (i, c) in data.chunks(2048).enumerate() {
        user[20 + i][..c.len()].copy_from_slice(c);
    }
    let mut bin = Vec::new();
    for (lba, u) in user.iter().enumerate() {
        let lba = lba as u32;
        let s = if (24..28).contains(&lba) {
            // Interleaved Form 2 stream sectors
            let mut d2 = vec![0u8; 2336];
            d2[..4].copy_from_slice(&[1, 1, 0x64, 0x01]);
            d2[4..8].copy_from_slice(&[1, 1, 0x64, 0x01]);
            for (j, b) in d2[8..2332].iter_mut().enumerate() {
                *b = (j as u32 ^ lba) as u8;
            }
            let mut s = cd::make_mode2(lba, &d2);
            let e = cd::edc(&s[0x10..0x92C]);
            s[0x92C..0x930].copy_from_slice(&e.to_le_bytes());
            s
        } else {
            let sub = if lba == 16 || lba == 17 || lba == 18 { [0, 0, 0x89, 0] } else { [0, 0, 0x08, 0] };
            cd::make_mode2_form1(lba, sub, u)
        };
        bin.extend_from_slice(&s);
    }
    std::fs::write(dir.join("Test Game (Track 1).bin"), &bin).unwrap();
    let audio: Vec<u8> = (0..AUDIO_SECTORS as usize * RAW).map(|i| (i % 253) as u8).collect();
    std::fs::write(dir.join("Test Game (Track 2).bin"), &audio).unwrap();
    let cue = "FILE \"Test Game (Track 1).bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\nFILE \"test game (track 2).BIN\" BINARY\n  TRACK 02 AUDIO\n    INDEX 00 00:00:00\n    INDEX 01 00:02:00\n";
    let p = dir.join("Test Game.cue");
    std::fs::write(&p, cue).unwrap();
    p
}

fn all_sectors(img: &Image) -> Vec<u8> {
    (0..img.total_sectors()).flat_map(|l| img.read_lba(l).unwrap()).collect()
}

#[test]
fn open_and_read_filesystem() {
    let dir = tmpdir("open");
    let img = Image::open(&make_disc(&dir)).unwrap();
    assert_eq!(img.tracks.len(), 2);
    assert_eq!(img.tracks[1].region_lba, DATA_SECTORS);
    assert_eq!(img.tracks[1].start_lba(), DATA_SECTORS + 150);
    assert_eq!(img.total_sectors(), DATA_SECTORS + AUDIO_SECTORS);
    let fs = Filesystem::read(&img).unwrap();
    assert_eq!(fs.info.volume_id, "TESTDISC");
    assert_eq!(fs.info.boot.as_deref(), Some("SLUS_123.45"));
    assert!(fs.info.license.as_deref().unwrap().contains("NTSC-U"));
    let e = fs.root.find("DATA.BIN").unwrap();
    assert_eq!(fs.read_file(&img, e).unwrap(), file_data(5000));
    assert!(fs.root.find("MOVIE.STR").unwrap().is_stream());
}

#[test]
fn scan_detects_and_rebuild_fixes() {
    let dir = tmpdir("scan");
    let cue = make_disc(&dir);
    let p = Progress::default();
    let img = Image::open(&cue).unwrap();
    assert!(ops::scan(&img, &p).unwrap().errors.is_empty());
    // Corrupt a data byte in sector 21 and the header of sector 30.
    let bin = dir.join("Test Game (Track 1).bin");
    let mut b = std::fs::read(&bin).unwrap();
    b[21 * RAW + 500] ^= 0xFF;
    b[30 * RAW + 13] = 0x99;
    std::fs::write(&bin, &b).unwrap();
    let img = Image::open(&cue).unwrap();
    let rep = ops::scan(&img, &p).unwrap();
    let lbas: Vec<u32> = rep.errors.iter().map(|e| e.lba).collect();
    assert_eq!(lbas, vec![21, 30]);
    assert!(rep.errors[0].problems.edc && rep.errors[0].problems.ecc_p);
    assert!(rep.errors[1].problems.address);
    // Merge with --fix produces a clean copy without touching the original.
    let mut o = SaveOptions::new(&img, dir.join("fixed"), SaveMode::Merge);
    o.regenerate = true;
    let fixed = Image::open(&ops::save(&img, &o, &p).unwrap()).unwrap();
    assert!(ops::scan(&fixed, &p).unwrap().errors.is_empty());
    assert_eq!(ops::scan(&img, &p).unwrap().errors.len(), 2);
    // In-place rebuild.
    ops::rebuild_in_place(&img, &lbas, cd::RebuildFields::default(), &p).unwrap();
    let img = Image::open(&cue).unwrap();
    assert!(ops::scan(&img, &p).unwrap().errors.is_empty());
}

#[test]
fn merge_and_split_preserve_layout() {
    let dir = tmpdir("merge");
    let cue = make_disc(&dir);
    let p = Progress::default();
    let img = Image::open(&cue).unwrap();
    let original = all_sectors(&img);

    let o = SaveOptions::new(&img, dir.join("merged"), SaveMode::Merge);
    let mcue = ops::save(&img, &o, &p).unwrap();
    let text = std::fs::read_to_string(&mcue).unwrap().replace("\r\n", "\n");
    assert_eq!(text, "FILE \"Test Game.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 00 00:00:40\n    INDEX 01 00:02:40\n");
    let merged = Image::open(&mcue).unwrap();
    assert_eq!(merged.files.len(), 1);
    assert_eq!(all_sectors(&merged), original);
    assert_eq!(merged.tracks[1].start_lba(), img.tracks[1].start_lba());

    let o = SaveOptions::new(&merged, dir.join("split"), SaveMode::Split);
    let scue = ops::save(&merged, &o, &p).unwrap();
    let split = Image::open(&scue).unwrap();
    assert_eq!(split.files.len(), 2);
    assert!(split.files[1].path.ends_with("Test Game (Track 2).bin"));
    assert_eq!(all_sectors(&split), original);
    assert_eq!(std::fs::read(&split.files[1].path).unwrap(), std::fs::read(dir.join("Test Game (Track 2).bin")).unwrap());

    // Refuses to overwrite its own source.
    let o = SaveOptions::new(&img, dir.clone(), SaveMode::Split);
    let mut o2 = o.clone();
    o2.pattern = "?i (Track ?n).?e".into();
    o2.name = "Test Game".into();
    assert!(ops::save(&img, &o2, &p).is_err());
}

#[test]
fn iso_roundtrip_and_cooked_conversion() {
    let dir = tmpdir("iso");
    let cue = make_disc(&dir);
    let p = Progress::default();
    let img = Image::open(&cue).unwrap();
    let iso = dir.join("t1.iso");
    ops::extract_track(&img, 0, TrackFormat::Iso, &iso, &p).unwrap();
    assert_eq!(std::fs::metadata(&iso).unwrap().len(), DATA_SECTORS as u64 * 2048);
    let i2 = Image::open(&iso).unwrap();
    assert_eq!(i2.tracks[0].mode_text, "MODE1/2048");
    let fs = Filesystem::read(&i2).unwrap();
    assert_eq!(fs.read_file(&i2, fs.root.find("DATA.BIN").unwrap()).unwrap(), file_data(5000));
    // Saving a cooked ISO produces valid raw Mode 1 sectors.
    let raw = Image::open(&ops::save(&i2, &SaveOptions::new(&i2, dir.join("raw"), SaveMode::Merge), &p).unwrap()).unwrap();
    assert_eq!(raw.tracks[0].mode_text, "MODE1/2352");
    assert!(ops::scan(&raw, &p).unwrap().errors.is_empty());
    let wav = dir.join("t2.wav");
    ops::extract_track(&img, 1, TrackFormat::Wav, &wav, &p).unwrap();
    assert_eq!(std::fs::metadata(&wav).unwrap().len(), 44 + AUDIO_SECTORS as u64 * RAW as u64);
}

#[test]
fn extract_and_import() {
    let dir = tmpdir("extract");
    let cue = make_disc(&dir);
    let p = Progress::default();
    let img = Image::open(&cue).unwrap();
    let fs = Filesystem::read(&img).unwrap();
    let out = dir.join("out");
    let warnings = ops::extract_entries(&img, &fs, &[&fs.root], &out, false, &p).unwrap();
    assert_eq!(std::fs::read(out.join("DATA.BIN")).unwrap(), file_data(5000));
    let str_file = std::fs::read(out.join("MOVIE.STR")).unwrap();
    assert_eq!(&str_file[..4], b"RIFF");
    assert_eq!(&str_file[8..12], b"CDXA");
    assert_eq!(str_file.len(), 44 + 4 * RAW);
    assert!(warnings.iter().any(|w| w.contains("MOVIE.STR")));

    let new_data = b"hello from comBINe".repeat(300);
    fs.import(&img, fs.root.find("DATA.BIN").unwrap(), &new_data).unwrap();
    let img = Image::open(&cue).unwrap();
    let fs = Filesystem::read(&img).unwrap();
    let e = fs.root.find("DATA.BIN").unwrap();
    assert_eq!(e.size as usize, new_data.len());
    assert_eq!(fs.read_file(&img, e).unwrap(), new_data);
    assert!(ops::scan(&img, &p).unwrap().errors.is_empty());
    assert!(fs.import(&img, e, &vec![0u8; 3 * 2048 + 1]).is_err());
}

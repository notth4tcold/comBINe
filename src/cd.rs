//! Raw CD sector primitives: layout constants, MSF addressing, EDC and ECC.
//!
//! The EDC is the ECMA-130 CRC (CDmage builds it from polynomial 0x8001801B;
//! the reflected form used here, 0xD8018001, produces the same values). The ECC
//! is the Reed-Solomon product code over GF(2^8) with P (86x24) and Q (52x43)
//! parity, as in ECMA-130 Annex A.

use std::sync::OnceLock;

pub const RAW: usize = 2352;
pub const SYNC: [u8; 12] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
/// Two seconds of lead-in: LBA 0 is MSF 00:02:00.
pub const LBA_OFFSET: u32 = 150;

pub type Sector = [u8; RAW];

struct Tables {
    edc: [u32; 256],
    ecc_f: [u8; 256],
    ecc_b: [u8; 256],
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = Tables { edc: [0; 256], ecc_f: [0; 256], ecc_b: [0; 256] };
        for i in 0..256u32 {
            let mut e = i;
            for _ in 0..8 {
                e = (e >> 1) ^ if e & 1 != 0 { 0xD801_8001 } else { 0 };
            }
            t.edc[i as usize] = e;
            let j = ((i << 1) ^ if i & 0x80 != 0 { 0x11D } else { 0 }) as u8;
            t.ecc_f[i as usize] = j;
            t.ecc_b[(i as u8 ^ j) as usize] = i as u8;
        }
        t
    })
}

pub fn edc(data: &[u8]) -> u32 {
    let t = tables();
    data.iter().fold(0u32, |e, &b| (e >> 8) ^ t.edc[((e ^ b as u32) & 0xFF) as usize])
}

/// Computes P (172 bytes at 0x81C) and Q (104 bytes at 0x8C8) parity.
/// When `zero_address` is set (Mode 2) the header is treated as zero.
pub fn ecc_compute(sector: &Sector, zero_address: bool) -> ([u8; 172], [u8; 104]) {
    let t = tables();
    let mut s = *sector;
    if zero_address {
        s[12..16].fill(0);
    }
    let mut p = [0u8; 172];
    let mut q = [0u8; 104];
    let block = |s: &Sector, major_count: usize, minor_count: usize, major_mult: usize, minor_inc: usize, dest: &mut [u8]| {
        let src = &s[0x0C..];
        let size = major_count * minor_count;
        for major in 0..major_count {
            let mut index = (major >> 1) * major_mult + (major & 1);
            let (mut a, mut b) = (0u8, 0u8);
            for _ in 0..minor_count {
                let v = src[index];
                index += minor_inc;
                if index >= size {
                    index -= size;
                }
                a ^= v;
                b ^= v;
                a = t.ecc_f[a as usize];
            }
            a = t.ecc_b[(t.ecc_f[a as usize] ^ b) as usize];
            dest[major] = a;
            dest[major + major_count] = a ^ b;
        }
    };
    block(&s, 86, 24, 2, 86, &mut p);
    // Q parity covers the P parity bytes, so P must be in place first.
    s[0x81C..0x8C8].copy_from_slice(&p);
    block(&s, 52, 43, 86, 88, &mut q);
    (p, q)
}

pub fn bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

/// Frames (sectors) to (minutes, seconds, frames).
pub fn msf(frames: u32) -> (u32, u32, u32) {
    (frames / 4500, (frames / 75) % 60, frames % 75)
}

pub fn msf_string(frames: u32) -> String {
    let (m, s, f) = msf(frames);
    format!("{m:02}:{s:02}:{f:02}")
}

pub fn header_address(lba: u32) -> [u8; 3] {
    let (m, s, f) = msf(lba + LBA_OFFSET);
    [bcd(m as u8), bcd(s as u8), bcd(f as u8)]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    Mode0,
    Mode1,
    Mode2Form1,
    Mode2Form2,
    /// Mode 2 without a valid subheader ("formless", 2336 user bytes).
    Mode2Formless,
    Unknown,
}

impl Form {
    pub fn name(self) -> &'static str {
        match self {
            Form::Mode0 => "Mode 0",
            Form::Mode1 => "Mode 1",
            Form::Mode2Form1 => "Mode 2 Form 1",
            Form::Mode2Form2 => "Mode 2 Form 2",
            Form::Mode2Formless => "Mode 2",
            Form::Unknown => "Desconhecido",
        }
    }
}

pub fn detect_form(sector: &Sector) -> Form {
    match sector[15] {
        0 => Form::Mode0,
        1 => Form::Mode1,
        2 => {
            let sh = &sector[16..20];
            if sh != &sector[20..24] {
                Form::Mode2Formless
            } else if sh[2] & 0x20 != 0 {
                Form::Mode2Form2
            } else {
                Form::Mode2Form1
            }
        }
        _ => Form::Unknown,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Problems {
    pub sync: bool,
    pub address: bool,
    pub mode: bool,
    pub subheader: bool,
    pub edc: bool,
    pub ecc_p: bool,
    pub ecc_q: bool,
    pub zero_fill: bool,
}

impl Problems {
    pub fn any(&self) -> bool {
        self.sync || self.address || self.mode || self.subheader || self.edc || self.ecc_p || self.ecc_q || self.zero_fill
    }

    pub fn describe(&self) -> String {
        let mut v = Vec::new();
        for (bad, name) in [
            (self.sync, "Sync"),
            (self.address, "Endereço (MSF)"),
            (self.mode, "Modo"),
            (self.subheader, "Subheader"),
            (self.edc, "EDC"),
            (self.ecc_p, "ECC P"),
            (self.ecc_q, "ECC Q"),
            (self.zero_fill, "Área intermediária"),
        ] {
            if bad {
                v.push(name);
            }
        }
        v.join(", ")
    }
}

/// Verifies all redundancy fields of a data sector. `expected_lba` enables the
/// header address check; `expect_mode` flags sectors whose mode differs from the track.
pub fn check(sector: &Sector, expected_lba: Option<u32>, expect_mode: Option<u8>) -> (Form, Problems) {
    let mut p = Problems { sync: sector[..12] != SYNC, ..Default::default() };
    if let Some(lba) = expected_lba {
        p.address = sector[12..15] != header_address(lba);
    }
    if let Some(m) = expect_mode {
        p.mode = sector[15] != m;
    }
    let form = detect_form(sector);
    match form {
        Form::Mode1 => {
            p.edc = edc(&sector[..0x810]).to_le_bytes() != sector[0x810..0x814];
            p.zero_fill = sector[0x814..0x81C].iter().any(|&b| b != 0);
            let (pp, qq) = ecc_compute(sector, false);
            p.ecc_p = pp[..] != sector[0x81C..0x8C8];
            p.ecc_q = qq[..] != sector[0x8C8..0x930];
        }
        Form::Mode2Form1 => {
            p.edc = edc(&sector[0x10..0x818]).to_le_bytes() != sector[0x818..0x81C];
            let (pp, qq) = ecc_compute(sector, true);
            p.ecc_p = pp[..] != sector[0x81C..0x8C8];
            p.ecc_q = qq[..] != sector[0x8C8..0x930];
        }
        Form::Mode2Form2 => {
            // The Form 2 EDC is optional; zero means "not computed".
            let stored = &sector[0x92C..0x930];
            p.edc = stored != [0; 4] && edc(&sector[0x10..0x92C]).to_le_bytes() != stored;
        }
        Form::Mode2Formless => p.subheader = true,
        Form::Mode0 => p.zero_fill = sector[16..].iter().any(|&b| b != 0),
        Form::Unknown => p.mode = true,
    }
    (form, p)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RebuildFields {
    pub sync: bool,
    pub address: bool,
    pub subheader: bool,
    pub edc: bool,
    pub ecc: bool,
}

impl Default for RebuildFields {
    fn default() -> Self {
        Self { sync: true, address: true, subheader: false, edc: true, ecc: true }
    }
}

/// Regenerates the selected fields of a data sector in place. `mode` forces
/// the header mode byte (1 or 2); `None` keeps the stored one.
pub fn rebuild(sector: &mut Sector, lba: u32, mode: Option<u8>, f: RebuildFields) {
    if f.sync {
        sector[..12].copy_from_slice(&SYNC);
    }
    if f.address {
        sector[12..15].copy_from_slice(&header_address(lba));
    }
    if let Some(m) = mode {
        sector[15] = m;
    }
    if f.subheader && sector[15] == 2 && sector[16..20] != sector[20..24] {
        // Trust the first copy unless it looks implausible and the second does not.
        let (a, b) = (&sector[16..20], &sector[20..24]);
        let src = if a[2] & 0x20 != 0 && a[2] & 0x08 != 0 && !(b[2] & 0x20 != 0 && b[2] & 0x08 != 0) { 20 } else { 16 };
        let copy: [u8; 4] = sector[src..src + 4].try_into().unwrap();
        sector[16..20].copy_from_slice(&copy);
        sector[20..24].copy_from_slice(&copy);
    }
    match detect_form(sector) {
        Form::Mode1 => {
            if f.edc {
                let e = edc(&sector[..0x810]);
                sector[0x810..0x814].copy_from_slice(&e.to_le_bytes());
                sector[0x814..0x81C].fill(0);
            }
            if f.ecc {
                let (p, q) = ecc_compute(sector, false);
                sector[0x81C..0x8C8].copy_from_slice(&p);
                sector[0x8C8..0x930].copy_from_slice(&q);
            }
        }
        Form::Mode2Form1 => {
            if f.edc {
                let e = edc(&sector[0x10..0x818]);
                sector[0x818..0x81C].copy_from_slice(&e.to_le_bytes());
            }
            if f.ecc {
                let (p, q) = ecc_compute(sector, true);
                sector[0x81C..0x8C8].copy_from_slice(&p);
                sector[0x8C8..0x930].copy_from_slice(&q);
            }
        }
        Form::Mode2Form2 => {
            // A stored zero means the mastering tool skipped the optional EDC.
            if f.edc && sector[0x92C..0x930] != [0; 4] {
                let e = edc(&sector[0x10..0x92C]);
                sector[0x92C..0x930].copy_from_slice(&e.to_le_bytes());
            }
        }
        _ => {}
    }
}

/// Builds a complete raw sector around 2048 bytes of Mode 1 user data.
pub fn make_mode1(lba: u32, data: &[u8]) -> Sector {
    let mut s = [0u8; RAW];
    s[15] = 1;
    s[16..16 + data.len().min(2048)].copy_from_slice(&data[..data.len().min(2048)]);
    rebuild(&mut s, lba, None, RebuildFields::default());
    s
}

/// Builds a complete raw sector around 2336 bytes of Mode 2 data (subheader included).
pub fn make_mode2(lba: u32, data: &[u8]) -> Sector {
    let mut s = [0u8; RAW];
    s[15] = 2;
    s[16..16 + data.len().min(2336)].copy_from_slice(&data[..data.len().min(2336)]);
    rebuild(&mut s, lba, None, RebuildFields::default());
    s
}

/// Builds a Mode 2 Form 1 sector from 2048 user bytes.
#[cfg(test)]
pub fn make_mode2_form1(lba: u32, subheader: [u8; 4], data: &[u8]) -> Sector {
    let mut s = [0u8; RAW];
    s[15] = 2;
    s[16..20].copy_from_slice(&subheader);
    s[20..24].copy_from_slice(&subheader);
    s[24..24 + data.len().min(2048)].copy_from_slice(&data[..data.len().min(2048)]);
    rebuild(&mut s, lba, None, RebuildFields::default());
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edc_table_matches_cdmage_polynomial() {
        // CDmage (sub_493216) builds an MSB-first table from 0x8001801B; the
        // reflected LSB-first form must agree bit-reversed.
        let mut msb = [0u32; 256];
        for (i, e) in msb.iter_mut().enumerate() {
            let mut v = (i as u32) << 24;
            for _ in 0..8 {
                v = if v & 0x8000_0000 != 0 { (v << 1) ^ 0x8001_801B } else { v << 1 };
            }
            *e = v;
        }
        let t = tables();
        for i in 0..256usize {
            let r = (i as u8).reverse_bits() as usize;
            assert_eq!(t.edc[i], msb[r].reverse_bits());
        }
    }

    #[test]
    fn generated_sectors_verify() {
        let data: Vec<u8> = (0..2048u32).map(|i| (i * 7 + 3) as u8).collect();
        let s = make_mode1(1234, &data);
        let (form, p) = check(&s, Some(1234), Some(1));
        assert_eq!(form, Form::Mode1);
        assert!(!p.any(), "{p:?}");
        let s2 = make_mode2_form1(16, [0, 0, 0x09, 0], &data);
        let (form, p) = check(&s2, Some(16), Some(2));
        assert_eq!(form, Form::Mode2Form1);
        assert!(!p.any(), "{p:?}");
        let mut bad = s2;
        bad[100] ^= 1;
        let (_, p) = check(&bad, Some(16), Some(2));
        assert!(p.edc && p.ecc_p && p.ecc_q);
        rebuild(&mut bad, 16, None, RebuildFields::default());
        assert!(!check(&bad, Some(16), Some(2)).1.any());
    }

    #[test]
    fn known_edc_value() {
        // A Mode 1 sector of zeros at LBA 0 has a well known EDC.
        let s = make_mode1(0, &[0u8; 2048]);
        assert_eq!(&s[12..16], &[0x00, 0x02, 0x00, 0x01]);
        assert!(!check(&s, Some(0), Some(1)).1.any());
    }
}

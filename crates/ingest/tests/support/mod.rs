//! Synthetic archive builders. Every fixture is generated at test time; no binary blobs are
//! checked in.
#![allow(dead_code)]

pub fn deflate(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec(data, 9)
}

pub fn gzip(name: Option<&str>, data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1F, 0x8B, 8, if name.is_some() { 0x08 } else { 0 }, 0, 0, 0, 0, 0, 3];
    if let Some(name) = name {
        out.extend_from_slice(name.as_bytes());
        out.push(0);
    }
    out.extend_from_slice(&deflate(data));
    out.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

pub fn zstd(data: &[u8]) -> Vec<u8> {
    zstd::encode_all(data, 3).unwrap()
}

pub struct ZipEntry {
    pub name: String,
    pub data: Vec<u8>,
    pub deflate: bool,
    /// Unix mode stored in the external attributes (made-by Unix) when present.
    pub unix_mode: Option<u32>,
}

pub fn file(name: &str, data: &[u8]) -> ZipEntry {
    ZipEntry {
        name: name.into(),
        data: data.to_vec(),
        deflate: true,
        unix_mode: None,
    }
}

pub fn zip(entries: &[ZipEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for e in entries {
        let offset = out.len() as u32;
        let (method, body) = if e.deflate {
            (8u16, deflate(&e.data))
        } else {
            (0u16, e.data.clone())
        };
        let crc = crc32fast::hash(&e.data);
        let name = e.name.as_bytes();
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(e.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&body);

        let made_by: u16 = if e.unix_mode.is_some() { 3 << 8 | 20 } else { 20 };
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&made_by.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&[0; 4]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(body.len() as u32).to_le_bytes());
        central.extend_from_slice(&(e.data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 8]);
        central.extend_from_slice(&(e.unix_mode.unwrap_or(0) << 16).to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

pub struct TarEntry {
    pub name: String,
    pub data: Vec<u8>,
    pub mode: u32,
    pub kind: u8,
    pub link: String,
}

pub fn tar_file(name: &str, data: &[u8]) -> TarEntry {
    TarEntry {
        name: name.into(),
        data: data.to_vec(),
        mode: 0o644,
        kind: b'0',
        link: String::new(),
    }
}

fn octal_field(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let s = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(s.as_bytes());
    field[digits] = 0;
}

pub fn tar(entries: &[TarEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        let mut h = [0u8; 512];
        h[..e.name.len()].copy_from_slice(e.name.as_bytes());
        octal_field(&mut h[100..108], u64::from(e.mode));
        octal_field(&mut h[108..116], 0);
        octal_field(&mut h[116..124], 0);
        octal_field(&mut h[124..136], e.data.len() as u64);
        octal_field(&mut h[136..148], 0);
        h[156] = e.kind;
        h[157..157 + e.link.len()].copy_from_slice(e.link.as_bytes());
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
        let s = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(s.as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(&e.data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.extend_from_slice(&[0u8; 1024]);
    out
}

/// Minimal 64-bit ELF header.
pub fn elf() -> Vec<u8> {
    let mut b = b"\x7FELF\x02\x01\x01".to_vec();
    b.resize(64, 0);
    b
}

/// Minimal PE image: DOS stub pointing at a `PE\0\0` signature.
pub fn pe() -> Vec<u8> {
    let mut b = vec![0u8; 0x80];
    b[0] = b'M';
    b[1] = b'Z';
    b[0x3C] = 0x40;
    b[0x40..0x44].copy_from_slice(b"PE\0\0");
    b
}

pub fn png() -> Vec<u8> {
    b"\x89PNG\r\n\x1A\n\0\0\0\rIHDR synthetic".to_vec()
}

//! Bounded, in-memory expansion of zip, gzip, zstd and tar containers. Nothing is written to
//! disk and nothing is executed: entries are returned as bytes for type detection and scanning.
//! Every decoder stops as soon as a limit would be exceeded instead of streaming past it.
use crate::{Code, Limits};
use miniz_oxide::{
    DataFormat, MZError, MZFlush, MZStatus,
    inflate::stream::{InflateState, inflate},
};
use std::io::Read;

/// Maximum zstd window (32 MiB) accepted by the decoder, bounding decoder memory.
const ZSTD_WINDOW_LOG_MAX: u32 = 25;
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub code: Code,
    pub message: String,
}

pub(crate) fn fail(code: Code, message: impl Into<String>) -> Failure {
    Failure {
        code,
        message: message.into(),
    }
}

/// Shared budget across every nesting level of one inspection.
#[derive(Debug)]
pub(crate) struct Budget<'a> {
    pub limits: &'a Limits,
    pub input_bytes: u64,
    pub expanded: u64,
    pub entries: u64,
}

impl Budget<'_> {
    fn remaining(&self) -> u64 {
        self.limits.max_expanded_bytes.saturating_sub(self.expanded)
    }
    fn count_entry(&mut self) -> Result<(), Failure> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(fail(
                Code::EntryLimit,
                format!("more than {} archive entries", self.limits.max_entries),
            ));
        }
        Ok(())
    }
    /// Output cap for one decompression stream of `compressed` input bytes.
    fn stream_cap(&self, compressed: u64) -> StreamCap {
        let ratio_cap = compressed
            .saturating_mul(self.limits.max_ratio)
            .max(self.limits.ratio_grace_bytes);
        StreamCap {
            total: self.remaining(),
            ratio: ratio_cap,
        }
    }
    fn charge(&mut self, produced: u64) -> Result<(), Failure> {
        self.expanded += produced;
        let overall = self
            .input_bytes
            .saturating_mul(self.limits.max_ratio)
            .max(self.limits.ratio_grace_bytes);
        if self.expanded > overall {
            return Err(fail(
                Code::RatioLimit,
                format!(
                    "total expansion exceeds {}:1 of the {}-byte input",
                    self.limits.max_ratio, self.input_bytes
                ),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct StreamCap {
    total: u64,
    ratio: u64,
}

impl StreamCap {
    fn check(&self, produced: u64, limits: &Limits) -> Result<(), Failure> {
        if produced > self.total {
            return Err(fail(
                Code::ExpansionLimit,
                format!(
                    "expanded content exceeds {} bytes",
                    limits.max_expanded_bytes
                ),
            ));
        }
        if produced > self.ratio {
            return Err(fail(
                Code::RatioLimit,
                format!("compression ratio exceeds {}:1", limits.max_ratio),
            ));
        }
        Ok(())
    }
}

/// One extracted entry, held in memory only.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub path: String,
    pub bytes: Vec<u8>,
    pub executable_mode: bool,
    pub directory: bool,
}

/// Validates an entry path: relative, no parent components, no NUL, no drive prefix.
pub fn safe_entry_path(name: &str) -> Result<String, Failure> {
    let unsafe_path = |why: &str| fail(Code::UnsafePath, format!("entry {name:?}: {why}"));
    if name.is_empty() {
        return Err(unsafe_path("empty name"));
    }
    if name.len() > 4096 {
        return Err(unsafe_path("name too long"));
    }
    if name.contains('\0') || name.chars().any(char::is_control) {
        return Err(unsafe_path("control character in name"));
    }
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(unsafe_path("absolute path"));
    }
    let bytes = normalized.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err(unsafe_path("drive-qualified path"));
    }
    let mut parts = Vec::new();
    for part in normalized.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(unsafe_path("parent directory traversal")),
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        return Err(unsafe_path("no path components"));
    }
    Ok(parts.join("/"))
}

/// Raw DEFLATE inflation that refuses to produce more than the cap allows.
/// Returns the output and the number of input bytes consumed.
fn inflate_bounded(
    input: &[u8],
    cap: StreamCap,
    limits: &Limits,
) -> Result<(Vec<u8>, usize), Failure> {
    let mut state = InflateState::new_boxed(DataFormat::Raw);
    let mut out = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    let mut pos = 0usize;
    loop {
        let res = inflate(&mut state, &input[pos..], &mut buf, MZFlush::None);
        pos += res.bytes_consumed;
        cap.check((out.len() + res.bytes_written) as u64, limits)?;
        out.extend_from_slice(&buf[..res.bytes_written]);
        match res.status {
            Ok(MZStatus::StreamEnd) => return Ok((out, pos)),
            Ok(_) if res.bytes_consumed > 0 || res.bytes_written > 0 => {}
            Ok(_) | Err(MZError::Buf) => {
                return Err(fail(Code::Malformed, "truncated deflate stream"));
            }
            Err(e) => {
                return Err(fail(Code::Malformed, format!("invalid deflate stream: {e:?}")));
            }
        }
    }
}

fn le16(b: &[u8], at: usize) -> Result<u16, Failure> {
    b.get(at..at + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| fail(Code::Malformed, "truncated header"))
}
fn le32(b: &[u8], at: usize) -> Result<u32, Failure> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| fail(Code::Malformed, "truncated header"))
}

/// Decodes every gzip member (RFC 1952) into one entry.
pub(crate) fn gunzip(input: &[u8], budget: &mut Budget<'_>) -> Result<Vec<Entry>, Failure> {
    let mut out = Vec::new();
    let mut name = None;
    let mut pos = 0usize;
    let cap = budget.stream_cap(input.len() as u64);
    while pos < input.len() {
        let h = &input[pos..];
        if h.len() < 18 || h[0] != 0x1F || h[1] != 0x8B {
            return Err(fail(Code::Malformed, "invalid or trailing gzip data"));
        }
        if h[2] != 8 {
            return Err(fail(Code::Unsupported, "gzip method other than deflate"));
        }
        let flags = h[3];
        if flags & 0xE0 != 0 {
            return Err(fail(Code::Malformed, "reserved gzip flags set"));
        }
        let mut p = 10usize;
        if flags & 0x04 != 0 {
            p += 2 + le16(h, p)? as usize;
        }
        let mut read_cstr = |p: &mut usize| -> Result<Vec<u8>, Failure> {
            let rest = h.get(*p..).unwrap_or(&[]);
            let end = rest
                .iter()
                .take(4096)
                .position(|&b| b == 0)
                .ok_or_else(|| fail(Code::Malformed, "unterminated gzip header field"))?;
            let s = rest[..end].to_vec();
            *p += end + 1;
            Ok(s)
        };
        if flags & 0x08 != 0 {
            let raw = read_cstr(&mut p)?;
            let member_name = String::from_utf8_lossy(&raw).into_owned();
            let safe = safe_entry_path(&member_name)?;
            name.get_or_insert(safe);
        }
        if flags & 0x10 != 0 {
            read_cstr(&mut p)?;
        }
        if flags & 0x02 != 0 {
            p += 2;
        }
        if p > h.len() {
            return Err(fail(Code::Malformed, "truncated gzip header"));
        }
        let member_cap = StreamCap {
            total: cap.total.saturating_sub(out.len() as u64),
            ratio: cap.ratio.saturating_sub(out.len() as u64),
        };
        let (data, used) = inflate_bounded(&h[p..], member_cap, budget.limits)?;
        p += used;
        let crc = le32(h, p)?;
        let size = le32(h, p + 4)?;
        if crc != crc32fast::hash(&data) || size != data.len() as u32 {
            return Err(fail(Code::Malformed, "gzip checksum or size mismatch"));
        }
        out.extend_from_slice(&data);
        pos += p + 8;
    }
    budget.count_entry()?;
    budget.charge(out.len() as u64)?;
    Ok(vec![Entry {
        path: name.unwrap_or_else(|| "<gzip>".into()),
        bytes: out,
        executable_mode: false,
        directory: false,
    }])
}

/// Decodes a zstd stream into one entry with a bounded window and bounded output.
pub(crate) fn unzstd(input: &[u8], budget: &mut Budget<'_>) -> Result<Vec<Entry>, Failure> {
    let cap = budget.stream_cap(input.len() as u64);
    let mut decoder = zstd::stream::read::Decoder::with_buffer(input)
        .map_err(|e| fail(Code::Malformed, format!("zstd: {e}")))?;
    decoder
        .window_log_max(ZSTD_WINDOW_LOG_MAX)
        .map_err(|e| fail(Code::Malformed, format!("zstd: {e}")))?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = decoder
            .read(&mut buf)
            .map_err(|e| fail(Code::Malformed, format!("zstd: {e}")))?;
        if n == 0 {
            break;
        }
        cap.check((out.len() + n) as u64, budget.limits)?;
        out.extend_from_slice(&buf[..n]);
    }
    budget.count_entry()?;
    budget.charge(out.len() as u64)?;
    Ok(vec![Entry {
        path: "<zstd>".into(),
        bytes: out,
        executable_mode: false,
        directory: false,
    }])
}

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

/// Parses a zip archive through its central directory with bounded inflation.
pub(crate) fn unzip(input: &[u8], budget: &mut Budget<'_>) -> Result<Vec<Entry>, Failure> {
    const EOCD: u32 = 0x0605_4b50;
    if input.len() < 22 {
        return Err(fail(Code::Malformed, "zip too short"));
    }
    let lowest = input.len().saturating_sub(22 + 0xFFFF);
    let eocd = (lowest..=input.len() - 22)
        .rev()
        .find(|&i| le32(input, i).ok() == Some(EOCD))
        .ok_or_else(|| fail(Code::Malformed, "zip end of central directory not found"))?;
    let disk = le16(input, eocd + 4)?;
    let cd_disk = le16(input, eocd + 6)?;
    let disk_entries = le16(input, eocd + 8)?;
    let total = le16(input, eocd + 10)?;
    let cd_size = le32(input, eocd + 12)? as usize;
    let cd_offset = le32(input, eocd + 16)? as usize;
    let comment = le16(input, eocd + 20)? as usize;
    if disk != 0 || cd_disk != 0 || disk_entries != total {
        return Err(fail(Code::Unsupported, "multi-disk zip"));
    }
    if total == 0xFFFF || cd_offset == 0xFFFF_FFFF || cd_size == 0xFFFF_FFFF {
        return Err(fail(Code::Unsupported, "zip64 archives are not supported"));
    }
    if eocd + 22 + comment != input.len() {
        return Err(fail(Code::Malformed, "data after zip end record"));
    }
    if cd_offset.checked_add(cd_size) != Some(eocd) {
        return Err(fail(Code::Malformed, "zip central directory bounds"));
    }
    if budget.entries + u64::from(total) > budget.limits.max_entries {
        return Err(fail(
            Code::EntryLimit,
            format!(
                "zip declares {total} entries, limit {}",
                budget.limits.max_entries
            ),
        ));
    }

    let mut entries = Vec::with_capacity(usize::from(total));
    let mut ranges = Vec::with_capacity(usize::from(total));
    let mut names = std::collections::BTreeSet::new();
    let mut p = cd_offset;
    for _ in 0..total {
        if le32(input, p)? != 0x0201_4b50 {
            return Err(fail(Code::Malformed, "bad zip central directory entry"));
        }
        let made_by = le16(input, p + 4)?;
        let flags = le16(input, p + 8)?;
        let method = le16(input, p + 10)?;
        let crc = le32(input, p + 16)?;
        let csize = le32(input, p + 20)? as usize;
        let usize_declared = le32(input, p + 24)? as u64;
        let name_len = le16(input, p + 28)? as usize;
        let extra_len = le16(input, p + 30)? as usize;
        let comment_len = le16(input, p + 32)? as usize;
        let external = le32(input, p + 38)?;
        let local = le32(input, p + 42)? as usize;
        let raw_name = input
            .get(p + 46..p + 46 + name_len)
            .ok_or_else(|| fail(Code::Malformed, "truncated zip name"))?;
        p += 46 + name_len + extra_len + comment_len;
        if p > eocd {
            return Err(fail(Code::Malformed, "zip central directory overflow"));
        }
        let name = std::str::from_utf8(raw_name)
            .map_err(|_| fail(Code::UnsafePath, "zip entry name is not UTF-8"))?;
        let path = safe_entry_path(name)?;
        let unix_mode = if made_by >> 8 == 3 { external >> 16 } else { 0 };
        let file_type = unix_mode & S_IFMT;
        if file_type == S_IFLNK {
            return Err(fail(
                Code::Link,
                format!("zip entry {path:?} is a symbolic link"),
            ));
        }
        if file_type != 0 && file_type != S_IFREG && file_type != S_IFDIR {
            return Err(fail(
                Code::Link,
                format!("zip entry {path:?} is a special file"),
            ));
        }
        let directory = name.ends_with('/') || file_type == S_IFDIR;
        if flags & 0x0001 != 0 {
            return Err(fail(
                Code::Unsupported,
                format!("zip entry {path:?} is encrypted and cannot be scanned"),
            ));
        }
        let key = if directory { format!("{path}/") } else { path.clone() };
        if !names.insert(key) {
            return Err(fail(
                Code::Malformed,
                format!("duplicate zip entry {path:?}"),
            ));
        }
        budget.count_entry()?;

        if le32(input, local)? != 0x0403_4b50 {
            return Err(fail(Code::Malformed, "bad zip local header"));
        }
        let lname = le16(input, local + 26)? as usize;
        let lextra = le16(input, local + 28)? as usize;
        if input.get(local + 30..local + 30 + lname) != Some(raw_name) {
            return Err(fail(
                Code::Malformed,
                format!("zip local name differs for {path:?}"),
            ));
        }
        let start = local + 30 + lname + lextra;
        let end = start
            .checked_add(csize)
            .filter(|&e| e <= cd_offset)
            .ok_or_else(|| fail(Code::Malformed, "zip entry data out of bounds"))?;
        ranges.push((local, end));
        let data = &input[start..end];

        let cap = budget.stream_cap(csize as u64);
        let bytes = match method {
            0 => {
                cap.check(data.len() as u64, budget.limits)?;
                data.to_vec()
            }
            8 => inflate_bounded(data, cap, budget.limits)?.0,
            m => {
                return Err(fail(
                    Code::Unsupported,
                    format!("zip compression method {m} for {path:?}"),
                ));
            }
        };
        if bytes.len() as u64 != usize_declared || crc32fast::hash(&bytes) != crc {
            return Err(fail(
                Code::Malformed,
                format!("zip entry {path:?} size or checksum mismatch"),
            ));
        }
        if directory && !bytes.is_empty() {
            return Err(fail(
                Code::Malformed,
                format!("zip directory {path:?} has data"),
            ));
        }
        if method != 0 {
            budget.charge(bytes.len() as u64)?;
        }
        entries.push(Entry {
            path,
            bytes,
            executable_mode: unix_mode & 0o111 != 0 && !directory,
            directory,
        });
    }
    // Overlapping local records are a known amplification technique; refuse them.
    ranges.sort_unstable();
    if ranges.windows(2).any(|w| w[1].0 < w[0].1) {
        return Err(fail(Code::Malformed, "overlapping zip entries"));
    }
    Ok(entries)
}

fn octal(field: &[u8]) -> Result<u64, Failure> {
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        return Err(fail(Code::Unsupported, "base-256 tar numeric field"));
    }
    let text: String = field
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as char)
        .collect();
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8).map_err(|_| fail(Code::Malformed, "invalid tar octal field"))
}

fn cstr(field: &[u8]) -> Result<&str, Failure> {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    std::str::from_utf8(&field[..end])
        .map_err(|_| fail(Code::UnsafePath, "tar entry name is not UTF-8"))
}

/// Parses a POSIX ustar archive. Links, devices and pax/GNU extension headers are refused.
pub(crate) fn untar(input: &[u8], budget: &mut Budget<'_>) -> Result<Vec<Entry>, Failure> {
    let mut entries = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    let mut pos = 0usize;
    loop {
        let header = input
            .get(pos..pos + 512)
            .ok_or_else(|| fail(Code::Malformed, "truncated tar header"))?;
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let stored = octal(&header[148..156])?;
        let sum: u64 = header
            .iter()
            .enumerate()
            .map(|(i, &b)| if (148..156).contains(&i) { 32 } else { u64::from(b) })
            .sum();
        if stored != sum {
            return Err(fail(Code::Malformed, "tar header checksum mismatch"));
        }
        let mut name = cstr(&header[0..100])?.to_string();
        if &header[257..262] == b"ustar" {
            let prefix = cstr(&header[345..500])?;
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        let path = safe_entry_path(&name)?;
        let mode = octal(&header[100..108])?;
        let size = octal(&header[124..136])?;
        let kind = header[156];
        let directory = match kind {
            b'0' | 0 | b'7' => false,
            b'5' => true,
            b'1' => {
                return Err(fail(Code::Link, format!("tar entry {path:?} is a hard link")));
            }
            b'2' => {
                return Err(fail(
                    Code::Link,
                    format!("tar entry {path:?} is a symbolic link"),
                ));
            }
            b'3' | b'4' | b'6' => {
                return Err(fail(
                    Code::Link,
                    format!("tar entry {path:?} is a special file"),
                ));
            }
            b'x' | b'g' | b'L' | b'K' => {
                return Err(fail(
                    Code::Unsupported,
                    "pax or GNU extended tar headers are not supported",
                ));
            }
            other => {
                return Err(fail(
                    Code::Unsupported,
                    format!("tar entry type {:?}", other as char),
                ));
            }
        };
        let start = pos + 512;
        let size = usize::try_from(size).map_err(|_| fail(Code::Malformed, "tar size"))?;
        let end = start
            .checked_add(size)
            .filter(|&e| e <= input.len())
            .ok_or_else(|| fail(Code::Malformed, "tar entry data out of bounds"))?;
        if directory && size != 0 {
            return Err(fail(Code::Malformed, format!("tar directory {path:?} has data")));
        }
        let key = if directory { format!("{path}/") } else { path.clone() };
        if !names.insert(key) {
            return Err(fail(Code::Malformed, format!("duplicate tar entry {path:?}")));
        }
        budget.count_entry()?;
        entries.push(Entry {
            path,
            bytes: input[start..end].to_vec(),
            executable_mode: mode & 0o111 != 0 && !directory,
            directory,
        });
        pos = start + size.div_ceil(512) * 512;
    }
    Ok(entries)
}

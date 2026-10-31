//! Bounded, in-memory expansion of zip, gzip, zstd and tar containers. Nothing is written to
//! disk and nothing is executed: entries are returned as bytes for type detection and scanning.
//! Container structure is validated before any entry is materialized, stored entries borrow
//! the input instead of copying it, and every decoder stops as soon as a limit would be
//! exceeded instead of streaming past it.
use crate::{Code, Limits};
use miniz_oxide::{
    DataFormat, MZError, MZFlush, MZStatus,
    inflate::stream::{InflateState, inflate},
};
use std::{borrow::Cow, io::Read};

/// Maximum zstd window (32 MiB) accepted by the decoder, bounding decoder memory.
const ZSTD_WINDOW_LOG_MAX: u32 = 25;
const CHUNK: usize = 64 * 1024;
/// Longest piece of attacker-controlled text echoed in a message.
const ECHO_CHARS: usize = 96;

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

/// Debug-quotes untrusted text, truncated to a bounded number of characters.
pub(crate) fn quoted(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(ECHO_CHARS).collect();
    if chars.next().is_some() {
        format!("{head:?}… ({} bytes)", text.len())
    } else {
        format!("{head:?}")
    }
}

/// Shared budget across every nesting level of one inspection.
#[derive(Debug)]
pub(crate) struct Budget<'a> {
    pub limits: &'a Limits,
    pub input_bytes: u64,
    /// Bytes produced by decoders plus bytes of stored entries handed out for inspection.
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
        if produced > self.remaining() {
            return Err(fail(
                Code::ExpansionLimit,
                format!(
                    "expanded content exceeds {} bytes",
                    self.limits.max_expanded_bytes
                ),
            ));
        }
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
    fn members(&self, count: u64, what: &str) -> Result<(), Failure> {
        if count > self.limits.max_stream_members {
            return Err(fail(
                Code::EntryLimit,
                format!(
                    "more than {} {what} in one stream",
                    self.limits.max_stream_members
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

/// One entry, held in memory only. Stored entries borrow the container bytes.
#[derive(Clone, Debug)]
pub(crate) struct Entry<'a> {
    pub path: String,
    pub bytes: Cow<'a, [u8]>,
    pub executable_mode: bool,
    pub directory: bool,
}

/// Validates an entry path: relative, no parent components, no NUL, no drive prefix.
pub fn safe_entry_path(name: &str) -> Result<String, Failure> {
    let unsafe_path = |why: &str| fail(Code::UnsafePath, format!("entry {}: {why}", quoted(name)));
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

/// Reusable raw DEFLATE decoder: one state and one output chunk per container.
struct Inflater {
    state: Box<InflateState>,
    buf: Vec<u8>,
}

impl Inflater {
    fn new() -> Self {
        Self {
            state: InflateState::new_boxed(DataFormat::Raw),
            buf: vec![0u8; CHUNK],
        }
    }

    /// Appends one raw DEFLATE stream to `out`, refusing to let `out` grow past the cap.
    /// Returns the number of input bytes consumed by the stream.
    fn inflate_into(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        cap: StreamCap,
        limits: &Limits,
    ) -> Result<usize, Failure> {
        self.state.reset(DataFormat::Raw);
        let mut pos = 0usize;
        loop {
            let res = inflate(&mut self.state, &input[pos..], &mut self.buf, MZFlush::None);
            pos += res.bytes_consumed;
            cap.check((out.len() + res.bytes_written) as u64, limits)?;
            out.extend_from_slice(&self.buf[..res.bytes_written]);
            match res.status {
                Ok(MZStatus::StreamEnd) => return Ok(pos),
                Ok(_) if res.bytes_consumed > 0 || res.bytes_written > 0 => {}
                Ok(_) | Err(MZError::Buf) => {
                    return Err(fail(Code::Malformed, "truncated deflate stream"));
                }
                Err(e) => {
                    return Err(fail(
                        Code::Malformed,
                        format!("invalid deflate stream: {e:?}"),
                    ));
                }
            }
        }
    }
}

fn le16(b: &[u8], at: usize) -> Result<u16, Failure> {
    b.get(at..at.saturating_add(2))
        .filter(|s| s.len() == 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| fail(Code::Malformed, "truncated header"))
}
fn le32(b: &[u8], at: usize) -> Result<u32, Failure> {
    b.get(at..at.saturating_add(4))
        .filter(|s| s.len() == 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| fail(Code::Malformed, "truncated header"))
}

/// Decodes every gzip member (RFC 1952) into one entry.
pub(crate) fn gunzip<'a>(
    input: &'a [u8],
    budget: &mut Budget<'_>,
) -> Result<Vec<Entry<'a>>, Failure> {
    let mut out = Vec::new();
    let mut name = None;
    let mut pos = 0usize;
    let mut members = 0u64;
    let mut inflater = Inflater::new();
    let cap = budget.stream_cap(input.len() as u64);
    while pos < input.len() {
        members += 1;
        budget.members(members, "gzip members")?;
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
        let read_cstr = |p: &mut usize| -> Result<Vec<u8>, Failure> {
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
        let start = out.len();
        p += inflater.inflate_into(&h[p..], &mut out, cap, budget.limits)?;
        let crc = le32(h, p)?;
        let size = le32(h, p + 4)?;
        let member = &out[start..];
        if crc != crc32fast::hash(member) || size != member.len() as u32 {
            return Err(fail(Code::Malformed, "gzip checksum or size mismatch"));
        }
        pos += p + 8;
    }
    budget.count_entry()?;
    budget.charge(out.len() as u64)?;
    Ok(vec![Entry {
        path: name.unwrap_or_else(|| "<gzip>".into()),
        bytes: Cow::Owned(out),
        executable_mode: false,
        directory: false,
    }])
}

const ZSTD_MAGIC: u32 = 0xFD2F_B528;

/// Walks the zstd frame sequence before decoding: only standard and skippable frames are
/// accepted. Legacy (pre-1.0) frames are refused so the bounded window always applies.
fn zstd_frames(input: &[u8], budget: &Budget<'_>) -> Result<(), Failure> {
    let mut pos = 0usize;
    let mut frames = 0u64;
    while pos < input.len() {
        frames += 1;
        budget.members(frames, "zstd frames")?;
        let magic = le32(input, pos)?;
        let size = if magic == ZSTD_MAGIC {
            zstd::zstd_safe::find_frame_compressed_size(&input[pos..])
                .map_err(|_| fail(Code::Malformed, "invalid zstd frame"))?
        } else if magic & 0xFFFF_FFF0 == 0x184D_2A50 {
            8usize.saturating_add(le32(input, pos + 4)? as usize)
        } else if (0xFD2F_B51E..=0xFD2F_B527).contains(&magic) {
            return Err(fail(
                Code::Unsupported,
                "legacy zstd frames are not supported",
            ));
        } else {
            return Err(fail(Code::Malformed, "trailing data after zstd frame"));
        };
        pos = pos
            .checked_add(size)
            .filter(|&e| e <= input.len() && size > 0)
            .ok_or_else(|| fail(Code::Malformed, "truncated zstd frame"))?;
    }
    Ok(())
}

/// Decodes a zstd stream into one entry with a bounded window and bounded output.
pub(crate) fn unzstd<'a>(
    input: &'a [u8],
    budget: &mut Budget<'_>,
) -> Result<Vec<Entry<'a>>, Failure> {
    zstd_frames(input, budget)?;
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
        bytes: Cow::Owned(out),
        executable_mode: false,
        directory: false,
    }])
}

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

/// Central-directory entry validated against its local header; nothing is decoded yet.
struct ZipRecord {
    path: String,
    directory: bool,
    executable: bool,
    method: u16,
    crc: u32,
    size: u64,
    data: (usize, usize),
}

/// Parses a zip archive through its central directory. Every structural check (bounds,
/// local/central agreement, full central-directory consumption, contiguous non-overlapping
/// records) runs before any entry is decoded or handed out.
pub(crate) fn unzip<'a>(
    input: &'a [u8],
    budget: &mut Budget<'_>,
) -> Result<Vec<Entry<'a>>, Failure> {
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

    let mut records = Vec::with_capacity(usize::from(total));
    let mut spans = Vec::with_capacity(usize::from(total));
    let mut names = std::collections::BTreeSet::new();
    let mut p = cd_offset;
    for _ in 0..total {
        if le32(input, p)? != 0x0201_4b50 {
            return Err(fail(Code::Malformed, "bad zip central directory entry"));
        }
        let flags = le16(input, p + 8)?;
        let method = le16(input, p + 10)?;
        let crc = le32(input, p + 16)?;
        let csize = le32(input, p + 20)? as usize;
        let size = le32(input, p + 24)?;
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
        let shown = quoted(&path);
        // Info-ZIP applies the high 16 bits as a Unix mode for several host systems (Unix,
        // VMS, BeOS, AtheOS and others), and some writers set them on FAT/NTFS too. Interpret
        // them whenever they are present, whatever the host byte says.
        let mode = external >> 16;
        let file_type = mode & S_IFMT;
        if file_type == S_IFLNK {
            return Err(fail(
                Code::Link,
                format!("zip entry {shown} is a symbolic link"),
            ));
        }
        if file_type != 0 && file_type != S_IFREG && file_type != S_IFDIR {
            return Err(fail(
                Code::Link,
                format!("zip entry {shown} is a special file"),
            ));
        }
        let directory = name.ends_with('/') || file_type == S_IFDIR;
        if flags & 0x0001 != 0 {
            return Err(fail(
                Code::Unsupported,
                format!("zip entry {shown} is encrypted and cannot be scanned"),
            ));
        }
        if method != 0 && method != 8 {
            return Err(fail(
                Code::Unsupported,
                format!("zip compression method {method} for {shown}"),
            ));
        }
        let key = if directory {
            format!("{path}/")
        } else {
            path.clone()
        };
        if !names.insert(key) {
            return Err(fail(
                Code::Malformed,
                format!("duplicate zip entry {shown}"),
            ));
        }
        budget.count_entry()?;

        // The local header must describe the same entry, so that streaming extractors which
        // read local headers see exactly what was inspected.
        if le32(input, local)? != 0x0403_4b50 {
            return Err(fail(Code::Malformed, "bad zip local header"));
        }
        let mismatch = |what: &str| {
            fail(
                Code::Malformed,
                format!("zip local header {what} differs for {shown}"),
            )
        };
        if le16(input, local + 6)? != flags {
            return Err(mismatch("flags"));
        }
        if le16(input, local + 8)? != method {
            return Err(mismatch("method"));
        }
        let (lcrc, lcsize, lsize) = (
            le32(input, local + 14)?,
            le32(input, local + 18)? as usize,
            le32(input, local + 22)?,
        );
        let lname = le16(input, local + 26)? as usize;
        let lextra = le16(input, local + 28)? as usize;
        if input.get(local + 30..local + 30 + lname) != Some(raw_name) {
            return Err(mismatch("name"));
        }
        let descriptor = flags & 0x0008 != 0;
        let agrees = |l: u64, c: u64| l == c || (descriptor && l == 0);
        if !agrees(lcrc.into(), crc.into())
            || !agrees(lcsize as u64, csize as u64)
            || !agrees(lsize.into(), size.into())
        {
            return Err(mismatch("checksum or sizes"));
        }
        let start = local + 30 + lname + lextra;
        let end = start
            .checked_add(csize)
            .filter(|&e| e <= cd_offset)
            .ok_or_else(|| fail(Code::Malformed, "zip entry data out of bounds"))?;
        let mut record_end = end;
        if descriptor {
            let at = if le32(input, end).ok() == Some(0x0807_4b50) {
                end + 4
            } else {
                end
            };
            if le32(input, at)? != crc
                || le32(input, at + 4)? as usize != csize
                || le32(input, at + 8)? != size
            {
                return Err(mismatch("data descriptor"));
            }
            record_end = at + 12;
        }
        if record_end > cd_offset {
            return Err(fail(Code::Malformed, "zip entry data out of bounds"));
        }
        spans.push((local, record_end));
        records.push(ZipRecord {
            path,
            directory,
            executable: mode & 0o111 != 0 && !directory,
            method,
            crc,
            size: size.into(),
            data: (start, end),
        });
    }
    if p != eocd {
        return Err(fail(
            Code::Malformed,
            "zip central directory holds more data than its declared entries",
        ));
    }
    // Local records must tile the area before the central directory exactly: overlapping
    // records are an amplification technique and gaps can hide entries from this parser.
    spans.sort_unstable();
    let mut cursor = 0usize;
    for (start, end) in &spans {
        if *start != cursor {
            return Err(fail(
                Code::Malformed,
                if *start < cursor {
                    "overlapping zip entries"
                } else {
                    "unaccounted data between zip entries"
                },
            ));
        }
        cursor = *end;
    }
    if cursor != cd_offset {
        return Err(fail(
            Code::Malformed,
            "unaccounted data before the zip central directory",
        ));
    }

    let mut inflater = Inflater::new();
    let mut entries = Vec::with_capacity(records.len());
    for record in records {
        let data = &input[record.data.0..record.data.1];
        let shown = quoted(&record.path);
        let bytes: Cow<'a, [u8]> = if record.method == 0 {
            budget
                .stream_cap(data.len() as u64)
                .check(data.len() as u64, budget.limits)?;
            Cow::Borrowed(data)
        } else {
            let mut out = Vec::new();
            let cap = budget.stream_cap(data.len() as u64);
            let used = inflater.inflate_into(data, &mut out, cap, budget.limits)?;
            if used != data.len() {
                return Err(fail(
                    Code::Malformed,
                    format!("trailing data after the deflate stream of {shown}"),
                ));
            }
            Cow::Owned(out)
        };
        if bytes.len() as u64 != record.size || crc32fast::hash(&bytes) != record.crc {
            return Err(fail(
                Code::Malformed,
                format!("zip entry {shown} size or checksum mismatch"),
            ));
        }
        if record.directory && !bytes.is_empty() {
            return Err(fail(
                Code::Malformed,
                format!("zip directory {shown} has data"),
            ));
        }
        budget.charge(bytes.len() as u64)?;
        entries.push(Entry {
            path: record.path,
            bytes,
            executable_mode: record.executable,
            directory: record.directory,
        });
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

/// Parses a POSIX ustar archive. Links, devices, pax/GNU extension headers and any non-zero
/// data after the end-of-archive block are refused.
pub(crate) fn untar<'a>(
    input: &'a [u8],
    budget: &mut Budget<'_>,
) -> Result<Vec<Entry<'a>>, Failure> {
    let mut entries = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    let mut pos = 0usize;
    loop {
        let header = input
            .get(pos..pos + 512)
            .ok_or_else(|| fail(Code::Malformed, "truncated tar header"))?;
        if header.iter().all(|&b| b == 0) {
            if input[pos..].iter().any(|&b| b != 0) {
                return Err(fail(
                    Code::Malformed,
                    "data after the tar end-of-archive block",
                ));
            }
            break;
        }
        let stored = octal(&header[148..156])?;
        let sum: u64 = header
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u64::from(b)
                }
            })
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
        let shown = quoted(&path);
        let mode = octal(&header[100..108])?;
        let size = octal(&header[124..136])?;
        let kind = header[156];
        let directory = match kind {
            b'0' | 0 | b'7' => false,
            b'5' => true,
            b'1' => {
                return Err(fail(
                    Code::Link,
                    format!("tar entry {shown} is a hard link"),
                ));
            }
            b'2' => {
                return Err(fail(
                    Code::Link,
                    format!("tar entry {shown} is a symbolic link"),
                ));
            }
            b'3' | b'4' | b'6' => {
                return Err(fail(
                    Code::Link,
                    format!("tar entry {shown} is a special file"),
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
            return Err(fail(
                Code::Malformed,
                format!("tar directory {shown} has data"),
            ));
        }
        let key = if directory {
            format!("{path}/")
        } else {
            path.clone()
        };
        if !names.insert(key) {
            return Err(fail(
                Code::Malformed,
                format!("duplicate tar entry {shown}"),
            ));
        }
        budget.count_entry()?;
        budget.charge(size as u64)?;
        entries.push(Entry {
            path,
            bytes: Cow::Borrowed(&input[start..end]),
            executable_mode: mode & 0o111 != 0 && !directory,
            directory,
        });
        pos = start + size.div_ceil(512) * 512;
    }
    Ok(entries)
}

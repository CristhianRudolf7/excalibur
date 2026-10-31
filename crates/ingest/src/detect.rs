//! Content-signature type detection. The declared media type and the file name are never
//! trusted: the kind is derived from the leading bytes only.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Valid UTF-8 without NUL bytes and without a stronger signature.
    Text,
    Pdf,
    Png,
    Jpeg,
    Gif,
    Webp,
    Wav,
    Mp3,
    Ogg,
    Flac,
    Mp4,
    Zip,
    Gzip,
    Zstd,
    Tar,
    Elf,
    Pe,
    MachO,
    Wasm,
    /// Binary content without a recognised signature.
    Binary,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Pdf => "pdf",
            Kind::Png => "png",
            Kind::Jpeg => "jpeg",
            Kind::Gif => "gif",
            Kind::Webp => "webp",
            Kind::Wav => "wav",
            Kind::Mp3 => "mp3",
            Kind::Ogg => "ogg",
            Kind::Flac => "flac",
            Kind::Mp4 => "mp4",
            Kind::Zip => "zip",
            Kind::Gzip => "gzip",
            Kind::Zstd => "zstd",
            Kind::Tar => "tar",
            Kind::Elf => "elf",
            Kind::Pe => "pe",
            Kind::MachO => "mach_o",
            Kind::Wasm => "wasm",
            Kind::Binary => "binary",
        }
    }
    /// Native or portable executable code. Never accepted.
    pub fn is_executable(self) -> bool {
        matches!(self, Kind::Elf | Kind::Pe | Kind::MachO | Kind::Wasm)
    }
    pub fn is_archive(self) -> bool {
        matches!(self, Kind::Zip | Kind::Gzip | Kind::Zstd | Kind::Tar)
    }
    /// Kinds identified by a positive signature (everything except text and unknown binary).
    pub fn has_signature(self) -> bool {
        !matches!(self, Kind::Text | Kind::Binary)
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

fn is_pe(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"MZ") {
        return false;
    }
    if bytes.len() >= 0x40 {
        let off = u32::from_le_bytes([bytes[0x3C], bytes[0x3D], bytes[0x3E], bytes[0x3F]]) as usize;
        if off
            .checked_add(4)
            .is_some_and(|end| end <= bytes.len() && &bytes[off..end] == b"PE\0\0")
        {
            return true;
        }
    }
    // A DOS executable without a PE header is still code unless the bytes are plain text.
    !is_text(bytes)
}

/// Detects the content kind from leading bytes.
pub fn detect(bytes: &[u8]) -> Kind {
    let at = |offset: usize, magic: &[u8]| {
        bytes.len() >= offset + magic.len() && &bytes[offset..offset + magic.len()] == magic
    };
    if at(0, b"%PDF") {
        Kind::Pdf
    } else if at(0, b"\x89PNG") {
        Kind::Png
    } else if at(0, b"\xFF\xD8\xFF") {
        Kind::Jpeg
    } else if at(0, b"GIF87a") || at(0, b"GIF89a") {
        Kind::Gif
    } else if at(0, b"RIFF") && at(8, b"WEBP") {
        Kind::Webp
    } else if at(0, b"RIFF") && at(8, b"WAVE") {
        Kind::Wav
    } else if at(0, b"ID3") {
        Kind::Mp3
    } else if at(0, b"OggS") {
        Kind::Ogg
    } else if at(0, b"fLaC") {
        Kind::Flac
    } else if at(4, b"ftyp") {
        Kind::Mp4
    } else if at(0, b"PK\x03\x04") || at(0, b"PK\x05\x06") || at(0, b"PK\x07\x08") {
        Kind::Zip
    } else if at(0, b"\x1F\x8B") {
        Kind::Gzip
    } else if at(0, b"\x28\xB5\x2F\xFD") {
        Kind::Zstd
    } else if at(257, b"ustar") {
        Kind::Tar
    } else if at(0, b"\x7FELF") {
        Kind::Elf
    } else if is_pe(bytes) {
        Kind::Pe
    } else if at(0, b"\xFE\xED\xFA\xCE")
        || at(0, b"\xFE\xED\xFA\xCF")
        || at(0, b"\xCE\xFA\xED\xFE")
        || at(0, b"\xCF\xFA\xED\xFE")
        || at(0, b"\xCA\xFE\xBA\xBE")
    {
        Kind::MachO
    } else if at(0, b"\0asm") {
        Kind::Wasm
    } else if is_text(bytes) {
        Kind::Text
    } else {
        Kind::Binary
    }
}

/// What a declared media type or extension promises about the content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expectation {
    /// Must be UTF-8 text without NUL bytes.
    Text,
    /// Must carry this exact signature.
    Strict(Kind),
    /// Usually carries this signature; only a different positive signature is a mismatch,
    /// because some valid encodings (for example raw MPEG frames) have no reliable magic.
    Loose(Kind),
    /// A media family (`image/*`, `audio/*`, `video/*`) without a specific signature: only
    /// documents, archives and executables are mismatches.
    Media,
    /// No promise: anything that is not executable is acceptable.
    Any,
}

impl Expectation {
    /// Returns a mismatch description when `detected` breaks the promise.
    pub fn conflict(self, detected: Kind) -> Option<String> {
        match self {
            Expectation::Text if detected != Kind::Text => Some(format!("content is {detected}")),
            Expectation::Strict(kind) if detected != kind => {
                Some(format!("expected {kind} signature, content is {detected}"))
            }
            Expectation::Loose(kind) if detected.has_signature() && detected != kind => {
                Some(format!("expected {kind}, content is {detected}"))
            }
            Expectation::Media
                if detected.is_archive()
                    || detected.is_executable()
                    || detected == Kind::Pdf =>
            {
                Some(format!("expected media, content is {detected}"))
            }
            _ => None,
        }
    }
}

pub fn is_text_media_type(media_type: &str) -> bool {
    let m = essence(media_type);
    m.starts_with("text/")
        || matches!(
            m.as_str(),
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/x-ndjson"
        )
        || m.ends_with("+json")
        || m.ends_with("+xml")
}

fn essence(media_type: &str) -> String {
    media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

pub fn expectation_for_media_type(media_type: &str) -> Expectation {
    let m = essence(media_type);
    if is_text_media_type(&m) {
        return Expectation::Text;
    }
    match m.as_str() {
        "application/pdf" => Expectation::Strict(Kind::Pdf),
        "image/png" => Expectation::Strict(Kind::Png),
        "image/jpeg" | "image/jpg" => Expectation::Strict(Kind::Jpeg),
        "image/gif" => Expectation::Strict(Kind::Gif),
        "image/webp" => Expectation::Strict(Kind::Webp),
        "application/zip" | "application/x-zip-compressed" | "application/epub+zip" => {
            Expectation::Strict(Kind::Zip)
        }
        "application/gzip" | "application/x-gzip" => Expectation::Strict(Kind::Gzip),
        "application/zstd" => Expectation::Strict(Kind::Zstd),
        "application/x-tar" => Expectation::Strict(Kind::Tar),
        "audio/wav" | "audio/x-wav" | "audio/wave" => Expectation::Loose(Kind::Wav),
        "audio/mpeg" | "audio/mp3" => Expectation::Loose(Kind::Mp3),
        "audio/ogg" | "video/ogg" | "audio/opus" => Expectation::Loose(Kind::Ogg),
        "audio/flac" | "audio/x-flac" => Expectation::Loose(Kind::Flac),
        "video/mp4" | "audio/mp4" | "audio/x-m4a" | "video/quicktime" => {
            Expectation::Loose(Kind::Mp4)
        }
        m if m.starts_with("application/vnd.openxmlformats-officedocument.")
            || m.starts_with("application/vnd.oasis.opendocument.") =>
        {
            Expectation::Strict(Kind::Zip)
        }
        m if m.starts_with("image/") || m.starts_with("audio/") || m.starts_with("video/") => {
            Expectation::Media
        }
        _ => Expectation::Any,
    }
}

/// Expectation implied by a file name extension, if the extension is known.
pub fn expectation_for_name(name: &str) -> Option<Expectation> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let lower = base.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return Some(Expectation::Strict(Kind::Gzip));
    }
    if lower.ends_with(".tar.zst") {
        return Some(Expectation::Strict(Kind::Zstd));
    }
    let (_, ext) = lower.rsplit_once('.')?;
    Some(match ext {
        "txt" | "md" | "markdown" | "csv" | "tsv" | "json" | "jsonl" | "xml" | "yaml" | "yml"
        | "toml" | "html" | "htm" | "srt" | "vtt" | "log" | "ini" | "rst" => Expectation::Text,
        "pdf" => Expectation::Strict(Kind::Pdf),
        "png" => Expectation::Strict(Kind::Png),
        "jpg" | "jpeg" => Expectation::Strict(Kind::Jpeg),
        "gif" => Expectation::Strict(Kind::Gif),
        "webp" => Expectation::Strict(Kind::Webp),
        "zip" | "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp" | "epub" => {
            Expectation::Strict(Kind::Zip)
        }
        "gz" => Expectation::Strict(Kind::Gzip),
        "zst" => Expectation::Strict(Kind::Zstd),
        "tar" => Expectation::Strict(Kind::Tar),
        "wav" => Expectation::Loose(Kind::Wav),
        "mp3" => Expectation::Loose(Kind::Mp3),
        "ogg" | "oga" | "opus" => Expectation::Loose(Kind::Ogg),
        "flac" => Expectation::Loose(Kind::Flac),
        "mp4" | "m4a" | "mov" => Expectation::Loose(Kind::Mp4),
        _ => return None,
    })
}

//! Deterministic ingestion guard for files entering the harness (architecture 20.7).
//!
//! [`ContentGuard`] detects the type from content signatures rather than names or declared
//! media types, expands zip, gzip, zstd and ustar containers in memory under total-size,
//! ratio, entry-count and nesting limits, refuses traversal, link and special entries, calls an
//! optional pluggable [`Scanner`] on every blob, and records rejected files in an optional
//! durable [`Quarantine`]. Nothing is extracted to an executable location, executed, or given
//! an executable mode: entries exist only as bytes in memory, and quarantined objects are
//! written with mode 0600.
pub mod archive;
pub mod detect;
pub mod quarantine;
pub mod scanner;

pub use archive::safe_entry_path;
pub use detect::{Expectation, Kind, detect};
pub use quarantine::{Quarantine, QuarantineItem, QuarantineRecord};
pub use scanner::{ScanVerdict, Scanner, SignatureScanner, eicar_test_string};

use archive::{Budget, Entry, Failure, fail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Stable rejection codes, recorded in the quarantine manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    Empty,
    Oversized,
    InvalidText,
    TypeMismatch,
    Executable,
    ExpansionLimit,
    RatioLimit,
    EntryLimit,
    DepthLimit,
    UnsafePath,
    /// Symbolic link, hard link or special file inside an archive.
    Link,
    Malformed,
    Unsupported,
    ScannerHit,
    ScannerFailed,
    ScannerRequired,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Empty => "empty",
            Code::Oversized => "oversized",
            Code::InvalidText => "invalid_text",
            Code::TypeMismatch => "type_mismatch",
            Code::Executable => "executable",
            Code::ExpansionLimit => "expansion_limit",
            Code::RatioLimit => "ratio_limit",
            Code::EntryLimit => "entry_limit",
            Code::DepthLimit => "depth_limit",
            Code::UnsafePath => "unsafe_path",
            Code::Link => "link",
            Code::Malformed => "malformed",
            Code::Unsupported => "unsupported",
            Code::ScannerHit => "scanner_hit",
            Code::ScannerFailed => "scanner_failed",
            Code::ScannerRequired => "scanner_required",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Size of the submitted file.
    pub max_input_bytes: u64,
    /// Total bytes produced by all decompression streams across every nesting level.
    pub max_expanded_bytes: u64,
    /// Maximum expanded/compressed ratio per stream and for the whole file.
    pub max_ratio: u64,
    /// Output below this many bytes is exempt from the ratio rule (tiny files compress well).
    pub ratio_grace_bytes: u64,
    /// Total archive entries across every nesting level.
    pub max_entries: u64,
    /// Maximum container nesting; a plain archive has depth 1, `.tar.gz` has depth 2.
    pub max_depth: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: 64 * 1024 * 1024,
            max_expanded_bytes: 256 * 1024 * 1024,
            max_ratio: 100,
            ratio_grace_bytes: 1024 * 1024,
            max_entries: 10_000,
            max_depth: 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MismatchPolicy {
    /// Declared type or extension disagreeing with the content is rejected.
    #[default]
    Reject,
    /// The disagreement is recorded as a flag and the file is accepted.
    Flag,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub limits: Limits,
    pub mismatch: MismatchPolicy,
    /// Reject every file when no scanner is configured.
    pub require_scanner: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryReport {
    pub path: String,
    pub depth: u32,
    pub size: u64,
    pub detected: Kind,
    pub directory: bool,
    /// The archive asked for an executable mode; it is never applied.
    pub executable_mode_ignored: bool,
}

/// Accepted file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inspection {
    pub sha256: String,
    pub size: u64,
    pub declared_type: String,
    pub detected: Kind,
    /// Non-fatal observations (flagged mismatches, ignored executable modes).
    pub flags: Vec<String>,
    pub entries: Vec<EntryReport>,
    pub expanded_bytes: u64,
    /// Name of the scanner that cleared every blob, if one is configured.
    pub scanner: Option<String>,
}

/// Rejected file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub code: Code,
    pub message: String,
    pub sha256: String,
    pub detected: Kind,
    /// Present when a quarantine is configured and the file was recorded.
    pub quarantine: Option<QuarantineRecord>,
    /// Present when a quarantine is configured but recording failed.
    pub quarantine_error: Option<String>,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)?;
        if let Some(e) = &self.quarantine_error {
            write!(f, " (quarantine failed: {e})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Rejection {}

#[derive(Clone, Default)]
pub struct ContentGuard {
    pub policy: Policy,
    pub scanner: Option<Arc<dyn Scanner>>,
    pub quarantine: Option<Arc<Quarantine>>,
}

impl std::fmt::Debug for ContentGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentGuard")
            .field("policy", &self.policy)
            .field("scanner", &self.scanner.as_ref().map(|s| s.name().to_string()))
            .field("quarantine", &self.quarantine.as_ref().map(|q| q.dir().to_path_buf()))
            .finish()
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

struct Walk<'a> {
    guard: &'a ContentGuard,
    budget: Budget<'a>,
    flags: Vec<String>,
    entries: Vec<EntryReport>,
}

impl ContentGuard {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            scanner: None,
            quarantine: None,
        }
    }
    pub fn with_scanner(mut self, scanner: Arc<dyn Scanner>) -> Self {
        self.scanner = Some(scanner);
        self
    }
    pub fn with_quarantine(mut self, quarantine: Arc<Quarantine>) -> Self {
        self.quarantine = Some(quarantine);
        self
    }

    /// Inspects one file. `name` is the original file name, when the caller has one; its
    /// extension is checked against the content like the declared media type.
    pub fn examine(
        &self,
        name: Option<&str>,
        declared_type: &str,
        bytes: &[u8],
    ) -> Result<Inspection, Rejection> {
        let sha256 = sha256_hex(bytes);
        let detected = detect(bytes);
        match self.evaluate(name, declared_type, bytes, detected) {
            Ok((flags, entries, expanded_bytes)) => Ok(Inspection {
                sha256,
                size: bytes.len() as u64,
                declared_type: declared_type.to_string(),
                detected,
                flags,
                entries,
                expanded_bytes,
                scanner: self.scanner.as_ref().map(|s| s.name().to_string()),
            }),
            Err(failure) => {
                let mut rejection = Rejection {
                    code: failure.code,
                    message: failure.message,
                    sha256,
                    detected,
                    quarantine: None,
                    quarantine_error: None,
                };
                if let Some(q) = &self.quarantine
                    && !bytes.is_empty()
                {
                    match q.store(QuarantineItem {
                        bytes,
                        sha256: &rejection.sha256,
                        reason_code: rejection.code.as_str(),
                        reason: &rejection.message,
                        original_name: name,
                        declared_type,
                        detected_type: detected.as_str(),
                    }) {
                        Ok(record) => rejection.quarantine = Some(record),
                        Err(e) => rejection.quarantine_error = Some(e.to_string()),
                    }
                }
                Err(rejection)
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn evaluate(
        &self,
        name: Option<&str>,
        declared_type: &str,
        bytes: &[u8],
        detected: Kind,
    ) -> Result<(Vec<String>, Vec<EntryReport>, u64), Failure> {
        let limits = &self.policy.limits;
        if bytes.is_empty() {
            return Err(fail(Code::Empty, "empty file"));
        }
        if bytes.len() as u64 > limits.max_input_bytes {
            return Err(fail(
                Code::Oversized,
                format!("file exceeds {} bytes", limits.max_input_bytes),
            ));
        }
        if self.policy.require_scanner && self.scanner.is_none() {
            return Err(fail(Code::ScannerRequired, "no scanner is configured"));
        }
        let declared = detect::expectation_for_media_type(declared_type);
        if declared == Expectation::Text && !detect::is_text(bytes) {
            return Err(fail(Code::InvalidText, "text file is not valid UTF-8"));
        }
        if detected.is_executable() {
            return Err(fail(
                Code::Executable,
                format!("executable content ({detected}) is never accepted"),
            ));
        }
        let mut flags = Vec::new();
        let mut mismatch = |what: String| -> Result<(), Failure> {
            match self.policy.mismatch {
                MismatchPolicy::Reject => Err(fail(Code::TypeMismatch, what)),
                MismatchPolicy::Flag => {
                    flags.push(format!("type_mismatch: {what}"));
                    Ok(())
                }
            }
        };
        if let Some(conflict) = declared.conflict(detected) {
            mismatch(format!("declared {declared_type}: {conflict}"))?;
        }
        if let Some(name) = name
            && let Some(conflict) =
                detect::expectation_for_name(name).and_then(|e| e.conflict(detected))
        {
            mismatch(format!("name {name:?}: {conflict}"))?;
        }
        let mut walk = Walk {
            guard: self,
            budget: Budget {
                limits,
                input_bytes: bytes.len() as u64,
                expanded: 0,
                entries: 0,
            },
            flags,
            entries: Vec::new(),
        };
        walk.scan(name, bytes)?;
        if detected.is_archive() {
            walk.expand(bytes, detected, 1)?;
        }
        Ok((walk.flags, walk.entries, walk.budget.expanded))
    }
}

impl Walk<'_> {
    fn scan(&self, path: Option<&str>, bytes: &[u8]) -> Result<(), Failure> {
        let Some(scanner) = &self.guard.scanner else {
            return Ok(());
        };
        let label = path.unwrap_or("<file>");
        match scanner.scan(path, bytes) {
            ScanVerdict::Clean => Ok(()),
            ScanVerdict::Infected { signature } => Err(fail(
                Code::ScannerHit,
                format!("{} found {signature} in {label:?}", scanner.name()),
            )),
            ScanVerdict::Failed { reason } => Err(fail(
                Code::ScannerFailed,
                format!("{} could not scan {label:?}: {reason}", scanner.name()),
            )),
        }
    }

    fn expand(&mut self, bytes: &[u8], kind: Kind, depth: u32) -> Result<(), Failure> {
        let max = self.budget.limits.max_depth;
        if depth > max {
            return Err(fail(
                Code::DepthLimit,
                format!("archive nesting exceeds {max} levels"),
            ));
        }
        let entries: Vec<Entry> = match kind {
            Kind::Zip => archive::unzip(bytes, &mut self.budget)?,
            Kind::Gzip => archive::gunzip(bytes, &mut self.budget)?,
            Kind::Zstd => archive::unzstd(bytes, &mut self.budget)?,
            Kind::Tar => archive::untar(bytes, &mut self.budget)?,
            _ => return Ok(()),
        };
        for entry in entries {
            let detected = if entry.directory {
                Kind::Binary
            } else {
                detect(&entry.bytes)
            };
            if entry.executable_mode {
                self.flags.push(format!(
                    "executable_mode_ignored: {:?} at depth {depth}",
                    entry.path
                ));
            }
            self.entries.push(EntryReport {
                path: entry.path.clone(),
                depth,
                size: entry.bytes.len() as u64,
                detected,
                directory: entry.directory,
                executable_mode_ignored: entry.executable_mode,
            });
            if entry.directory {
                continue;
            }
            if detected.is_executable() {
                return Err(fail(
                    Code::Executable,
                    format!(
                        "archive entry {:?} contains executable content ({detected})",
                        entry.path
                    ),
                ));
            }
            if let Some(conflict) =
                detect::expectation_for_name(&entry.path).and_then(|e| e.conflict(detected))
            {
                match self.guard.policy.mismatch {
                    MismatchPolicy::Reject => {
                        return Err(fail(
                            Code::TypeMismatch,
                            format!("archive entry {:?}: {conflict}", entry.path),
                        ));
                    }
                    MismatchPolicy::Flag => self
                        .flags
                        .push(format!("type_mismatch: entry {:?}: {conflict}", entry.path)),
                }
            }
            self.scan(Some(&entry.path), &entry.bytes)?;
            if detected.is_archive() {
                self.expand(&entry.bytes, detected, depth + 1)?;
            }
        }
        Ok(())
    }
}

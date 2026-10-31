//! Pluggable malware scanner hook (architecture 14.1 `file-scanner`, 20.7 step 3).
//!
//! The crate never starts an antivirus engine or opens a network connection. Hosts supply a
//! [`Scanner`] (for example an adapter over a local ClamAV socket); tests use
//! [`SignatureScanner`], a deterministic byte-pattern matcher.

/// Result of scanning one blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanVerdict {
    Clean,
    Infected { signature: String },
    /// The scanner could not decide. The guard fails closed on this verdict.
    Failed { reason: String },
}

pub trait Scanner: Send + Sync {
    /// Stable identifier recorded in reports.
    fn name(&self) -> &str;
    /// Scans one blob. `path` is the archive entry path or the original name, if any.
    fn scan(&self, path: Option<&str>, bytes: &[u8]) -> ScanVerdict;
}

/// Deterministic scanner matching literal byte signatures anywhere in the blob.
#[derive(Clone, Debug, Default)]
pub struct SignatureScanner {
    name: String,
    signatures: Vec<(String, Vec<u8>)>,
}

impl SignatureScanner {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            signatures: Vec::new(),
        }
    }
    pub fn with_signature(mut self, label: impl Into<String>, pattern: impl Into<Vec<u8>>) -> Self {
        let pattern = pattern.into();
        if !pattern.is_empty() {
            self.signatures.push((label.into(), pattern));
        }
        self
    }
    /// Scanner that recognises only the industry-standard EICAR antivirus test string.
    pub fn eicar() -> Self {
        Self::new("signature-mock").with_signature("EICAR-Test-File", eicar_test_string())
    }
}

impl Scanner for SignatureScanner {
    fn name(&self) -> &str {
        &self.name
    }
    fn scan(&self, _path: Option<&str>, bytes: &[u8]) -> ScanVerdict {
        for (label, pattern) in &self.signatures {
            if bytes.windows(pattern.len()).any(|w| w == pattern.as_slice()) {
                return ScanVerdict::Infected {
                    signature: label.clone(),
                };
            }
        }
        ScanVerdict::Clean
    }
}

/// The 68-byte EICAR antivirus test string. It is harmless by design; it is assembled from
/// two halves so that the source file itself does not contain the contiguous signature.
pub fn eicar_test_string() -> Vec<u8> {
    let mut s = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-".to_vec();
    s.extend_from_slice(b"ANTIVIRUS-TEST-FILE!$H+H*");
    s
}

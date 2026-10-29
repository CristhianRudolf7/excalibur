use crate::SecretValue;
use regex::bytes::Regex;
use serde_json::Value;
use std::sync::{Arc, RwLock};
use zeroize::Zeroizing;

const REPLACEMENT: &[u8] = b"[REDACTED]";

/// Shared, live redaction registry. Exact registered bytes and known credential
/// formats are removed before storage. This does not detect every possible
/// encoding or an unknown credential format.
#[derive(Clone)]
pub struct Redactor {
    secrets: Arc<RwLock<Vec<Zeroizing<Vec<u8>>>>>,
    patterns: Arc<Vec<Regex>>,
}
impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Redactor([REDACTED])")
    }
}
impl Default for Redactor {
    fn default() -> Self {
        let patterns = [
            r"(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})",
            r"(?:sk-(?:proj-|ant-)?[A-Za-z0-9_-]{16,}|AKIA[A-Z0-9]{16}|ASIA[A-Z0-9]{16})",
            r"(?i)(?:authorization\s*[:=]\s*(?:bearer|basic)\s+)[^\r\n]+",
            r#"(?i)(?:[\"']?(?:api[_-]?key|access[_-]?token|refresh[_-]?token|password|client[_-]?secret|secret[_-]?key)[\"']?\s*[:=]\s*)[^\r\n,}]+"#,
            r"(?s)-----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY-----.*?(?:-----END (?:[A-Z0-9 ]+ )?PRIVATE KEY-----|\z)",
        ].into_iter().map(|p| Regex::new(p).expect("static redaction pattern")).collect();
        Self {
            secrets: Arc::default(),
            patterns: Arc::new(patterns),
        }
    }
}
impl Redactor {
    /// Identity check for host adapters that must share live secret updates.
    pub fn same_registry(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.secrets, &other.secrets)
    }
    pub fn with_secrets(secrets: impl IntoIterator<Item = SecretValue>) -> Self {
        let redactor = Self::default();
        for secret in secrets {
            redactor.register_secret(&secret);
        }
        redactor
    }
    /// Retains old values until this registry is dropped, protecting in-flight
    /// output after update/delete. Its owned values are zeroized on drop.
    pub fn register_secret(&self, secret: &SecretValue) {
        let mut secrets = self.secrets.write().unwrap_or_else(|e| e.into_inner());
        secret.expose(|bytes| {
            if !secrets.iter().any(|old| old.as_slice() == bytes) {
                secrets.push(Zeroizing::new(bytes.to_vec()));
            }
        });
    }
    pub fn redact_text(&self, text: &str) -> String {
        String::from_utf8_lossy(&self.redact_bytes(text.as_bytes())).into_owned()
    }
    pub fn redact_json(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.redact_text(s)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|v| self.redact_json(v)).collect())
            }
            Value::Object(items) => Value::Object(
                items
                    .iter()
                    .map(|(k, v)| (self.redact_text(k), self.redact_json(v)))
                    .collect(),
            ),
            other => {
                let encoded = other.to_string();
                if self.contains_secret(encoded.as_bytes()) {
                    Value::String("[REDACTED]".into())
                } else {
                    other.clone()
                }
            }
        }
    }
    pub fn contains_secret(&self, bytes: &[u8]) -> bool {
        !self.spans(bytes).is_empty()
    }
    pub fn redact_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        let spans = self.spans(bytes);
        if spans.is_empty() {
            return bytes.to_vec();
        }
        let mut output = Vec::with_capacity(bytes.len());
        let mut position = 0;
        for (start, end) in spans {
            output.extend_from_slice(&bytes[position..start]);
            output.extend_from_slice(REPLACEMENT);
            position = end;
        }
        output.extend_from_slice(&bytes[position..]);
        output
    }
    fn spans(&self, bytes: &[u8]) -> Vec<(usize, usize)> {
        let mut spans = Vec::new();
        for secret in self
            .secrets
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            if bytes.len() >= secret.len() {
                for (start, window) in bytes.windows(secret.len()).enumerate() {
                    if window == secret.as_slice() {
                        spans.push((start, start + secret.len()));
                    }
                }
            }
        }
        for pattern in self.patterns.iter() {
            spans.extend(
                pattern
                    .find_iter(bytes)
                    .filter(|m| !reference_only(m.as_bytes()))
                    .map(|m| (m.start(), m.end())),
            );
        }
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in spans {
            if let Some(last) = merged.last_mut()
                && start <= last.1
            {
                last.1 = last.1.max(end);
                continue;
            }
            merged.push((start, end));
        }
        merged
    }
    pub fn stream(&self) -> StreamRedactor {
        StreamRedactor {
            redactor: self.clone(),
            pending: Zeroizing::new(Vec::new()),
            exact_pending: Zeroizing::new(Vec::new()),
            mode: Mode::Normal,
        }
    }
}

// References are safe metadata, including in Authorization/password fields.
// Extra content after a reference invalidates this exemption.
fn reference_only(bytes: &[u8]) -> bool {
    static REFERENCE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    REFERENCE.get_or_init(|| Regex::new(r#"(?i)^(?:authorization\s*[:=]\s*(?:bearer|basic)\s+|[\"']?(?:api[_-]?key|access[_-]?token|refresh[_-]?token|password|client[_-]?secret|secret[_-]?key)[\"']?\s*[:=]\s*)[\"']?secret://[A-Za-z0-9_.-]{1,128}[\"']?\s*$"#).expect("static reference pattern")).is_match(bytes)
}

// Prefixes are held only while they can become a known credential marker.
const LABELS: &[&[u8]] = &[
    b"authorization",
    b"api_key",
    b"api-key",
    b"apikey",
    b"access_token",
    b"access-token",
    b"accesstoken",
    b"refresh_token",
    b"refresh-token",
    b"refreshtoken",
    b"password",
    b"client_secret",
    b"client-secret",
    b"clientsecret",
    b"secret_key",
    b"secret-key",
    b"secretkey",
];
const TOKENS: &[(&[u8], usize)] = &[
    (b"ghp_", 24),
    (b"gho_", 24),
    (b"ghu_", 24),
    (b"ghs_", 24),
    (b"ghr_", 24),
    (b"github_pat_", 31),
    (b"sk-", 19),
    (b"AKIA", 20),
    (b"ASIA", 20),
];
const BEGIN: &[u8] = b"-----BEGIN ";

enum Mode {
    Normal,
    AfterLabel {
        auth: bool,
    },
    BeforeValue {
        auth: bool,
    },
    Probe {
        auth: bool,
        bytes: Zeroizing<Vec<u8>>,
    },
    Reference {
        auth: bool,
        bytes: Zeroizing<Vec<u8>>,
        name_start: usize,
    },
    AfterReference {
        auth: bool,
    },
    Value {
        auth: bool,
    },
    Token {
        bytes: Zeroizing<Vec<u8>>,
        minimum: usize,
        masked: bool,
    },
    PemHeader(Zeroizing<Vec<u8>>),
    Pem(Zeroizing<Vec<u8>>),
}

/// Incremental redaction with bounded retained prefixes, preserving arbitrary
/// nonsecret output and prompts without newlines. Credential values/private-key
/// bodies are suppressed once identified, without buffering their whole length.
/// Exact-value matching runs after pattern filtering and shares live updates.
pub struct StreamRedactor {
    redactor: Redactor,
    pending: Zeroizing<Vec<u8>>,
    exact_pending: Zeroizing<Vec<u8>>,
    mode: Mode,
}
impl std::fmt::Debug for StreamRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamRedactor([REDACTED])")
    }
}
impl StreamRedactor {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut filtered = Zeroizing::new(Vec::with_capacity(bytes.len()));
        let mut position = 0;
        while position < bytes.len() {
            if matches!(self.mode, Mode::Normal) && self.pending.is_empty() {
                // Bulk-copy bytes that cannot begin any supported format.
                // Exact-value matching still examines this entire safe run.
                let safe = bytes[position..]
                    .iter()
                    .position(|byte| {
                        matches!(
                            byte.to_ascii_lowercase(),
                            b'a' | b'p' | b'r' | b'c' | b's' | b'g' | b'-'
                        )
                    })
                    .unwrap_or(bytes.len() - position);
                filtered.extend_from_slice(&bytes[position..position + safe]);
                position += safe;
                if position == bytes.len() {
                    break;
                }
            }
            self.byte(bytes[position], &mut filtered);
            position += 1;
        }
        self.exact(&filtered, false)
    }
    fn byte(&mut self, byte: u8, output: &mut Vec<u8>) {
        loop {
            let mode = std::mem::replace(&mut self.mode, Mode::Normal);
            match mode {
                Mode::Normal => {
                    self.pending.push(byte);
                    if let Some(label) = LABELS
                        .iter()
                        .find(|label| self.pending.eq_ignore_ascii_case(label))
                    {
                        output.extend_from_slice(&self.pending);
                        self.pending.clear();
                        self.mode = Mode::AfterLabel {
                            auth: label.eq_ignore_ascii_case(b"authorization"),
                        };
                        return;
                    }
                    if let Some((_, minimum)) = TOKENS
                        .iter()
                        .find(|(prefix, _)| self.pending.as_slice() == *prefix)
                    {
                        self.mode = Mode::Token {
                            bytes: std::mem::replace(&mut self.pending, Zeroizing::new(Vec::new())),
                            minimum: *minimum,
                            masked: false,
                        };
                        return;
                    }
                    if self.pending.as_slice() == BEGIN {
                        self.mode = Mode::PemHeader(std::mem::replace(
                            &mut self.pending,
                            Zeroizing::new(Vec::new()),
                        ));
                        return;
                    }
                    let keep = (1..=self.pending.len())
                        .rev()
                        .find(|&n| {
                            let tail = &self.pending[self.pending.len() - n..];
                            LABELS.iter().any(|prefix| {
                                n < prefix.len() && prefix[..n].eq_ignore_ascii_case(tail)
                            }) || TOKENS
                                .iter()
                                .any(|(prefix, _)| n < prefix.len() && prefix[..n] == *tail)
                                || n < BEGIN.len() && BEGIN[..n] == *tail
                        })
                        .unwrap_or(0);
                    let emit = self.pending.len() - keep;
                    output.extend_from_slice(&self.pending[..emit]);
                    self.pending.drain(..emit);
                    return;
                }
                Mode::AfterLabel { auth } => {
                    if byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\'') {
                        output.push(byte);
                        self.mode = Mode::AfterLabel { auth };
                        return;
                    }
                    if matches!(byte, b':' | b'=') {
                        output.push(byte);
                        self.mode = Mode::BeforeValue { auth };
                        return;
                    }
                }
                Mode::BeforeValue { auth } => {
                    if matches!(byte, b' ' | b'\t' | b'"' | b'\'') {
                        output.push(byte);
                        self.mode = Mode::BeforeValue { auth };
                        return;
                    }
                    if matches!(byte, b'\n' | b'\r') {
                        output.push(byte);
                        return;
                    }
                    self.mode = Mode::Probe {
                        auth,
                        bytes: Zeroizing::new(Vec::new()),
                    };
                    continue;
                }
                Mode::Probe { auth, mut bytes } => {
                    bytes.push(byte);
                    let options: &[&[u8]] = if auth {
                        &[b"secret://", b"bearer secret://", b"basic secret://"]
                    } else {
                        &[b"secret://"]
                    };
                    if let Some(prefix) = options.iter().find(|p| p.eq_ignore_ascii_case(&bytes)) {
                        self.mode = Mode::Reference {
                            auth,
                            name_start: prefix.len(),
                            bytes,
                        };
                        return;
                    }
                    if options.iter().any(|p| {
                        bytes.len() < p.len() && p[..bytes.len()].eq_ignore_ascii_case(&bytes)
                    }) {
                        self.mode = Mode::Probe { auth, bytes };
                        return;
                    }
                    output.extend_from_slice(REPLACEMENT);
                    self.mode = Mode::Value { auth };
                    continue;
                }
                Mode::Reference {
                    auth,
                    mut bytes,
                    name_start,
                } => {
                    if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
                        bytes.push(byte);
                        if bytes.len() - name_start <= 128 {
                            self.mode = Mode::Reference {
                                auth,
                                bytes,
                                name_start,
                            };
                            return;
                        }
                        output.extend_from_slice(REPLACEMENT);
                        self.mode = Mode::Value { auth };
                        return;
                    }
                    if bytes.len() > name_start {
                        output.extend_from_slice(&bytes);
                        self.mode = Mode::AfterReference { auth };
                        continue;
                    }
                    output.extend_from_slice(REPLACEMENT);
                    self.mode = Mode::Value { auth };
                    continue;
                }
                Mode::AfterReference { auth } => {
                    if matches!(byte, b' ' | b'\t') {
                        output.push(byte);
                        self.mode = Mode::AfterReference { auth };
                        return;
                    }
                    if matches!(byte, b'"' | b'\'' | b',' | b'}' | b'\n' | b'\r') {
                        output.push(byte);
                        return;
                    }
                    output.extend_from_slice(REPLACEMENT);
                    self.mode = Mode::Value { auth };
                    continue;
                }
                Mode::Value { auth } => {
                    if matches!(byte, b'\n' | b'\r') || !auth && matches!(byte, b',' | b'}') {
                        output.push(byte);
                        return;
                    }
                    self.mode = Mode::Value { auth };
                    return;
                }
                Mode::Token {
                    mut bytes,
                    minimum,
                    mut masked,
                } => {
                    if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') {
                        if !masked {
                            bytes.push(byte);
                            if bytes.len() >= minimum {
                                output.extend_from_slice(REPLACEMENT);
                                bytes.clear();
                                masked = true;
                            }
                        }
                        self.mode = Mode::Token {
                            bytes,
                            minimum,
                            masked,
                        };
                        return;
                    }
                    if !masked {
                        output.extend_from_slice(&bytes);
                    }
                }
                Mode::PemHeader(mut bytes) => {
                    bytes.push(byte);
                    if bytes.ends_with(b"PRIVATE KEY-----") || bytes.len() > 128 {
                        output.extend_from_slice(REPLACEMENT);
                        self.mode = Mode::Pem(Zeroizing::new(Vec::new()));
                        return;
                    }
                    if matches!(byte, b'\n' | b'\r') {
                        output.extend_from_slice(&bytes);
                        return;
                    }
                    self.mode = Mode::PemHeader(bytes);
                    return;
                }
                Mode::Pem(mut tail) => {
                    tail.push(byte);
                    if tail.len() > 128 {
                        tail.remove(0);
                    }
                    if tail.ends_with(b"PRIVATE KEY-----")
                        && tail.windows(9).any(|w| w == b"-----END ")
                    {
                        return;
                    }
                    self.mode = Mode::Pem(tail);
                    return;
                }
            }
        }
    }
    fn exact(&mut self, bytes: &[u8], finish: bool) -> Vec<u8> {
        let mut output = Vec::with_capacity(bytes.len());
        for chunk in bytes.chunks(64 * 1024) {
            self.exact_pending.extend_from_slice(chunk);
            self.exact_flush(false, &mut output);
        }
        if finish {
            self.exact_flush(true, &mut output);
        }
        output
    }
    fn exact_flush(&mut self, finish: bool, output: &mut Vec<u8>) {
        let secrets = self
            .redactor
            .secrets
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let bytes = &self.exact_pending;
        let keep = if finish {
            0
        } else {
            secrets
                .iter()
                .flat_map(|secret| {
                    (1..secret.len().min(bytes.len() + 1))
                        .rev()
                        .find(|&n| bytes.ends_with(&secret[..n]))
                })
                .max()
                .unwrap_or(0)
        };
        let mut emit = bytes.len() - keep;
        let mut spans = Vec::new();
        for secret in secrets.iter() {
            if secret.len() <= bytes.len() {
                for (start, window) in bytes.windows(secret.len()).enumerate() {
                    if window == secret.as_slice() {
                        spans.push((start, start + secret.len()));
                    }
                }
            }
        }
        spans.sort_unstable();
        // Never split an already recognized full match across an emitted prefix.
        for &(start, end) in &spans {
            if start < emit && end > emit {
                emit = start;
            }
        }
        let mut position = 0;
        for (start, end) in spans {
            if end > emit {
                continue;
            }
            if start < position {
                position = position.max(end);
                continue;
            }
            output.extend_from_slice(&bytes[position..start]);
            output.extend_from_slice(REPLACEMENT);
            position = end;
        }
        output.extend_from_slice(&bytes[position..emit]);
        self.exact_pending.drain(..emit);
    }
    pub fn finish(&mut self) -> Vec<u8> {
        let mut filtered = Zeroizing::new(Vec::new());
        filtered.extend_from_slice(&self.pending);
        self.pending.clear();
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Probe { .. } => filtered.extend_from_slice(REPLACEMENT),
            Mode::Reference {
                bytes, name_start, ..
            } if bytes.len() > name_start => filtered.extend_from_slice(&bytes),
            Mode::Reference { .. } => filtered.extend_from_slice(REPLACEMENT),
            Mode::Token {
                bytes,
                masked: false,
                ..
            }
            | Mode::PemHeader(bytes) => filtered.extend_from_slice(&bytes),
            _ => {}
        }
        self.exact(&filtered, true)
    }
}

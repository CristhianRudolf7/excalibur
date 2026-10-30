//! Exact and near-duplicate detection (section 15.4).
//!
//! Exact duplicates use `sha256` over normalized Markdown. Near duplicates
//! use a 64-bit SimHash over 3-word shingles with Hamming distance <= 3
//! inside the same host, the operating point reported for billions of pages.

use sha2::{Digest, Sha256};

/// Hex `sha256` of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes.as_ref());
    hex_encode(hasher.finalize())
}

/// Normalize page Markdown before hashing: trim trailing whitespace per line
/// and collapse blank runs (case is preserved).
#[must_use]
pub fn normalize_markdown(text: &str) -> String {
    let mut lines = Vec::new();
    let mut blank_run = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            if !blank_run {
                lines.push(String::new());
            }
            blank_run = true;
        } else {
            blank_run = false;
            lines.push(trimmed.to_owned());
        }
    }
    while lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Exact-duplicate key for page Markdown.
#[must_use]
pub fn content_hash(markdown: &str) -> String {
    sha256_hex(normalize_markdown(markdown).as_bytes())
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hash_word(word: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in word.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash ^ (word.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// 64-bit SimHash over 3-word shingles of tokenized text.
#[must_use]
pub fn simhash64(tokens: &[String]) -> u64 {
    if tokens.is_empty() {
        return 0;
    }
    let mut acc = [0_i32; 64];
    let window = tokens.len().min(3);
    if tokens.len() < 3 {
        let combined = tokens.join(" ");
        let digest = hash_word(&combined);
        return mix_single(digest);
    }
    for start in 0..=tokens.len() - window {
        let mut shingle = String::new();
        for token in &tokens[start..start + window] {
            if !shingle.is_empty() {
                shingle.push(' ');
            }
            shingle.push_str(token);
        }
        let digest = hash_word(&shingle);
        for (bit, slot) in acc.iter_mut().enumerate() {
            if digest >> bit & 1 == 1 {
                *slot += 1;
            } else {
                *slot -= 1;
            }
        }
    }
    let mut out = 0_u64;
    for (bit, slot) in acc.iter().enumerate() {
        if *slot > 0 {
            out |= 1 << bit;
        }
    }
    out
}

fn mix_single(digest: u64) -> u64 {
    digest
        .wrapping_mul(0xff51_afd7_ed55_8ccd)
        .wrapping_add(0xc4ce_b9fe_1a85_ec53)
}

/// Hamming distance between two SimHash values.
#[must_use]
pub fn hamming_distance(left: u64, right: u64) -> u32 {
    (left ^ right).count_ones()
}

/// Near-duplicate threshold inside one host.
pub const SIMHASH_HAMMING_THRESHOLD: u32 = 3;

/// Whether two same-host pages are near duplicates.
#[must_use]
pub fn is_near_duplicate(left: u64, right: u64) -> bool {
    hamming_distance(left, right) <= SIMHASH_HAMMING_THRESHOLD
}

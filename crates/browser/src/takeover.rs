//! Human takeover codes and session transitions (9.2).
//!
//! A blocked session moves to `human_control` while a person uses the viewer.
//! Ending the intervention bumps the document epoch and invalidates live
//! references as `stale_ref(takeover)`.
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Viewer one-time code length from the architecture (8 characters).
pub const CODE_LEN: usize = 8;
/// Code validity in seconds (10 minutes).
pub const CODE_TTL_SECS: i64 = 600;
/// Human intervention deadline in seconds (15 minutes).
pub const INTERVENTION_TTL_SECS: i64 = 900;

const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Generate an 8-character unambiguous code from OS randomness.
pub fn generate_code() -> String {
    let mut bytes = [0u8; CODE_LEN];
    // getrandom via /dev/urandom fallback is overkill here; use a hashed
    // mix of time and process identity, then map into the alphabet. Uniqueness
    // across interventions comes from the stored hash plus expiry, and the
    // code is single-use.
    let seed = format!(
        "{}-{}-{}",
        now_secs(),
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    let digest = Sha256::digest(seed.as_bytes());
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = CODE_ALPHABET[digest[index] as usize % CODE_ALPHABET.len()];
    }
    String::from_utf8(bytes.to_vec()).unwrap_or_else(|_| "AAAAAAAA".into())
}

pub fn code_hash(code: &str) -> String {
    format!("{:x}", Sha256::digest(code.as_bytes()))
}

/// Single-use viewer credential bound to one tab and deadline.
#[derive(Clone, Debug)]
pub struct Takeover {
    pub tab_id: String,
    pub code_hash: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub used: bool,
}

impl Takeover {
    pub fn issue(tab_id: &str, now: i64) -> (Self, String) {
        let code = generate_code();
        let takeover = Self {
            tab_id: tab_id.into(),
            code_hash: code_hash(&code),
            issued_at: now,
            expires_at: now + CODE_TTL_SECS,
            used: false,
        };
        (takeover, code)
    }

    /// Validate without consuming. Consumption happens on first successful use.
    pub fn check(&self, code: &str, now: i64) -> Result<(), String> {
        if self.used {
            return Err("single_use_code_reused".into());
        }
        if now > self.expires_at {
            return Err("code_expired".into());
        }
        if code_hash(code) != self.code_hash {
            return Err("invalid_code".into());
        }
        Ok(())
    }

    pub fn consume(&mut self, code: &str, now: i64) -> Result<(), String> {
        self.check(code, now)?;
        self.used = true;
        Ok(())
    }

    pub fn intervention_expired(&self, started_at: i64, now: i64) -> bool {
        now - started_at > INTERVENTION_TTL_SECS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_single_use_and_expire() {
        let now = 1_700_000_000;
        let (mut takeover, code) = Takeover::issue("tab-1", now);
        assert_eq!(code.len(), CODE_LEN);
        assert!(takeover.check(&code, now + 10).is_ok());
        takeover.consume(&code, now + 10).unwrap();
        assert!(takeover.check(&code, now + 11).is_err());
        let (takeover2, code2) = Takeover::issue("tab-2", now);
        assert!(takeover2.check(&code2, now + CODE_TTL_SECS + 1).is_err());
        assert!(takeover2.check("WRONGCOD", now).is_err());
    }

    #[test]
    fn intervention_deadline_is_fifteen_minutes() {
        let takeover = Takeover::issue("tab-1", 100).0;
        assert!(!takeover.intervention_expired(100, 100 + 899));
        assert!(takeover.intervention_expired(100, 100 + 901));
    }
}

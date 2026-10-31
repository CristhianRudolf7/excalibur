//! Cryptographic primitives. Nothing here logs or formats secret material.
use crate::{IdentityError, PasswordPolicy};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

pub(crate) const HASH_LEN: usize = 32;
pub(crate) const SALT_LEN: usize = 16;
const SECRET_LEN: usize = 32;
/// Crockford base32 without I, L, O and U.
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
pub(crate) const CODE_LEN: usize = 10;

pub(crate) fn random<const N: usize>() -> Result<[u8; N], IdentityError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|_| IdentityError::Randomness)?;
    Ok(bytes)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

/// Opaque identifier with a type prefix, e.g. `prn_` followed by 32 hex digits.
pub(crate) fn new_id(prefix: &str) -> Result<String, IdentityError> {
    Ok(format!("{prefix}_{}", hex(&random::<16>()?)))
}

pub(crate) fn valid_id(prefix: &str, id: &str) -> bool {
    id.len() == prefix.len() + 33
        && id.starts_with(prefix)
        && id.as_bytes()[prefix.len()] == b'_'
        && id[prefix.len() + 1..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn sha256(parts: &[&[u8]]) -> [u8; HASH_LEN] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// A bearer token `<kind>1.<id>.<secret>`. Only SHA-256 of the 256-bit secret is
/// stored; lookup is by id and the digest comparison is constant-time.
pub(crate) struct MintedToken {
    pub id: String,
    pub secret_hash: [u8; HASH_LEN],
    pub token: Zeroizing<String>,
}

pub(crate) fn mint_token(kind: &str, id_prefix: &str) -> Result<MintedToken, IdentityError> {
    let id = new_id(id_prefix)?;
    let secret = Zeroizing::new(random::<SECRET_LEN>()?);
    let encoded = Zeroizing::new(URL_SAFE_NO_PAD.encode(*secret));
    let secret_hash = sha256(&[kind.as_bytes(), encoded.as_bytes()]);
    let token = Zeroizing::new(format!("{kind}1.{id}.{}", encoded.as_str()));
    Ok(MintedToken {
        id,
        secret_hash,
        token,
    })
}

/// Parses a bearer token, returning its id and the digest of its secret part.
pub(crate) fn parse_token(kind: &str, id_prefix: &str, token: &str) -> Option<(String, [u8; 32])> {
    if token.len() > 256 {
        return None;
    }
    let rest = token.strip_prefix(kind)?.strip_prefix("1.")?;
    let (id, secret) = rest.split_once('.')?;
    if !valid_id(id_prefix, id) || secret.len() != 43 {
        return None;
    }
    let decoded = Zeroizing::new(URL_SAFE_NO_PAD.decode(secret).ok()?);
    if decoded.len() != SECRET_LEN {
        return None;
    }
    Some((id.to_owned(), sha256(&[kind.as_bytes(), secret.as_bytes()])))
}

pub(crate) fn new_code() -> Result<Zeroizing<String>, IdentityError> {
    let bytes = Zeroizing::new(random::<CODE_LEN>()?);
    let mut code = Zeroizing::new(String::with_capacity(CODE_LEN));
    for b in bytes.iter() {
        code.push(CODE_ALPHABET[(*b & 31) as usize] as char);
    }
    Ok(code)
}

/// Canonical form accepted for typed codes: case-insensitive, hyphens and spaces
/// ignored, and the Crockford confusables O->0, I/L->1.
pub(crate) fn normalize_code(input: &str) -> Option<Zeroizing<String>> {
    if input.len() > 64 {
        return None;
    }
    let mut out = Zeroizing::new(String::with_capacity(CODE_LEN));
    for c in input.chars() {
        let c = match c.to_ascii_uppercase() {
            '-' | ' ' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        if !c.is_ascii() || !CODE_ALPHABET.contains(&(c as u8)) {
            return None;
        }
        out.push(c);
    }
    (out.len() == CODE_LEN).then_some(out)
}

pub(crate) fn code_hash(purpose: &str, normalized: &str) -> [u8; HASH_LEN] {
    sha256(&[
        b"excalibur-identity/v1/code",
        purpose.as_bytes(),
        normalized.as_bytes(),
    ])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Argon2Cost {
    pub m: u32,
    pub t: u32,
    pub p: u32,
}

pub(crate) fn argon2_hash(
    password: &[u8],
    salt: &[u8],
    cost: Argon2Cost,
) -> Result<Zeroizing<[u8; HASH_LEN]>, IdentityError> {
    let params = Params::new(cost.m, cost.t, cost.p, Some(HASH_LEN))
        .map_err(|_| IdentityError::Invalid("argon2 parameters"))?;
    let mut out = Zeroizing::new([0u8; HASH_LEN]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password, salt, out.as_mut())
        .map_err(|_| IdentityError::Invalid("argon2 input"))?;
    Ok(out)
}

pub(crate) fn policy_cost(policy: &PasswordPolicy) -> Argon2Cost {
    Argon2Cost {
        m: policy.memory_kib,
        t: policy.iterations,
        p: policy.parallelism,
    }
}

pub(crate) fn verify_device_signature(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(message, signature)
        .is_ok()
}

/// The exact bytes a device signs to prove possession of its key for a session.
pub fn device_proof_message(challenge_id: &str, nonce: &[u8], device_id: &str) -> Vec<u8> {
    let mut message = b"excalibur-identity/v1/device-session\0".to_vec();
    for part in [challenge_id.as_bytes(), nonce, device_id.as_bytes()] {
        message.extend_from_slice(&(part.len() as u32).to_be_bytes());
        message.extend_from_slice(part);
    }
    message
}

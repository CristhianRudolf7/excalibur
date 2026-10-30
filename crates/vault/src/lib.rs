//! Encrypted host-side secret storage. Only metadata and `secret://` references
//! belong in model input. SecretValue deliberately has no serde implementation.
//! Native key stores are explicit: tests never access a real operating-system store.

mod native;
mod redaction;

use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use fs2::FileExt;
pub use native::NativeKeyStore;
pub use redaction::{Redactor, StreamRedactor};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroize::Zeroizing;

pub const MAX_SECRET_BYTES: usize = 64 * 1024;
const MAX_VAULT_BYTES: u64 = 96 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("invalid secret reference")]
    InvalidReference,
    #[error("invalid secret metadata")]
    InvalidMetadata,
    #[error("secret must contain 1 to 65536 bytes")]
    InvalidSecretSize,
    #[error("secret does not exist")]
    NotFound,
    #[error("secret deletion requires matching host approval")]
    ApprovalRequired,
    #[error("operating-system key store unavailable")]
    KeyStoreUnavailable,
    #[error("vault master key missing or invalid")]
    InvalidMasterKey,
    #[error("vault authentication failed")]
    AuthenticationFailed,
    #[error("vault format is invalid or unsupported")]
    InvalidFormat,
    #[error("vault storage operation failed")]
    Storage,
    #[error("operating-system randomness unavailable")]
    Randomness,
    #[error("vault capacity exceeded")]
    Capacity,
}

/// A host-provided secret. Debug is redacted and drop erases its owned allocation.
/// Construct this only from the client's secure input channel, never tool/chat JSON.
#[derive(Clone)]
pub struct SecretValue(Zeroizing<Vec<u8>>);
impl SecretValue {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, VaultError> {
        let bytes = Zeroizing::new(bytes.into());
        if bytes.is_empty() || bytes.len() > MAX_SECRET_BYTES {
            return Err(VaultError::InvalidSecretSize);
        }
        Ok(Self(bytes))
    }
    /// Trusted executor boundary. The closure must not publish the borrowed value.
    pub fn expose<T>(&self, use_secret: impl FnOnce(&[u8]) -> T) -> T {
        use_secret(&self.0)
    }
}
impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SecretRef(String);
impl SecretRef {
    pub fn parse(reference: impl Into<String>) -> Result<Self, VaultError> {
        let reference = reference.into();
        let name = reference
            .strip_prefix("secret://")
            .ok_or(VaultError::InvalidReference)?;
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(VaultError::InvalidReference);
        }
        Ok(Self(reference))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    ApiKey,
    WebsitePassword,
    Host,
    Token,
    SshKey,
    ModelProviderCredential,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretMetadata {
    pub reference: SecretRef,
    pub name: String,
    pub kind: SecretKind,
    pub usage: String,
    pub url: Option<String>,
}
impl SecretMetadata {
    fn validate(&self) -> Result<(), VaultError> {
        if self.name.is_empty() || self.name.len() > 256 || self.usage.len() > 2048 {
            return Err(VaultError::InvalidMetadata);
        }
        if self.kind == SecretKind::WebsitePassword && self.url.is_none() {
            return Err(VaultError::InvalidMetadata);
        }
        if let Some(url) = &self.url
            && (url.len() > 2048
                || !(url.starts_with("https://") || url.starts_with("http://"))
                || url
                    .split('/')
                    .nth(2)
                    .is_none_or(|h| h.is_empty() || h.contains('@')))
        {
            return Err(VaultError::InvalidMetadata);
        }
        Ok(())
    }
}

/// Host authorization, bound to exactly one reference. The host must authenticate
/// approval with its security policy before constructing this capability.
#[derive(Debug)]
pub struct DeleteApproval {
    reference: SecretRef,
}
impl DeleteApproval {
    pub fn from_approved_host(reference: SecretRef) -> Self {
        Self { reference }
    }
}

/// Adapter boundary for OS-protected keys. Implementations must never fall back to
/// plaintext files. Calls are serialized by the vault's interprocess file lock.
pub trait MasterKeyStore: Send + Sync {
    fn load(&self, id: &str) -> Result<Option<SecretValue>, VaultError>;
    fn store(&self, id: &str, key: &SecretValue) -> Result<(), VaultError>;
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    entries: BTreeMap<String, EncryptedRecord>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EncryptedRecord {
    metadata: SecretMetadata,
    nonce: [u8; 24],
    ciphertext: Vec<u8>,
}
impl Default for Envelope {
    fn default() -> Self {
        Self {
            version: 1,
            entries: BTreeMap::new(),
        }
    }
}

/// A synchronous host-side adapter: call it on a blocking lane in async hosts.
/// Every operation reloads under the same OS file lock, avoiding lost updates
/// across handles/processes. Only authenticated ciphertext is persisted.
pub struct Vault {
    path: PathBuf,
    lock: PathBuf,
    master: SecretValue,
    redactor: Redactor,
    resolved_uses: std::sync::atomic::AtomicU64,
}
impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Vault([REDACTED])")
    }
}
impl Vault {
    pub fn open(
        path: impl AsRef<Path>,
        keys: Arc<dyn MasterKeyStore>,
        key_id: &str,
    ) -> Result<Self, VaultError> {
        if key_id.is_empty() || key_id.len() > 256 {
            return Err(VaultError::InvalidMetadata);
        }
        let path = path.as_ref().to_path_buf();
        let parent = path.parent().ok_or(VaultError::Storage)?;
        let mut directories = fs::DirBuilder::new();
        directories.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directories.mode(0o700);
        }
        directories
            .create(parent)
            .map_err(|_| VaultError::Storage)?;
        let path = fs::canonicalize(parent)
            .map_err(|_| VaultError::Storage)?
            .join(path.file_name().ok_or(VaultError::Storage)?);
        let lock = path.with_extension("vault-lock");
        let guard = locked(&lock)?;
        let master = match keys.load(key_id)? {
            Some(master) => master,
            None if path.exists() => return Err(VaultError::InvalidMasterKey),
            None => {
                let mut bytes = Zeroizing::new(vec![0; 32]);
                getrandom::fill(&mut bytes).map_err(|_| VaultError::Randomness)?;
                let key = SecretValue::new(bytes.to_vec())?;
                keys.store(key_id, &key)?;
                key
            }
        };
        if master.expose(<[u8]>::len) != 32 {
            return Err(VaultError::InvalidMasterKey);
        }
        let vault = Self {
            path,
            lock,
            master,
            redactor: Redactor::default(),
            resolved_uses: std::sync::atomic::AtomicU64::new(0),
        };
        let envelope = vault.read()?;
        for record in envelope.entries.values() {
            vault.redactor.register_secret(&vault.decrypt(record)?);
        }
        drop(guard);
        Ok(vault)
    }
    pub fn open_native(path: impl AsRef<Path>, namespace: &str) -> Result<Self, VaultError> {
        let path = path.as_ref();
        let keys = Arc::new(NativeKeyStore::new(
            namespace,
            path.parent().ok_or(VaultError::Storage)?,
        )?);
        Self::open(path, keys, namespace)
    }
    pub fn redactor(&self) -> Redactor {
        self.redactor.clone()
    }
    /// Metadata lookup for origin checks before any secret is decrypted.
    pub fn metadata(&self, reference: &SecretRef) -> Result<SecretMetadata, VaultError> {
        let _guard = locked(&self.lock)?;
        self.read()?
            .entries
            .get(reference.as_str())
            .map(|r| r.metadata.clone())
            .ok_or(VaultError::NotFound)
    }
    pub fn list(&self) -> Result<Vec<SecretMetadata>, VaultError> {
        let _guard = locked(&self.lock)?;
        let envelope = self.read()?;
        for record in envelope.entries.values() {
            self.redactor.register_secret(&self.decrypt(record)?);
        }
        Ok(envelope.entries.into_values().map(|r| r.metadata).collect())
    }
    pub fn put(&self, metadata: SecretMetadata, value: SecretValue) -> Result<(), VaultError> {
        metadata.validate()?;
        self.redactor.register_secret(&value);
        let encoded = serde_json::to_vec(&metadata).map_err(|_| VaultError::InvalidMetadata)?;
        if self.redactor.contains_secret(&encoded) {
            return Err(VaultError::InvalidMetadata);
        }
        let _guard = locked(&self.lock)?;
        let mut envelope = self.read()?;
        if envelope.entries.len() >= MAX_ENTRIES
            && !envelope.entries.contains_key(metadata.reference.as_str())
        {
            return Err(VaultError::Capacity);
        }
        let mut nonce = [0; 24];
        getrandom::fill(&mut nonce).map_err(|_| VaultError::Randomness)?;
        let ciphertext = self.master.expose(|key| {
            value.expose(|plaintext| {
                XChaCha20Poly1305::new_from_slice(key)
                    .map_err(|_| VaultError::InvalidMasterKey)?
                    .encrypt(
                        XNonce::from_slice(&nonce),
                        Payload {
                            msg: plaintext,
                            aad: &encoded,
                        },
                    )
                    .map_err(|_| VaultError::AuthenticationFailed)
            })
        })?;
        envelope.entries.insert(
            metadata.reference.as_str().to_owned(),
            EncryptedRecord {
                metadata,
                nonce,
                ciphertext,
            },
        );
        self.persist(&envelope)
    }
    /// Number of explicit with_secret uses; metadata reads do not increment this counter.
    pub fn resolved_use_count(&self) -> u64 {
        self.resolved_uses
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn with_secret<T>(
        &self,
        reference: &SecretRef,
        use_secret: impl FnOnce(&[u8]) -> T,
    ) -> Result<T, VaultError> {
        let value = {
            self.resolved_uses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _guard = locked(&self.lock)?;
            let envelope = self.read()?;
            let record = envelope
                .entries
                .get(reference.as_str())
                .ok_or(VaultError::NotFound)?;
            self.decrypt(record)?
        };
        self.redactor.register_secret(&value);
        Ok(value.expose(use_secret))
    }
    pub fn delete(
        &self,
        reference: &SecretRef,
        approval: DeleteApproval,
    ) -> Result<(), VaultError> {
        if &approval.reference != reference {
            return Err(VaultError::ApprovalRequired);
        }
        let _guard = locked(&self.lock)?;
        let mut envelope = self.read()?;
        let removed = envelope
            .entries
            .remove(reference.as_str())
            .ok_or(VaultError::NotFound)?;
        // Keep the old value registered in this process so an in-flight output is
        // still sanitized after deletion. It is absent from persistent storage.
        self.redactor.register_secret(&self.decrypt(&removed)?);
        self.persist(&envelope)
    }
    fn decrypt(&self, record: &EncryptedRecord) -> Result<SecretValue, VaultError> {
        let aad = serde_json::to_vec(&record.metadata).map_err(|_| VaultError::InvalidFormat)?;
        let bytes = self.master.expose(|key| {
            XChaCha20Poly1305::new_from_slice(key)
                .map_err(|_| VaultError::InvalidMasterKey)?
                .decrypt(
                    XNonce::from_slice(&record.nonce),
                    Payload {
                        msg: &record.ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| VaultError::AuthenticationFailed)
        })?;
        SecretValue::new(bytes)
    }
    fn read(&self) -> Result<Envelope, VaultError> {
        reject_symlink(&self.path)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = match options.open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Envelope::default()),
            Err(_) => return Err(VaultError::Storage),
        };
        let mut bytes = Vec::new();
        file.take(MAX_VAULT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| VaultError::Storage)?;
        if bytes.len() as u64 > MAX_VAULT_BYTES {
            return Err(VaultError::Capacity);
        }
        let envelope: Envelope =
            serde_json::from_slice(&bytes).map_err(|_| VaultError::InvalidFormat)?;
        if envelope.version != 1 || envelope.entries.len() > MAX_ENTRIES {
            return Err(VaultError::InvalidFormat);
        }
        for (id, r) in &envelope.entries {
            r.metadata.validate()?;
            if id != r.metadata.reference.as_str() || r.ciphertext.len() > MAX_SECRET_BYTES + 16 {
                return Err(VaultError::InvalidFormat);
            }
        }
        Ok(envelope)
    }
    fn persist(&self, envelope: &Envelope) -> Result<(), VaultError> {
        let bytes = serde_json::to_vec(envelope).map_err(|_| VaultError::InvalidFormat)?;
        if bytes.len() as u64 > MAX_VAULT_BYTES {
            return Err(VaultError::Capacity);
        }
        atomic_write(&self.path, &bytes)
    }
}

fn locked(path: &Path) -> Result<File, VaultError> {
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|_| VaultError::Storage)?;
    file.lock_exclusive().map_err(|_| VaultError::Storage)?;
    Ok(file)
}
fn reject_symlink(path: &Path) -> Result<(), VaultError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(VaultError::Storage),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(VaultError::Storage),
    }
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), VaultError> {
    reject_symlink(path)?;
    let parent = path.parent().ok_or(VaultError::Storage)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|_| VaultError::Storage)?;
    temporary
        .write_all(bytes)
        .map_err(|_| VaultError::Storage)?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| VaultError::Storage)?;
    temporary.persist(path).map_err(|_| VaultError::Storage)?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|_| VaultError::Storage)?;
    Ok(())
}

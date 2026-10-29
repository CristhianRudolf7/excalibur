use crate::{MasterKeyStore, SecretValue, VaultError};
use std::path::Path;

/// Actual OS adapter. No mock or file/plaintext fallback is selected when a
/// system store is unavailable. Android requires host-initialized ndk-context.
pub struct NativeKeyStore {
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "android"
    ))]
    store: std::sync::Arc<keyring_core::CredentialStore>,
    #[cfg(windows)]
    directory: std::path::PathBuf,
    namespace: String,
}
impl std::fmt::Debug for NativeKeyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeKeyStore([REDACTED])")
    }
}
impl NativeKeyStore {
    pub fn new(namespace: &str, directory: &Path) -> Result<Self, VaultError> {
        if namespace.is_empty()
            || namespace.len() > 128
            || !namespace
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(VaultError::InvalidMetadata);
        }
        let _ = directory;
        #[cfg(target_os = "linux")]
        let store = zbus_secret_service_keyring_store::Store::new()
            .map_err(|_| VaultError::KeyStoreUnavailable)?;
        #[cfg(target_os = "macos")]
        let store = apple_native_keyring_store::keychain::Store::new()
            .map_err(|_| VaultError::KeyStoreUnavailable)?;
        #[cfg(target_os = "ios")]
        let store = apple_native_keyring_store::protected::Store::new()
            .map_err(|_| VaultError::KeyStoreUnavailable)?;
        #[cfg(target_os = "android")]
        let store = android_native_keyring_store::Store::new()
            .map_err(|_| VaultError::KeyStoreUnavailable)?;
        #[cfg(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "android"
        ))]
        return Ok(Self {
            store,
            namespace: namespace.into(),
        });
        #[cfg(windows)]
        return Ok(Self {
            directory: directory.to_path_buf(),
            namespace: namespace.into(),
        });
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "android",
            windows
        )))]
        Err(VaultError::KeyStoreUnavailable)
    }
}
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "android"
))]
impl MasterKeyStore for NativeKeyStore {
    fn load(&self, id: &str) -> Result<Option<SecretValue>, VaultError> {
        use base64::Engine;
        let entry = self
            .store
            .build(&self.namespace, id, None)
            .map_err(|_| VaultError::KeyStoreUnavailable)?;
        match entry.get_secret() {
            Ok(encoded) => {
                let encoded = zeroize::Zeroizing::new(encoded);
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&encoded)
                    .map_err(|_| VaultError::InvalidMasterKey)?;
                Ok(Some(SecretValue::new(bytes)?))
            }
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(_) => Err(VaultError::KeyStoreUnavailable),
        }
    }
    fn store(&self, id: &str, key: &SecretValue) -> Result<(), VaultError> {
        use base64::Engine;
        let entry = self
            .store
            .build(&self.namespace, id, None)
            .map_err(|_| VaultError::KeyStoreUnavailable)?;
        // ASCII encoding also works with Secret Service implementations such as
        // KDE Wallet that reject arbitrary binary secrets.
        key.expose(|bytes| {
            let encoded =
                zeroize::Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(bytes));
            entry
                .set_secret(encoded.as_bytes())
                .map_err(|_| VaultError::KeyStoreUnavailable)
        })
    }
}

#[cfg(windows)]
impl NativeKeyStore {
    fn key_path(&self, id: &str) -> Result<std::path::PathBuf, VaultError> {
        if id != self.namespace {
            return Err(VaultError::InvalidMetadata);
        }
        Ok(self
            .directory
            .join(format!("{}.master.dpapi", self.namespace)))
    }
}
#[cfg(windows)]
impl MasterKeyStore for NativeKeyStore {
    fn load(&self, id: &str) -> Result<Option<SecretValue>, VaultError> {
        let path = self.key_path(id)?;
        crate::reject_symlink(&path)?;
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(VaultError::Storage),
        };
        let plaintext = dpapi(&bytes, false)?;
        Ok(Some(SecretValue::new(plaintext)?))
    }
    fn store(&self, id: &str, key: &SecretValue) -> Result<(), VaultError> {
        let path = self.key_path(id)?;
        std::fs::create_dir_all(&self.directory).map_err(|_| VaultError::Storage)?;
        let ciphertext = key.expose(|bytes| dpapi(bytes, true))?;
        crate::atomic_write(&path, &ciphertext)
    }
}
#[cfg(windows)]
fn dpapi(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>, VaultError> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len()).map_err(|_| VaultError::Capacity)?,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // DPAPI binds encryption to the current OS user. UI is forbidden, so locked
    // or unavailable protection fails rather than prompting from a tool thread.
    let result = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if result == 0 {
        return Err(VaultError::KeyStoreUnavailable);
    }
    let value =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        if !encrypt {
            std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        }
        LocalFree(output.pbData.cast());
    }
    Ok(value)
}
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "android",
    windows
)))]
impl MasterKeyStore for NativeKeyStore {
    fn load(&self, _: &str) -> Result<Option<SecretValue>, VaultError> {
        Err(VaultError::KeyStoreUnavailable)
    }
    fn store(&self, _: &str, _: &SecretValue) -> Result<(), VaultError> {
        Err(VaultError::KeyStoreUnavailable)
    }
}

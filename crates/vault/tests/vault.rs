use excalibur_vault::{
    DeleteApproval, MasterKeyStore, Redactor, SecretKind, SecretMetadata, SecretRef, SecretValue,
    Vault, VaultError,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct MemoryKeys(Mutex<BTreeMap<String, SecretValue>>);
impl MasterKeyStore for MemoryKeys {
    fn load(&self, id: &str) -> Result<Option<SecretValue>, VaultError> {
        Ok(self.0.lock().unwrap().get(id).cloned())
    }
    fn store(&self, id: &str, key: &SecretValue) -> Result<(), VaultError> {
        self.0.lock().unwrap().insert(id.into(), key.clone());
        Ok(())
    }
}
fn meta(name: &str) -> SecretMetadata {
    SecretMetadata {
        reference: SecretRef::parse(format!("secret://{name}")).unwrap(),
        name: name.into(),
        kind: SecretKind::ApiKey,
        usage: "synthetic test only".into(),
        url: None,
    }
}
fn value(text: &str) -> SecretValue {
    SecretValue::new(text.as_bytes().to_vec()).unwrap()
}

#[test]
fn encrypted_crud_reopen_metadata_and_live_redaction() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("vault.json");
    let keys = Arc::new(MemoryKeys::default());
    let vault = Vault::open(&path, keys.clone(), "synthetic-master").unwrap();
    let redactor = vault.redactor();
    let reference = meta("fixture").reference;
    vault
        .put(meta("fixture"), value("fixture-private-credential-one"))
        .unwrap();
    assert_eq!(vault.list().unwrap(), vec![meta("fixture")]);
    assert!(
        vault
            .with_secret(&reference, |b| b == b"fixture-private-credential-one")
            .unwrap()
    );
    assert!(
        !String::from_utf8(fs::read(&path).unwrap())
            .unwrap()
            .contains("fixture-private-credential-one")
    );
    assert_eq!(
        redactor.redact_text("before fixture-private-credential-one after"),
        "before [REDACTED] after"
    );
    let debug = format!(
        "{vault:?} {:?} {redactor:?}",
        value("fixture-private-credential-one")
    );
    assert!(!debug.contains("credential-one"));
    vault
        .put(meta("fixture"), value("fixture-private-credential-two"))
        .unwrap();
    assert!(
        !redactor
            .redact_text("fixture-private-credential-one fixture-private-credential-two")
            .contains("credential")
    );
    drop(vault);
    let vault = Vault::open(&path, keys, "synthetic-master").unwrap();
    assert!(
        vault
            .with_secret(&reference, |b| b == b"fixture-private-credential-two")
            .unwrap()
    );
    assert!(matches!(
        vault.delete(
            &reference,
            DeleteApproval::from_approved_host(SecretRef::parse("secret://another").unwrap())
        ),
        Err(VaultError::ApprovalRequired)
    ));
    vault
        .delete(
            &reference,
            DeleteApproval::from_approved_host(reference.clone()),
        )
        .unwrap();
    assert!(vault.list().unwrap().is_empty());
    assert!(matches!(
        vault.with_secret(&reference, |_| ()),
        Err(VaultError::NotFound)
    ));
    assert_eq!(
        vault
            .redactor()
            .redact_text("fixture-private-credential-two"),
        "[REDACTED]"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn ciphertext_nonce_and_associated_metadata_tampering_fail_closed() {
    for field in ["ciphertext", "nonce", "metadata"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("vault.json");
        let keys = Arc::new(MemoryKeys::default());
        let vault = Vault::open(&path, keys.clone(), "test").unwrap();
        vault
            .put(meta("fixture"), value("synthetic-private-value-42"))
            .unwrap();
        drop(vault);
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let record = &mut envelope["entries"]["secret://fixture"];
        if field == "metadata" {
            record[field]["usage"] = json!("tampered");
        } else {
            record[field][0] = json!(record[field][0].as_u64().unwrap() ^ 1);
        }
        fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
        let result = Vault::open(&path, keys, "test");
        assert!(
            matches!(result, Err(VaultError::AuthenticationFailed)),
            "field {field}: {result:?}"
        );
    }
}

#[test]
fn nonces_change_and_missing_master_never_replaces_existing_ciphertext() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("vault.json");
    let keys = Arc::new(MemoryKeys::default());
    let vault = Vault::open(&path, keys, "test").unwrap();
    vault
        .put(meta("fixture"), value("synthetic-private-value-42"))
        .unwrap();
    let first = fs::read(&path).unwrap();
    vault
        .put(meta("fixture"), value("synthetic-private-value-42"))
        .unwrap();
    let second = fs::read(&path).unwrap();
    assert_ne!(first, second);
    drop(vault);
    assert!(matches!(
        Vault::open(&path, Arc::new(MemoryKeys::default()), "test"),
        Err(VaultError::InvalidMasterKey)
    ));
    assert_eq!(fs::read(&path).unwrap(), second);
    let wrong = Arc::new(MemoryKeys::default());
    wrong
        .store("test", &SecretValue::new(vec![17; 32]).unwrap())
        .unwrap();
    assert!(matches!(
        Vault::open(&path, wrong, "test"),
        Err(VaultError::AuthenticationFailed)
    ));
}

#[test]
fn separate_handles_concurrent_puts_preserve_all_entries() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("vault.json");
    let keys = Arc::new(MemoryKeys::default());
    let barrier = Arc::new(std::sync::Barrier::new(8));
    std::thread::scope(|scope| {
        for thread in 0..8 {
            let path = &path;
            let keys = keys.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                let vault = Vault::open(path, keys, "test").unwrap();
                barrier.wait();
                for n in 0..8 {
                    vault
                        .put(
                            meta(&format!("fixture-{thread}-{n}")),
                            value(&format!("synthetic-private-value-{thread}-{n}")),
                        )
                        .unwrap();
                }
            });
        }
    });
    assert_eq!(
        Vault::open(path, keys, "test")
            .unwrap()
            .list()
            .unwrap()
            .len(),
        64
    );
}

#[test]
fn references_and_secure_input_never_accept_chat_shaped_payloads() {
    for reference in [
        "token",
        "secret://",
        "secret://../key",
        "secret://user@host",
        "secret://x?token=abc",
    ] {
        assert!(SecretRef::parse(reference).is_err());
    }
    assert!(serde_json::from_value::<SecretRef>(json!("secret://../key")).is_err());
    assert!(SecretValue::new(Vec::new()).is_err());
    assert!(SecretValue::new(vec![0; 65537]).is_err());
    let temp = tempfile::tempdir().unwrap();
    let vault = Vault::open(
        temp.path().join("vault"),
        Arc::new(MemoryKeys::default()),
        "test",
    )
    .unwrap();
    let mut website = meta("website");
    website.kind = SecretKind::WebsitePassword;
    assert!(matches!(
        vault.put(website.clone(), value("private-password-value")),
        Err(VaultError::InvalidMetadata)
    ));
    website.url = Some("https://user:pass@example.test".into());
    assert!(vault.put(website, value("private-password-value")).is_err());
}

#[test]
fn exact_patterns_json_and_arbitrary_bytes_are_redacted_without_destroying_safe_data() {
    let redactor = Redactor::with_secrets([
        value("synthetic-secret-value"),
        SecretValue::new(vec![0xff, 0x00, 0xfe]).unwrap(),
    ]);
    assert_eq!(
        redactor.redact_bytes(&[0x01, 0xff, 0, 0xfe, 0x02]),
        b"\x01[REDACTED]\x02"
    );
    let safe = json!({"safe":"remain","nested":["synthetic-secret-value",{"synthetic-secret-value":"synthetic-secret-value"}]});
    let clean = redactor.redact_json(&safe);
    assert_eq!(clean["safe"], "remain");
    assert!(!clean.to_string().contains("synthetic-secret-value"));
    for raw in [
        "Authorization: Bearer fixture-auth-123",
        "api_key = privatefixture",
        "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        "sk-proj-abcdefghijklmnopqrstuvwxyz123456",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nprivatebody\n-----END OPENSSH PRIVATE KEY-----",
    ] {
        assert!(redactor.contains_secret(raw.as_bytes()), "missing {raw}");
        let clean = redactor.redact_text(raw);
        assert!(!clean.contains("privatebody"));
        assert!(!clean.contains("fixture-auth"));
    }
    assert!(!redactor.contains_secret(b"ordinary synthetic terminal output"));
    assert!(!redactor.contains_secret(b"Authorization: Bearer secret://github-token"));
    assert!(!redactor.contains_secret(br#"{"password":"secret://website"}"#));
    assert!(
        redactor
            .contains_secret(b"Authorization: Bearer secret://github-token extra-private-value")
    );
    assert!(redactor.contains_secret(b"api_key=private-prefix secret://github-token"));
}

#[test]
fn streaming_all_boundaries_and_multiline_exact_values_never_release_secret_fragments() {
    let redactor = Redactor::with_secrets([
        value("fixture-cross-chunk-credential"),
        value("multiline\nsecret-body\nfinished"),
    ]);
    for text in [
        "prefix fixture-cross-chunk-credential suffix\n",
        "before multiline\nsecret-body\nfinished after\n",
        "Authorization: Bearer fixture-known-pattern\nnext\n",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nprivatebody\n-----END OPENSSH PRIVATE KEY-----\nnext\n",
    ] {
        for chunk_size in 1..text.len() {
            let mut stream = redactor.stream();
            let mut output = Vec::new();
            for chunk in text.as_bytes().chunks(chunk_size) {
                output.extend(stream.push(chunk));
            }
            output.extend(stream.finish());
            let output = String::from_utf8(output).unwrap();
            assert!(
                !output.contains("fixture-cross"),
                "chunk={chunk_size}: {output}"
            );
            assert!(
                !output.contains("secret-body"),
                "chunk={chunk_size}: {output}"
            );
            assert!(
                !output.contains("fixture-known"),
                "chunk={chunk_size}: {output}"
            );
            assert!(
                !output.contains("privatebody"),
                "chunk={chunk_size}: {output}"
            );
        }
    }
}

#[test]
fn streaming_preserves_large_safe_lines_prompts_and_bounds_secret_values() {
    let redactor = Redactor::with_secrets([value("synthetic-secret-value")]);
    let mut stream = redactor.stream();
    let mut output = Vec::new();
    assert_eq!(stream.push(b"ready"), b"ready");
    for _ in 0..2048 {
        output.extend(stream.push(&vec![b'x'; 1024]));
    }
    output.extend(stream.push(b"synthetic-secret-value\nvisible\n"));
    output.extend(stream.finish());
    assert_eq!(
        output.len(),
        2 * 1024 * 1024 + b"[REDACTED]\nvisible\n".len()
    );
    assert!(output[..2 * 1024 * 1024].iter().all(|&b| b == b'x'));
    assert!(output.ends_with(b"[REDACTED]\nvisible\n"));
    let mut stream = redactor.stream();
    let mut masked = stream.push(b"Authorization: Bearer ");
    for _ in 0..1024 {
        masked.extend(stream.push(&vec![b'z'; 1024]));
    }
    masked.extend(stream.push(b"\nsafe\n"));
    masked.extend(stream.finish());
    assert!(masked.len() < 64);
    assert!(masked.ends_with(b"\nsafe\n"));
    assert_eq!(masked, b"Authorization: [REDACTED]\nsafe\n");
    let mut stream = redactor.stream();
    redactor.register_secret(&value("freshly-registered-value"));
    let mut masked = stream.push(b"freshly-registered-value");
    masked.extend(stream.finish());
    assert_eq!(masked, b"[REDACTED]");
}

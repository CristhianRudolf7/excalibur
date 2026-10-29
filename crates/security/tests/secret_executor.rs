use excalibur_journal::Journal;
use excalibur_security::{
    Action, ApprovalScope, Autonomy, CapabilityToken, Decision, Error, Network, Redactor, Root,
    RunSecurity, SecretExecutor, Security,
};
use excalibur_store::{Database, DatabaseKind, ReadLane, StorageConfig};
use excalibur_types::runtime::ToolResult;
use excalibur_vault::{
    MasterKeyStore, SecretKind, SecretMetadata, SecretRef, SecretValue, Vault, VaultError,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

const SYNTHETIC: &str = "fixture-private-executor-credential";
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
struct Fixture {
    _root: tempfile::TempDir,
    db: Database,
    security: Security,
    executor: SecretExecutor,
    run: String,
    reference: SecretRef,
    vault: Arc<Vault>,
}
async fn fixture(autonomy: Autonomy) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let vault = Arc::new(
        Vault::open(
            root.path().join("private/vault"),
            Arc::new(MemoryKeys::default()),
            "synthetic-master",
        )
        .unwrap(),
    );
    let reference = SecretRef::parse("secret://fixture").unwrap();
    vault
        .put(
            SecretMetadata {
                reference: reference.clone(),
                name: "fixture".into(),
                kind: SecretKind::ApiKey,
                usage: "synthetic executor tests".into(),
                url: None,
            },
            SecretValue::new(SYNTHETIC.as_bytes().to_vec()).unwrap(),
        )
        .unwrap();
    let db = Database::open(
        root.path().join("state/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let token = CapabilityToken {
        roots: vec![Root {
            path: workspace.clone(),
            write: false,
        }],
        network: Network::Full,
        secrets: [reference.as_str().to_owned()].into(),
        budget: 100,
        ..Default::default()
    };
    let security = Security::new(db.clone(), token.clone(), vault.redactor())
        .await
        .unwrap();
    let run = Journal::new(db.clone())
        .await
        .unwrap()
        .create_run("secret-executor", json!({}))
        .await
        .unwrap();
    security
        .register_run(
            &run,
            RunSecurity {
                parent: None,
                project: "synthetic-project".into(),
                workspace,
                token,
                autonomy: vec![autonomy],
                full_acknowledged: autonomy == Autonomy::Full,
                user_requested: true,
            },
            100,
        )
        .await
        .unwrap();
    let executor = SecretExecutor::new(security.clone(), vault.clone()).unwrap();
    Fixture {
        _root: root,
        db,
        security,
        executor,
        run,
        reference,
        vault,
    }
}
fn approval(error: Error) -> String {
    match error {
        Error::ApprovalPending(id) => id,
        other => panic!("expected approval, got {other:?}"),
    }
}

#[tokio::test]
async fn resolver_requires_bound_registry_and_exact_capability_without_exposing_value() {
    let f = fixture(Autonomy::Full).await;
    let unrelated = Security::new(
        f.db.clone(),
        CapabilityToken::default(),
        Redactor::default(),
    )
    .await
    .unwrap();
    assert!(SecretExecutor::new(unrelated, f.vault.clone()).is_err());
    let called = Arc::new(AtomicUsize::new(0));
    let counter = called.clone();
    let result = f
        .executor
        .execute(
            &f.run,
            "denied",
            &SecretRef::parse("secret://forbidden").unwrap(),
            100,
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                ToolResult::ok(json!({}))
            },
        )
        .await;
    assert!(matches!(result, Err(Error::Denied(_))));
    assert_eq!(called.load(Ordering::SeqCst), 0);
    let result = f
        .executor
        .execute(&f.run, "authorized", &f.reference, 100, |value| {
            assert_eq!(value, SYNTHETIC.as_bytes());
            ToolResult::ok(json!({"header":String::from_utf8_lossy(value)}))
        })
        .await
        .unwrap();
    assert_eq!(result.output["header"], "[REDACTED]");
    assert!(result.full_output_blob.is_none());
    let rows = f.security.audit_query(1000).await.unwrap();
    let text = serde_json::to_string(&rows).unwrap();
    assert!(text.contains("secret://fixture"));
    assert!(!text.contains(SYNTHETIC));
    assert!(f.security.verify_audit().await.unwrap() > 0);
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn single_use_approval_and_duplicate_operation_dispatch_exactly_once() {
    let f = fixture(Autonomy::Supervised).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let first = approval(
        f.executor
            .execute(&f.run, "effect", &f.reference, 100, |_| {
                ToolResult::ok(json!({}))
            })
            .await
            .unwrap_err(),
    );
    let again = approval(
        f.executor
            .execute(&f.run, "effect", &f.reference, 101, |_| {
                ToolResult::ok(json!({}))
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(first, again);
    assert!(
        f.security
            .answer_approval(
                &first,
                true,
                ApprovalScope::Once,
                "authenticated-response",
                102
            )
            .await
            .unwrap()
    );
    assert!(
        !f.security
            .answer_approval(&first, true, ApprovalScope::Once, "duplicate-channel", 103)
            .await
            .unwrap()
    );
    let count = calls.clone();
    let result = f
        .executor
        .execute(&f.run, "effect", &f.reference, 104, move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            ToolResult::ok(json!({"value":SYNTHETIC}))
        })
        .await
        .unwrap();
    let count = calls.clone();
    let cached = f
        .executor
        .execute(&f.run, "effect", &f.reference, 105, move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            ToolResult::error("must not run")
        })
        .await
        .unwrap();
    assert_eq!(result, cached);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let next = approval(
        f.executor
            .execute(&f.run, "second-effect", &f.reference, 106, |_| {
                ToolResult::ok(json!({}))
            })
            .await
            .unwrap_err(),
    );
    assert_ne!(first, next);
    let text =
        f.db.read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT result FROM security_dispatch WHERE result IS NOT NULL LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    assert!(!text.contains(SYNTHETIC));
    f.db.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_retry_cannot_enter_closure_and_taint_audit_precede_resolution() {
    let f = fixture(Autonomy::Full).await;
    let count = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let executor = f.executor.clone();
    let run = f.run.clone();
    let reference = f.reference.clone();
    let calls = count.clone();
    let first = tokio::spawn(async move {
        executor
            .execute(&run, "concurrent", &reference, 100, move |value| {
                assert_eq!(value, SYNTHETIC.as_bytes());
                calls.fetch_add(1, Ordering::SeqCst);
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                ToolResult::ok(json!({"ok":true}))
            })
            .await
    });
    entered_rx.await.unwrap();
    let rows = f.security.audit_query(1000).await.unwrap();
    let text = serde_json::to_string(&rows).unwrap();
    assert!(text.contains("secret_executor_dispatch"));
    assert!(text.contains("secret_used"));
    assert!(!text.contains(SYNTHETIC));
    assert!(
        matches!(
            f.security
                .authorize(
                    &f.run,
                    &Action::Network {
                        domain: "example.test".into()
                    },
                    100
                )
                .await
                .unwrap(),
            Decision::Ask { .. }
        ),
        "taint must exist while the operation is still inside its first secret closure"
    );
    let calls = count.clone();
    let duplicate = f
        .executor
        .execute(&f.run, "concurrent", &f.reference, 100, move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            ToolResult::ok(json!({}))
        })
        .await;
    assert!(matches!(duplicate, Err(Error::Denied(_))));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    release_tx.send(()).unwrap();
    assert!(first.await.unwrap().unwrap().success);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    f.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupted_claim_is_not_repeated_and_unchecked_artifacts_are_rejected() {
    let f = fixture(Autonomy::Full).await;
    assert!(
        f.executor
            .execute(&f.run, "interrupted", &f.reference, 100, |_| panic!(
                "synthetic interrupted secret effect"
            ))
            .await
            .is_err()
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    assert!(
        f.executor
            .execute(&f.run, "interrupted", &f.reference, 100, move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                ToolResult::ok(json!({}))
            })
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let result = f
        .executor
        .execute(&f.run, "artifact", &f.reference, 100, |_| ToolResult {
            success: true,
            output: json!({}),
            full_output_blob: Some("unchecked-synthetic-reference".into()),
        })
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.full_output_blob.is_none());
    f.db.shutdown().await.unwrap();
}

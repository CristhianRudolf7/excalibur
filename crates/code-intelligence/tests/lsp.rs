#![cfg(target_os = "linux")]
use excalibur_code_intelligence::*;
use excalibur_vault::Redactor;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
struct CountSpawner {
    inner: ReadOnlySpawner,
    starts: AtomicUsize,
}
impl LspSpawner for CountSpawner {
    fn spawn<'a>(&'a self, config: &'a ServerConfig) -> LspFuture<'a, Result<SpawnedLsp>> {
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.inner.spawn(config).await
        })
    }
}
struct Fixture {
    dir: tempfile::TempDir,
    root: PathBuf,
    script: PathBuf,
    spawner: Arc<CountSpawner>,
    manager: LspManager,
}
impl Fixture {
    async fn new(mode: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file.py"), "OLD_ERROR\n").unwrap();
        let script = dir.path().join("server.py");
        std::fs::write(&script, include_str!("fixtures/lsp_server.py")).unwrap();
        let spawner = Arc::new(CountSpawner {
            inner: ReadOnlySpawner::default(),
            starts: AtomicUsize::new(0),
        });
        let manager = LspManager::new(spawner.clone());
        let f = Self {
            dir,
            root,
            script,
            spawner,
            manager,
        };
        f.manager
            .register("project", "python", f.config(mode))
            .await
            .unwrap();
        f
    }
    fn config(&self, mode: &str) -> ServerConfig {
        let mut config = ServerConfig::new(
            self.root.clone(),
            PathBuf::from("/usr/bin/python3"),
            vec![self.script.to_string_lossy().into_owned(), mode.into()],
        );
        config.read_roots.push(self.script.clone());
        config.request_timeout_ms = 2000;
        config
    }
    async fn metrics(&self) -> serde_json::Value {
        self.manager
            .request(
                "project",
                "python",
                LspOperation::WorkspaceSymbols {
                    query: "__metrics".into(),
                },
            )
            .await
            .unwrap()
            .result
    }
}
#[tokio::test]
async fn concrete_readonly_server_is_lazy_shared_protocol_complete_and_cannot_apply_edits() {
    let f = Fixture::new("pull").await;
    assert_eq!(f.spawner.starts.load(Ordering::SeqCst), 0);
    assert!(!f.manager.states().await[0].running);
    let before = f
        .manager
        .sync_document("project", "python", Path::new("file.py"), "OLD_ERROR\n")
        .await
        .unwrap();
    assert!(before.fresh);
    assert_eq!(before.new_errors.len(), 1);
    let (a, b) = tokio::join!(
        f.manager.request(
            "project",
            "python",
            LspOperation::Definition {
                path: "file.py".into(),
                position: Position {
                    line: 0,
                    character: 2
                }
            }
        ),
        f.manager.request(
            "project",
            "python",
            LspOperation::References {
                path: "file.py".into(),
                position: Position {
                    line: 0,
                    character: 2
                },
                include_declaration: true
            }
        )
    );
    assert!(
        a.unwrap().result["uri"]
            .as_str()
            .unwrap()
            .starts_with("file://")
    );
    assert_eq!(b.unwrap().result.as_array().unwrap().len(), 2);
    assert_eq!(f.spawner.starts.load(Ordering::SeqCst), 1);
    let symbols = f
        .manager
        .request(
            "project",
            "python",
            LspOperation::DocumentSymbols {
                path: "file.py".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(symbols.result[0]["name"], "synthetic_symbol");
    let renamed = f
        .manager
        .request(
            "project",
            "python",
            LspOperation::Rename {
                path: "file.py".into(),
                position: Position {
                    line: 0,
                    character: 2,
                },
                new_name: "renamed".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        renamed.result["documentChanges"][0]["edits"][0]["newText"],
        "renamed"
    );
    assert_eq!(renamed.document_hashes.len(), 1);
    assert_eq!(
        std::fs::read_to_string(f.root.join("file.py")).unwrap(),
        "OLD_ERROR\n"
    );
    let after = f
        .manager
        .sync_document(
            "project",
            "python",
            Path::new("file.py"),
            "OLD_ERROR NEW_ERROR\n🦀",
        )
        .await
        .unwrap();
    assert!(after.fresh);
    assert_eq!(after.diagnostics.len(), 2);
    assert_eq!(after.new_errors.len(), 1);
    assert_eq!(after.new_errors[0]["message"], "new error");
    f.manager
        .sync_document("project", "python", Path::new("file.py"), "OK")
        .await
        .unwrap();
    let metrics = f.metrics().await;
    assert!(metrics["readonly"].as_bool().unwrap());
    assert!(metrics["apply_edit_rejected"].as_bool().unwrap());
    assert_eq!(
        metrics["last_change"]["range"]["end"],
        json!({"line":1,"character":2})
    );
    assert!(!f.root.join("forbidden-server-write").exists());
    let pid = f.manager.states().await[0].pid.unwrap();
    assert_eq!(
        f.manager
            .reap_idle(Instant::now() + Duration::from_secs(599))
            .await,
        0
    );
    assert_eq!(
        f.manager
            .reap_idle(Instant::now() + Duration::from_secs(601))
            .await,
        1
    );
    assert!(!f.manager.states().await[0].running);
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    f.manager
        .sync_document("project", "python", Path::new("file.py"), "OK")
        .await
        .unwrap();
    assert_eq!(f.spawner.starts.load(Ordering::SeqCst), 2);
    f.manager.shutdown_all().await;
}
#[tokio::test]
async fn push_diagnostics_are_versioned_redacted_and_capabilities_fail_honestly() {
    let f = Fixture::new("push").await;
    let first = f
        .manager
        .sync_document("project", "python", Path::new("file.py"), "OLD_ERROR")
        .await
        .unwrap();
    assert!(first.fresh);
    assert_eq!(first.published_version, Some(1));
    let second = f
        .manager
        .sync_document(
            "project",
            "python",
            Path::new("file.py"),
            "OLD_ERROR NEW_ERROR",
        )
        .await
        .unwrap();
    assert_eq!(second.new_errors.len(), 1);
    f.manager.shutdown_all().await;
    let config = f.config("no-rename");
    f.manager
        .register("project", "python", config)
        .await
        .unwrap();
    f.manager
        .sync_document("project", "python", Path::new("file.py"), "OK")
        .await
        .unwrap();
    assert!(matches!(
        f.manager
            .request(
                "project",
                "python",
                LspOperation::Rename {
                    path: "file.py".into(),
                    position: Position::default(),
                    new_name: "new".into()
                }
            )
            .await,
        Err(Error::UnsupportedOperation)
    ));
    assert!(
        f.manager
            .sync_document("project", "python", Path::new("../server.py"), "data")
            .await
            .is_err()
    );
    f.manager.shutdown_all().await;
    let redactor = Redactor::default();
    redactor.register_secret(
        &excalibur_vault::SecretValue::new(b"synthetic-private-value".to_vec()).unwrap(),
    );
    let manager = LspManager::with_options(f.spawner.clone(), redactor, Duration::from_secs(600));
    manager
        .register("private", "python", f.config("pull"))
        .await
        .unwrap();
    let diagnostics = manager
        .sync_document("private", "python", Path::new("file.py"), "SECRET_ERROR")
        .await
        .unwrap();
    assert!(
        !serde_json::to_string(&diagnostics)
            .unwrap()
            .contains("synthetic-private-value")
    );
    manager.shutdown_all().await;
}
#[tokio::test]
async fn oversized_frames_and_wrong_encoding_close_the_session_without_invented_results() {
    for mode in ["malformed", "wrong-encoding"] {
        let f = Fixture::new(mode).await;
        let result = f
            .manager
            .request(
                "project",
                "python",
                LspOperation::WorkspaceSymbols { query: "x".into() },
            )
            .await;
        assert!(result.is_err());
        f.manager.shutdown_all().await;
    }
    let f = Fixture::new("unversioned").await;
    let result = f
        .manager
        .sync_document("project", "python", Path::new("file.py"), "OLD_ERROR")
        .await
        .unwrap();
    assert!(!result.fresh);
    assert!(result.unavailable_reason.is_some());
    assert!(result.new_errors.is_empty());
    f.manager.shutdown_all().await;
}
#[tokio::test]
async fn cancellation_sends_protocol_cancel_and_failed_server_is_restarted_lazily() {
    let f = Fixture::new("pull").await;
    f.manager
        .sync_document("project", "python", Path::new("file.py"), "OK")
        .await
        .unwrap();
    let manager = f.manager.clone();
    let task = tokio::spawn(async move {
        manager
            .request(
                "project",
                "python",
                LspOperation::WorkspaceSymbols {
                    query: "hold".into(),
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if f.metrics().await["held"] == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("held request never reached real server");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let metrics = f.metrics().await;
    assert_eq!(metrics["cancelled"], 1);
    assert!(
        f.manager
            .request(
                "project",
                "python",
                LspOperation::WorkspaceSymbols {
                    query: "crash".into()
                }
            )
            .await
            .is_err()
    );
    f.manager
        .request(
            "project",
            "python",
            LspOperation::WorkspaceSymbols {
                query: "restart".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(f.spawner.starts.load(Ordering::SeqCst), 2);
    f.manager.shutdown_all().await;
}

#[tokio::test]
async fn native_rust_analyzer_definitions_references_symbols_diagnostics_and_rename_in_readonly_project()
 {
    let located = std::process::Command::new("rustup")
        .args(["which", "rust-analyzer"])
        .output();
    let Ok(located) = located else {
        eprintln!("native rust-analyzer check unavailable: rustup absent");
        return;
    };
    if !located.status.success() {
        eprintln!("native rust-analyzer check unavailable: component absent");
        return;
    }
    let binary = PathBuf::from(String::from_utf8(located.stdout).unwrap().trim());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"synthetic_lsp_project\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Cargo.lock"),
        "version = 4\n[[package]]\nname = \"synthetic_lsp_project\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let file = root.join("src/lib.rs");
    let original =
        "pub fn add_one(n: i32) -> i32 { n + 1 }\npub fn caller() -> i32 { add_one(2) }\n";
    std::fs::write(&file, original).unwrap();
    let mut config = ServerConfig::new(root.clone(), binary.clone(), vec![]);
    config
        .read_roots
        .push(binary.parent().unwrap().parent().unwrap().to_path_buf());
    config.request_timeout_ms = 20_000;
    config.environment.insert(
        "PATH".into(),
        format!("{}:/usr/bin:/bin", binary.parent().unwrap().display()),
    );
    config.initialization_options = json!({"cargo":{"sysroot":binary.parent().unwrap().parent().unwrap(),"buildScripts":{"enable":false}},"procMacro":{"enable":false},"checkOnSave":false});
    let manager = LspManager::new(Arc::new(ReadOnlySpawner::default()));
    manager
        .register("native-rust", "rust", config)
        .await
        .unwrap();
    manager
        .sync_document("native-rust", "rust", Path::new("src/lib.rs"), original)
        .await
        .unwrap();
    let position = Position {
        line: 1,
        character: original.lines().nth(1).unwrap().find("add_one").unwrap() as u32 + 2,
    };
    let definition = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let candidate = manager
                .request(
                    "native-rust",
                    "rust",
                    LspOperation::Definition {
                        path: "src/lib.rs".into(),
                        position,
                    },
                )
                .await;
            if let Ok(candidate) = candidate
                && !candidate.result.is_null()
                && candidate
                    .result
                    .as_array()
                    .is_none_or(|items| !items.is_empty())
            {
                break candidate;
            }
            let messages = manager.server_messages("native-rust", "rust").await;
            if messages.iter().any(|message| {
                message.to_string().contains("Failed to load")
                    || message.to_string().contains("failed to load")
            }) {
                panic!("native project load notices: {messages:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("native rust-analyzer did not finish loading the synthetic project")
    });
    let references = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let candidate = manager
                .request(
                    "native-rust",
                    "rust",
                    LspOperation::References {
                        path: "src/lib.rs".into(),
                        position,
                        include_declaration: true,
                    },
                )
                .await
                .unwrap();
            if candidate
                .result
                .as_array()
                .is_some_and(|items| items.len() >= 2)
            {
                break candidate;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("native rust-analyzer did not publish references for the synthetic project")
    });
    assert!(
        references.result.as_array().is_some_and(|v| v.len() >= 2),
        "native references: {:?}; definition: {:?}",
        references.result,
        definition.result
    );
    let symbols = manager
        .request(
            "native-rust",
            "rust",
            LspOperation::DocumentSymbols {
                path: "src/lib.rs".into(),
            },
        )
        .await
        .unwrap();
    assert!(symbols.result.to_string().contains("add_one"));
    let renamed = manager
        .request(
            "native-rust",
            "rust",
            LspOperation::Rename {
                path: "src/lib.rs".into(),
                position,
                new_name: "increment".into(),
            },
        )
        .await
        .unwrap();
    assert!(renamed.result.to_string().contains("increment"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    let db = excalibur_store::Database::open(
        dir.path().join("codegraph.db"),
        excalibur_store::DatabaseKind::CodeGraph,
        excalibur_store::StorageConfig::default(),
    )
    .unwrap();
    let graph = CodeGraph::open(db.clone(), "native-rust").await.unwrap();
    graph
        .index_path_content(Path::new("src/lib.rs"), original)
        .await
        .unwrap();
    let enriched = graph
        .enrich_lsp(
            &manager,
            "native-rust",
            "rust",
            &root,
            Path::new("src/lib.rs"),
        )
        .await
        .unwrap();
    assert!(enriched.requests <= 32);
    assert!(
        enriched.enriched_edges > 0,
        "native LSP did not enrich graph: {enriched:?}"
    );
    let callers = graph
        .query(GraphQuery::Callers {
            symbol: "add_one".into(),
            limit: 100,
        })
        .await
        .unwrap();
    assert!(callers.edges.iter().any(|edge| edge.resolution == "lsp"));
    let mut bad = manager
        .sync_document(
            "native-rust",
            "rust",
            Path::new("src/lib.rs"),
            "pub fn broken( {\n",
        )
        .await
        .unwrap();
    if bad.new_errors.is_empty() {
        bad = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let reply = manager
                    .request(
                        "native-rust",
                        "rust",
                        LspOperation::Diagnostics {
                            path: "src/lib.rs".into(),
                        },
                    )
                    .await
                    .unwrap();
                let diagnostic: DiagnosticSnapshot = serde_json::from_value(reply.result).unwrap();
                if !diagnostic.new_errors.is_empty() {
                    break diagnostic;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("native server did not publish the new syntax errors");
    }
    assert!(
        bad.fresh,
        "native diagnostics freshness unavailable: {bad:?}"
    );
    assert!(
        !bad.new_errors.is_empty(),
        "native syntax diagnostics missing: {bad:?}"
    );
    manager.shutdown_all().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn simultaneous_document_update_invalidates_a_held_rename_and_prevents_idle_shutdown() {
    let f = Fixture::new("hold-rename").await;
    f.manager
        .sync_document("project", "python", Path::new("file.py"), "OK")
        .await
        .unwrap();
    let manager = f.manager.clone();
    let rename = tokio::spawn(async move {
        manager
            .request(
                "project",
                "python",
                LspOperation::Rename {
                    path: "file.py".into(),
                    position: Position::default(),
                    new_name: "new_name".into(),
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if f.metrics().await["held_rename"] == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("rename never reached held server request");
    assert_eq!(
        f.manager
            .reap_idle(Instant::now() + Duration::from_secs(601))
            .await,
        0,
        "in-flight operation must prevent idle shutdown"
    );
    f.manager
        .sync_document(
            "project",
            "python",
            Path::new("file.py"),
            "Changed while rename was computed",
        )
        .await
        .unwrap();
    f.manager
        .request(
            "project",
            "python",
            LspOperation::WorkspaceSymbols {
                query: "__release_rename".into(),
            },
        )
        .await
        .unwrap();
    let error = rename.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("documents changed"));
    assert_eq!(
        std::fs::read_to_string(f.root.join("file.py")).unwrap(),
        "OLD_ERROR\n"
    );
    f.manager.shutdown_all().await;
}
#[tokio::test]
async fn project_language_sessions_never_share_documents_or_read_authority() {
    let f = Fixture::new("pull").await;
    let second = f.dir.path().join("second-project");
    std::fs::create_dir(&second).unwrap();
    std::fs::write(second.join("file.py"), "second").unwrap();
    let mut config = f.config("pull");
    config.root = second.clone();
    f.manager
        .register("second", "python", config)
        .await
        .unwrap();
    let (first, other) = tokio::join!(
        f.manager
            .sync_document("project", "python", Path::new("file.py"), "first"),
        f.manager
            .sync_document("second", "python", Path::new("file.py"), "second")
    );
    assert!(first.unwrap().fresh);
    assert!(other.unwrap().fresh);
    assert_eq!(f.spawner.starts.load(Ordering::SeqCst), 2);
    let states = f.manager.states().await;
    assert_eq!(states.len(), 2);
    assert_ne!(states[0].pid, states[1].pid);
    let first = f
        .manager
        .document_snapshot("project", "python", Path::new("file.py"))
        .await
        .unwrap();
    let other = f
        .manager
        .document_snapshot("second", "python", Path::new("file.py"))
        .await
        .unwrap();
    assert_eq!(first.text, "first");
    assert_eq!(other.text, "second");
    assert_ne!(first.uri, other.uri);
    assert!(
        f.manager
            .request(
                "second",
                "python",
                LspOperation::Definition {
                    path: f.root.join("file.py"),
                    position: Position::default()
                }
            )
            .await
            .is_err()
    );
    f.manager.shutdown_all().await;
}

#[tokio::test]
async fn readonly_lsp_process_cannot_read_hidden_or_unaccounted_hardlink_aliases() {
    let f = Fixture::new("pull").await;
    let hidden = f.root.join(".env.synthetic");
    let metadata = f.root.join(".git");
    std::fs::create_dir(&metadata).unwrap();
    let unknown = f.dir.path().join("unmounted-private");
    std::fs::create_dir(&unknown).unwrap();
    let sdk = f.dir.path().join("sdk");
    std::fs::create_dir(&sdk).unwrap();
    let single = f.dir.path().join("single-public.rs");
    let sentinel = b"unregistered-synthetic-private-sentinel";
    let mut denied = Vec::new();
    for (source, alias) in [
        (hidden, f.root.join("public-env.rs")),
        (metadata.join("config"), f.root.join("public-metadata.rs")),
        (
            unknown.join("secret-one"),
            f.root.join("public-external.rs"),
        ),
        (sdk.join(".env.sdk"), sdk.join("public-sdk.rs")),
        (sdk.join(".git"), sdk.join("public-gitfile.rs")),
        (unknown.join("secret-two"), single.clone()),
    ] {
        std::fs::write(&source, sentinel).unwrap();
        std::fs::hard_link(&source, &alias).unwrap();
        denied.push(alias);
    }
    let ordinary = f.root.join("ordinary.rs");
    let permitted = sdk.join("ordinary-copy.rs");
    std::fs::write(&ordinary, b"ordinary-public-file").unwrap();
    std::fs::hard_link(&ordinary, &permitted).unwrap();
    let mut config = f.config("pull");
    config.read_roots.extend([sdk, single]);
    let probes = denied
        .iter()
        .chain([&ordinary, &permitted])
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    config.initialization_options = json!({"read_probes":probes});
    f.manager
        .register("project", "python", config)
        .await
        .unwrap();
    f.manager
        .sync_document("project", "python", Path::new("file.py"), "OK")
        .await
        .unwrap();
    let actual = f.metrics().await;
    for path in denied {
        let key = path.to_string_lossy();
        assert_eq!(
            actual["read_probes"][key.as_ref()]["readable"],
            false,
            "the confined process read {key}"
        );
        assert_eq!(actual["read_probes"][key.as_ref()]["bytes"], 0);
    }
    for path in [ordinary, permitted] {
        let key = path.to_string_lossy();
        assert_eq!(
            actual["read_probes"][key.as_ref()]["readable"],
            true,
            "ordinary authorized hardlink was hidden: {key}"
        );
        assert_eq!(actual["read_probes"][key.as_ref()]["bytes"], 20);
    }
    // No secret is registered with the redactor: successful reads cannot be hidden by sanitization.
    assert!(
        !actual
            .to_string()
            .contains(std::str::from_utf8(sentinel).unwrap())
    );
    f.manager.shutdown_all().await;
}

#[tokio::test]
async fn memory_pressure_releases_a_real_idle_server_without_waiting_for_its_age() {
    let f = Fixture::new("pull").await;
    f.metrics().await;
    assert!(f.manager.states().await[0].running);
    assert_eq!(f.manager.reap_idle(Instant::now()).await, 0);
    assert_eq!(f.manager.unload_idle().await, 1);
    assert!(!f.manager.states().await[0].running);
    f.metrics().await;
    assert_eq!(f.spawner.starts.load(Ordering::SeqCst), 2);
    f.manager.shutdown_all().await;
}

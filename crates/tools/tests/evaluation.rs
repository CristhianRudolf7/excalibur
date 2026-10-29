use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy, evaluation::EpisodeTools};
use excalibur_types::runtime::{Effect, Reconcile, ToolCall, ToolRuntime};
use serde_json::{Value, json};
use std::{fs, sync::Arc};
fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}
async fn fixture() -> (tempfile::TempDir, Database, Arc<LocalTools>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let local = Arc::new(
        LocalTools::new(
            db.clone(),
            root,
            ToolPolicy {
                read: Access::Allow,
                write: Access::Deny,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    (temp, db, local)
}
async fn invoke(
    tools: &EpisodeTools,
    id: &str,
    args: Value,
) -> excalibur_types::runtime::ToolResult {
    let prepared = tools
        .prepare("run", call(id, "task_db", args))
        .await
        .unwrap();
    tools.execute("run", id, &prepared).await
}
#[tokio::test]
async fn episode_records_are_atomic_versioned_scoped_and_recoverable() {
    let (_temp, db, local) = fixture().await;
    let tools = EpisodeTools::new(local.clone(), db.clone(), "trial-one")
        .await
        .unwrap();
    let args = json!({"ops":[{"op":"create_task","id":"task-1","title":"Synthetic task"},{"op":"create_record","id":"plan-1","entity":"plan","data":{"steps":["inspect","implement","verify"]}}]});
    let prepared = tools
        .prepare("run", call("create", "task_db", args.clone()))
        .await
        .unwrap();
    assert_eq!(prepared.effect, Effect::Keyed);
    let first = tools.execute("run", "create-key", &prepared).await;
    assert!(first.success, "{first:?}");
    assert_eq!(tools.execute("run", "create-key", &prepared).await, first);
    let reopened = EpisodeTools::new(local.clone(), db.clone(), "trial-one")
        .await
        .unwrap();
    assert!(
        matches!(reopened.reconcile("run","create-key",&prepared).await,Reconcile::Done(result)if result==first)
    );
    let updated=invoke(&tools,"update",json!({"ops":[{"op":"update_task","id":"task-1","expected_version":1,"title":"Updated task"}]})).await;
    assert!(updated.success);
    let rejected=invoke(&tools,"bad-batch",json!({"ops":[{"op":"create_task","id":"must-rollback","title":"Atomic rollback"},{"op":"update_task","id":"task-1","expected_version":1,"title":"Stale update"}]})).await;
    assert!(!rejected.success, "{rejected:?}");
    assert!(rejected.output.to_string().contains("version conflict"));
    let queried = invoke(&tools, "query", json!({"ops":[{"op":"query","limit":20}]})).await;
    assert!(queried.success);
    let rows = queried.output["results"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row["id"] != "must-rollback"));
    assert_eq!(
        rows.iter().find(|row| row["id"] == "task-1").unwrap()["version"],
        2
    );
    let other = EpisodeTools::new(local, db.clone(), "trial-two")
        .await
        .unwrap();
    let empty = invoke(&other, "query", json!({"ops":[{"op":"query"}]})).await;
    assert_eq!(empty.output["results"][0]["rows"], json!([]));
    assert!(!other.execute("run", "foreign", &prepared).await.success);
    for args in [
        json!({"ops":[{"op":"create_project","id":"project"}]}),
        json!({"ops":[{"op":"query","scope":"trial-two"}]}),
        json!({"ops":[{"op":"query","entity":"project"}]}),
        json!({"ops":[{"op":"query","limit":21}]}),
    ] {
        assert!(
            tools
                .prepare("run", call("denied", "task_db", args))
                .await
                .is_err()
        );
    }
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn episode_registry_exposes_real_tools_and_preserves_delegate_permissions() {
    let (temp, db, local) = fixture().await;
    fs::write(
        temp.path().join("work/public.txt"),
        "synthetic public input",
    )
    .unwrap();
    let tools = EpisodeTools::new(local, db.clone(), "trial").await.unwrap();
    let names = tools
        .definitions()
        .into_iter()
        .map(|d| d.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"task_db".into()) && names.contains(&"clock".into()));
    assert!(!names.iter().any(|name| name.contains("memory")
        || name.contains("project")
        || name.contains("secrets")));
    let read = tools
        .prepare(
            "run",
            call(
                "dynamic-read",
                "call_tool",
                json!({"name":"read_file","arguments":{"path":"public.txt"}}),
            ),
        )
        .await
        .unwrap();
    let result = tools.execute("run", "dynamic-read", &read).await;
    assert!(result.success, "{result:?}");
    assert!(result.output.to_string().contains("synthetic public input"));
    assert!(
        tools
            .prepare(
                "run",
                call(
                    "write",
                    "call_tool",
                    json!({"name":"write_file","arguments":{"path":"bad","content":"no"}})
                )
            )
            .await
            .is_err()
    );
    for name in [
        "secrets",
        "memory_search",
        "create_project",
        "call_tool",
        "ask_user",
        "subagent",
    ] {
        assert!(
            tools
                .prepare(
                    "run",
                    call("invalid", "call_tool", json!({"name":name,"arguments":{}}))
                )
                .await
                .is_err()
        );
    }
    let clock = tools
        .prepare("run", call("clock", "clock", json!({})))
        .await
        .unwrap();
    let result = tools.execute("run", "clock", &clock).await;
    assert_eq!(result.output["timezone"], "UTC");
    assert!(result.output["unix_seconds"].as_i64().unwrap() > 0);
    let search = tools
        .prepare(
            "run",
            call("search", "search_tools", json!({"query":"read_file"})),
        )
        .await
        .unwrap();
    let result = tools.execute("run", "search", &search).await;
    assert_eq!(result.output["tools"][0]["name"], "read_file");
    assert_eq!(result.output["frozen"], true);
    db.shutdown().await.unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn eval_terminal_uses_landlock_without_bubblewrap_and_rejects_private_overlap() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    fs::create_dir(&root).unwrap();
    fs::write(
        temp.path().join("private.txt"),
        "SYNTHETIC_PRIVATE_CONTAINER_CONFIG",
    )
    .unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "eval_terminal_subprocess",
            "--nocapture",
        ])
        .env("EXCALIBUR_EVAL_CONTAINER", "1")
        .env("EXCALIBUR_EVAL_TEST_WORK", &root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(root.join("build/renamed/result")).unwrap(),
        b"SYNTHETIC_EVAL_OK"
    );
    assert!(!root.join("forbidden-effect").exists());
}
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "process-isolated host declaration fixture invoked by eval_terminal_uses_landlock_without_bubblewrap_and_rejects_private_overlap"]
async fn eval_terminal_subprocess() {
    use excalibur_sandbox::{SandboxConfig, evaluation::EvalContainerAttestation};
    use excalibur_tools::terminal::{TerminalConfig, TerminalManager};
    let root = std::path::PathBuf::from(std::env::var_os("EXCALIBUR_EVAL_TEST_WORK").unwrap());
    let attestation =
        EvalContainerAttestation::from_host_declaration("local-synthetic-fixture").unwrap();
    assert_eq!(
        attestation.metadata()["container_engine_independently_verified"],
        false
    );
    let mut config = SandboxConfig::evaluation(&root, attestation.clone());
    config
        .hidden_paths
        .push(root.parent().unwrap().join("private.txt"));
    config.bubblewrap = Some(root.join("nonexistent-bwrap"));
    let terminal = TerminalManager::new(TerminalConfig {
        spool_dir: root.parent().unwrap().join("spool"),
        sandbox: Some(config),
        ..Default::default()
    })
    .unwrap();
    let private = serde_json::to_string(&root.parent().unwrap().join("private.txt")).unwrap();
    let command = format!(
        r#"python3 - <<'PY'
import ctypes,errno,os,socket
libc=ctypes.CDLL(None,use_errno=True)
assert libc.prctl(39,0,0,0,0)==1
assert libc.prctl(21,0,0,0,0)==2
for path in [{private}]:
    try:open(path).read();raise AssertionError('private container data was readable')
    except OSError:pass
for family,kind in [(socket.AF_INET,socket.SOCK_STREAM),(socket.AF_UNIX,socket.SOCK_STREAM)]:
    try:socket.socket(family,kind);raise AssertionError('disabled network socket opened')
    except OSError as error:assert error.errno==errno.EPERM
os.makedirs('build/fresh')
open('build/fresh/result','w').write('SYNTHETIC_EVAL_OK')
os.rename('build/fresh','build/renamed')
print('eval kernel restrictions verified')
PY"#
    );
    let result = terminal
        .exec("run", json!({"cmd":command,"yield_ms":30000}))
        .await;
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["exit_code"], 0);
    terminal.shutdown().await;
    let mut overlap = SandboxConfig::evaluation(&root, attestation);
    overlap.hidden_paths.push(root.join("private-config"));
    let denied = TerminalManager::new(TerminalConfig {
        spool_dir: root.parent().unwrap().join("spool-denied"),
        sandbox: Some(overlap),
        ..Default::default()
    })
    .unwrap();
    let result = denied
        .exec(
            "run",
            json!({"cmd":"printf bad > forbidden-effect","yield_ms":250}),
        )
        .await;
    assert!(!result.success);
    assert!(
        result.output["error"]
            .as_str()
            .unwrap()
            .contains("disjoint")
    );
    assert!(!root.join("forbidden-effect").exists());
    denied.shutdown().await;
}

#[tokio::test]
async fn episode_queries_are_bounded_and_concurrent_updates_preserve_data() {
    let (_temp, db, local) = fixture().await;
    let tools = Arc::new(
        EpisodeTools::new(local, db.clone(), "bounded")
            .await
            .unwrap(),
    );
    let ops=(0..25).map(|index|json!({"op":"create_task","id":format!("task-{index:02}"),"title":"Task","data":{"retained":true}})).collect::<Vec<_>>();
    assert!(invoke(&tools, "create", json!({"ops":ops})).await.success);
    let query = invoke(&tools, "query", json!({"ops":[{"op":"query"}]})).await;
    assert_eq!(
        query.output["results"][0]["rows"].as_array().unwrap().len(),
        20
    );
    assert_eq!(query.output["results"][0]["more"], true);
    let cursor = query.output["results"][0]["next"].clone();
    let rest = invoke(
        &tools,
        "rest",
        json!({"ops":[{"op":"query","after":cursor}]}),
    )
    .await;
    assert_eq!(
        rest.output["results"][0]["rows"].as_array().unwrap().len(),
        5
    );
    assert_eq!(rest.output["results"][0]["more"], false);
    let args = json!({"ops":[{"op":"update_task","id":"task-00","expected_version":1,"title":"CAS update"}]});
    let left = tools
        .prepare("run", call("left", "task_db", args.clone()))
        .await
        .unwrap();
    let right = tools
        .prepare("run", call("right", "task_db", args))
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        tools.execute("run", "left-key", &left),
        tools.execute("run", "right-key", &right)
    );
    assert_eq!(
        [first.success, second.success]
            .into_iter()
            .filter(|v| *v)
            .count(),
        1
    );
    let record = invoke(
        &tools,
        "final-query",
        json!({"ops":[{"op":"query","limit":1}]}),
    )
    .await;
    assert_eq!(
        record.output["results"][0]["rows"][0]["data"]["retained"],
        true
    );
    assert_eq!(record.output["results"][0]["rows"][0]["version"], 2);
    db.shutdown().await.unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn eval_sensitive_metadata_requires_a_real_readonly_mount() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".git/config"), b"SYNTHETIC_FROZEN_CONFIG").unwrap();
    let output = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "eval_readonly_metadata_subprocess",
            "--nocapture",
        ])
        .env("EXCALIBUR_EVAL_CONTAINER", "1")
        .env("EXCALIBUR_EVAL_TEST_WORK", &root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(root.join(".git/config")).unwrap(),
        b"SYNTHETIC_FROZEN_CONFIG"
    );
    assert_eq!(
        fs::read(root.join("allowed/nested/file")).unwrap(),
        b"allowed"
    );
}
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "private mount namespace fixture invoked by eval_sensitive_metadata_requires_a_real_readonly_mount"]
async fn eval_readonly_metadata_subprocess() {
    use excalibur_sandbox::{SandboxConfig, evaluation::EvalContainerAttestation};
    use excalibur_tools::terminal::{TerminalConfig, TerminalManager};
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let root = std::path::PathBuf::from(std::env::var_os("EXCALIBUR_EVAL_TEST_WORK").unwrap());
    let attestation =
        EvalContainerAttestation::from_host_declaration("synthetic-readonly-mount-container")
            .unwrap();
    let config = SandboxConfig::evaluation(&root, attestation);
    let denied = TerminalManager::new(TerminalConfig {
        spool_dir: root.parent().unwrap().join("first-spool"),
        sandbox: Some(config.clone()),
        ..Default::default()
    })
    .unwrap();
    let result = denied
        .exec(
            "run",
            json!({"cmd":"printf bad > bad-effect","yield_ms":250}),
        )
        .await;
    assert!(!result.success);
    assert!(
        result.output["error"]
            .as_str()
            .unwrap()
            .contains("read-only mount")
    );
    assert!(!root.join("bad-effect").exists());
    denied.shutdown().await;
    let path = CString::new(root.join(".git").as_os_str().as_bytes()).unwrap();
    unsafe {
        assert_eq!(
            libc::mount(
                path.as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null()
            ),
            0
        );
        assert_eq!(
            libc::mount(
                std::ptr::null(),
                path.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                std::ptr::null()
            ),
            0
        );
    }
    let terminal = TerminalManager::new(TerminalConfig {
        spool_dir: root.parent().unwrap().join("second-spool"),
        sandbox: Some(config),
        ..Default::default()
    })
    .unwrap();
    let result=terminal.exec("run",json!({"cmd":"python3 - <<'PY'\nimport os\ntry:open('.git/config','w').write('bad');raise AssertionError('readonly mount write escaped')\nexcept OSError:pass\nos.makedirs('allowed/nested')\nopen('allowed/nested/file','w').write('allowed')\nPY","yield_ms":30000})).await;
    assert!(result.success, "{result:?}");
    let result=terminal.exec("run",json!({"cmd":"mkdir -p allowed/new; mv allowed/nested/file allowed/new/file; mv allowed/new/file allowed/nested/file","yield_ms":30000})).await;
    assert!(result.success, "{result:?}");
    terminal.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn invalid_cgroup_delegation_prevents_user_code() {
    use excalibur_tools::terminal::{
        SandboxConfig, TerminalConfig, TerminalManager, prepare_project_boundary,
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    fs::create_dir(&root).unwrap();
    prepare_project_boundary(&root).unwrap();
    let fake = temp.path().join("fake-cgroup");
    fs::create_dir(&fake).unwrap();
    fs::write(fake.join("cgroup.procs"), b"").unwrap();
    let mut config = SandboxConfig::project(&root);
    config.cgroup_directory = Some(fake);
    let terminal = TerminalManager::new(TerminalConfig {
        spool_dir: temp.path().join("spool"),
        sandbox: Some(config),
        ..Default::default()
    })
    .unwrap();
    let result = terminal
        .exec(
            "run",
            json!({"cmd":"printf forbidden > cgroup-effect","yield_ms":250}),
        )
        .await;
    assert!(!result.success);
    assert!(
        result.output["error"]
            .as_str()
            .unwrap()
            .contains("real cgroup v2")
    );
    assert!(!root.join("cgroup-effect").exists());
    terminal.shutdown().await;
}

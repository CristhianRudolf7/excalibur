#![cfg(target_os = "linux")]
use excalibur_tools::terminal::{
    SandboxConfig, SandboxProfile, TerminalConfig, TerminalManager, prepare_project_boundary,
};
use excalibur_vault::{Redactor, SecretValue};
use serde_json::json;
use std::{fs, path::Path};
fn manager(_root: &Path, spool: &Path, sandbox: Option<SandboxConfig>) -> TerminalManager {
    TerminalManager::new(TerminalConfig {
        spool_dir: spool.into(),
        sandbox,
        ..Default::default()
    })
    .unwrap()
}
fn output(result: &excalibur_types::runtime::ToolResult) -> &str {
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["exited"], true, "{result:?}");
    result.output["output"].as_str().unwrap()
}
#[tokio::test]
async fn absent_sandbox_missing_runtime_and_unprepared_metadata_fail_before_user_effects() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir(&root).unwrap();
    let unconfigured = manager(&root, &dir.path().join("spool-a"), None);
    let denied = unconfigured
        .exec(
            "run",
            json!({"cmd":"printf BAD > marker","cwd":root,"yield_ms":250}),
        )
        .await;
    assert!(!denied.success);
    assert!(
        denied.output["error"]
            .as_str()
            .unwrap()
            .contains("explicit host sandbox")
    );
    assert!(!root.join("marker").exists());
    unconfigured.shutdown().await;
    let incomplete = manager(
        &root,
        &dir.path().join("spool-b"),
        Some(SandboxConfig::project(&root)),
    );
    let denied = incomplete
        .exec("run", json!({"cmd":"printf BAD > marker","yield_ms":250}))
        .await;
    assert!(!denied.success);
    assert!(
        denied.output["error"]
            .as_str()
            .unwrap()
            .contains("host-provisioned metadata"),
        "{denied:?}"
    );
    assert!(!root.join("marker").exists());
    assert!(!root.join(".git").exists());
    assert!(!root.join(".agents").exists());
    incomplete.shutdown().await;
    let mut missing = SandboxConfig::read_only(&root);
    missing.bubblewrap = Some(dir.path().join("does-not-exist"));
    let unavailable = manager(&root, &dir.path().join("spool-c"), Some(missing));
    let denied = unavailable
        .exec("run", json!({"cmd":"printf BAD > marker","yield_ms":250}))
        .await;
    assert!(!denied.success);
    assert!(!root.join("marker").exists());
    unavailable.shutdown().await;
    if std::process::Command::new("/usr/bin/bwrap")
        .arg("--version")
        .output()
        .ok()
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).starts_with("bubblewrap 0.9."))
    {
        let mut old = SandboxConfig::read_only(&root);
        old.bubblewrap = Some("/usr/bin/bwrap".into());
        let old_runtime = manager(&root, &dir.path().join("spool-old"), Some(old));
        let denied = old_runtime
            .exec("run", json!({"cmd":"printf BAD > marker","yield_ms":250}))
            .await;
        assert!(
            !denied.success,
            "the unpatched distro build must not run untrusted commands"
        );
        assert!(
            denied.output["error"]
                .as_str()
                .unwrap()
                .contains("CVE-2026-87766")
        );
        old_runtime.shutdown().await;
    }
}
#[tokio::test]
async fn readonly_namespaces_seccomp_and_private_home_prevent_escape_and_socket_exfiltration() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("input"), b"permitted").unwrap();
    let outside = dir.path().join("outside");
    fs::write(&outside, b"HOST_PRIVATE_SYNTHETIC").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("alias")).unwrap();
    let terminal = manager(
        &root,
        &dir.path().join("spool"),
        Some(SandboxConfig::read_only(&root)),
    );
    let script = format!(
        r#"python3 - <<'PY'
import ctypes, errno, json, os, socket
assert open('input').read() == 'permitted'
for path in [{outside:?}, 'alias', '/proc/1/root{outside}']:
    try:
        open(path).read()
        raise AssertionError('escaped read')
    except OSError:
        pass
try:
    open('input','w').write('BAD')
    raise AssertionError('readonly write permitted')
except OSError:
    pass
for family, kind in [(socket.AF_INET,socket.SOCK_STREAM),(socket.AF_INET6,socket.SOCK_DGRAM),(socket.AF_UNIX,socket.SOCK_STREAM)]:
    try:
        socket.socket(family,kind)
        raise AssertionError('socket creation permitted')
    except OSError as error:
        assert error.errno == errno.EPERM
libc=ctypes.CDLL(None,use_errno=True)
assert libc.unshare(0x10000000) == -1 and ctypes.get_errno() == errno.EPERM
assert os.environ['HOME'] == '/home/excalibur'
assert 'Seccomp:\t2' in open('/proc/self/status').read()
assert 'CapEff:\t0000000000000000' in open('/proc/self/status').read()
assert not os.path.exists('/root/.config/excalibur')
print(json.dumps({{'read_only':True,'tcp_udp_unix_denied':True,'namespace_escape_denied':True,'seccomp_filter':True,'private_home':True}}))
PY"#,
        outside = outside.display().to_string()
    );
    let result = terminal
        .exec("read", json!({"cmd":script,"yield_ms":30000}))
        .await;
    assert_eq!(result.output["exit_code"], 0, "{result:?}");
    let data: serde_json::Value = serde_json::from_str(output(&result).trim()).unwrap();
    assert_eq!(data["tcp_udp_unix_denied"], true);
    assert_eq!(fs::read(root.join("input")).unwrap(), b"permitted");
    assert_eq!(fs::read(&outside).unwrap(), b"HOST_PRIVATE_SYNTHETIC");
    terminal.shutdown().await;
}
#[tokio::test]
async fn restricted_project_preserves_metadata_secrets_and_supports_prepared_file_writes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(root.join("src")).unwrap();
    prepare_project_boundary(&root).unwrap();
    fs::write(root.join(".git/config"), b"host configuration").unwrap();
    fs::write(root.join("src/.agents/policy"), b"nested policy").unwrap();
    fs::write(root.join(".env"), b"SYNTHETIC_PRIVATE_ENV").unwrap();
    fs::write(root.join("input"), b"source").unwrap();
    let terminal = manager(
        &root,
        &dir.path().join("spool"),
        Some(SandboxConfig::project(&root)),
    );
    let command = r#"python3 - <<'PY'
import errno,os
open('output','w').write('allowed')
open('src/result','w').write('nested allowed')
for path in ['.git/config','src/.agents/policy']:
    try:
        open(path,'w').write('BAD')
        raise AssertionError('protected write permitted')
    except OSError: pass
try:
    open('.env').read()
    raise AssertionError('secret read permitted')
except OSError: pass
try:
    os.mkdir('new-directory')
    raise AssertionError('unprepared hierarchy mutation permitted')
except OSError as e: assert e.errno == errno.EPERM
try:
    os.rename('input','renamed')
    raise AssertionError('unverified directory/rename policy bypassed')
except OSError as e: assert e.errno == errno.EPERM
print('restricted project checks passed')
PY"#;
    let result = terminal
        .exec("project", json!({"cmd":command,"yield_ms":30000}))
        .await;
    assert_eq!(result.output["exit_code"], 0, "{result:?}");
    assert!(output(&result).contains("checks passed"));
    assert_eq!(
        fs::read(root.join(".git/config")).unwrap(),
        b"host configuration"
    );
    assert_eq!(
        fs::read(root.join("src/.agents/policy")).unwrap(),
        b"nested policy"
    );
    assert_eq!(
        fs::read(root.join(".env")).unwrap(),
        b"SYNTHETIC_PRIVATE_ENV"
    );
    assert_eq!(fs::read(root.join("output")).unwrap(), b"allowed");
    assert_eq!(
        fs::read(root.join("src/result")).unwrap(),
        b"nested allowed"
    );
    assert!(!root.join("new-directory").exists());
    assert!(!root.join("renamed").exists());
    terminal.shutdown().await;
}
#[tokio::test]
async fn narrower_run_capability_reaches_the_kernel_without_inheriting_the_whole_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(root.join("allowed")).unwrap();
    fs::write(root.join("allowed/file"), b"allowed").unwrap();
    fs::write(root.join("ungranted"), b"other scope").unwrap();
    prepare_project_boundary(&root).unwrap();
    let terminal = manager(
        &root,
        &dir.path().join("spool"),
        Some(SandboxConfig::project(&root)),
    );
    let mut child = SandboxConfig::read_only(&root);
    child.read_roots = vec![root.join("allowed")];
    terminal.set_run_sandbox("child", child).unwrap();
    let command = "test \"$(cat allowed/file)\" = allowed && test ! -e ungranted && ! printf BAD > allowed/file && printf CHILD_CONFINED";
    let result = terminal
        .exec("child", json!({"cmd":command,"yield_ms":30000}))
        .await;
    assert_eq!(result.output["exit_code"], 0, "{result:?}");
    assert!(output(&result).contains("CHILD_CONFINED"));
    assert_eq!(fs::read(root.join("allowed/file")).unwrap(), b"allowed");
    let mut escalation = SandboxConfig::project(&root);
    escalation.write_roots = vec![dir.path().into()];
    assert!(terminal.set_run_sandbox("child", escalation).is_err());
    let redirected = terminal
        .exec(
            "child",
            json!({"cmd":"printf BAD","cwd":dir.path(),"yield_ms":250}),
        )
        .await;
    assert!(!redirected.success);
    assert!(
        redirected.output["error"]
            .as_str()
            .unwrap()
            .contains("cwd is fixed")
    );
    terminal.shutdown().await;
}
#[tokio::test]
async fn explicit_protected_files_are_hidden_and_live_vault_redaction_precedes_spooling() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("vault"), b"opaque encrypted synthetic bytes").unwrap();
    let mut sandbox = SandboxConfig::read_only(&root);
    sandbox.hidden_paths = vec![root.join("vault")];
    let redactor = Redactor::default();
    let terminal = TerminalManager::new(TerminalConfig {
        spool_dir: dir.path().join("spool"),
        sandbox: Some(sandbox),
        redactor: redactor.clone(),
        ..Default::default()
    })
    .unwrap();
    let first=terminal.exec("live",json!({"cmd":"if cat vault 2>/dev/null; then exit 9; fi; printf READY; IFS= read -r line; printf '%s' \"$line\"","yield_ms":250})).await;
    assert_eq!(first.output["exited"], false, "{first:?}");
    assert_eq!(first.output["output"], "READY");
    let secret = SecretValue::new(b"opaque_runtime_value_0123456789".to_vec()).unwrap();
    redactor.register_secret(&secret);
    let session = first.output["session_id"].as_str().unwrap();
    terminal.io("live",json!({"action":"write","session_id":session,"text":"opaque_runtime_value_0123456789\n"})).await;
    let done = terminal
        .io(
            "live",
            json!({"action":"wait_for","session_id":session,"timeout_ms":5000}),
        )
        .await;
    assert_eq!(done.output["exit_code"], 0, "{done:?}");
    let full = fs::read(done.output["spool_path"].as_str().unwrap()).unwrap();
    assert!(!String::from_utf8_lossy(&full).contains("opaque_runtime_value_0123456789"));
    assert!(String::from_utf8_lossy(&full).contains("REDACTED"));
    terminal.shutdown().await;
}
#[tokio::test]
async fn analysis_temporary_writes_and_restricted_package_profiles_are_real_and_unsupported_profiles_deny()
 {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("input");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("source"), b"analysis input").unwrap();
    let analysis = manager(
        &root,
        &dir.path().join("analysis-spool"),
        Some(SandboxConfig::file_analysis(&root)),
    );
    let result=analysis.exec("analysis",json!({"cmd":"test \"$(cat source)\" = 'analysis input' && printf temporary > /tmp/analysis-result && cat /tmp/analysis-result","yield_ms":30000})).await;
    assert_eq!(result.output["exit_code"], 0, "{result:?}");
    assert_eq!(output(&result), "temporary");
    analysis.shutdown().await;
    prepare_project_boundary(&root).unwrap();
    let package = manager(
        &root,
        &dir.path().join("package-spool"),
        Some(SandboxConfig::tool_package(&root)),
    );
    let result = package
        .exec(
            "package",
            json!({"cmd":"printf package > artifact; cat artifact","yield_ms":30000}),
        )
        .await;
    assert_eq!(result.output["exit_code"], 0, "{result:?}");
    assert_eq!(output(&result), "package");
    assert_eq!(fs::read(root.join("artifact")).unwrap(), b"package");
    package.shutdown().await;
    for profile in [SandboxProfile::ProjectNet, SandboxProfile::Eval] {
        let mut config = SandboxConfig::project(&root);
        config.profile = profile;
        let terminal = manager(
            &root,
            &dir.path().join(format!("unsupported-{profile:?}")),
            Some(config),
        );
        let result = terminal
            .exec(
                "denied",
                json!({"cmd":"printf BAD > forbidden","yield_ms":250}),
            )
            .await;
        assert!(!result.success);
        assert!(!root.join("forbidden").exists());
        terminal.shutdown().await;
    }
}

#[tokio::test]
async fn application_data_subtrees_hide_later_sidecars_and_overlapping_roots_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(root.join("data")).unwrap();
    fs::write(root.join("data/main.db"), b"synthetic opaque storage").unwrap();
    let mut config = SandboxConfig::read_only(&root);
    config.hidden_paths = vec![root.join("data")];
    let terminal = manager(&root, &dir.path().join("spool"), Some(config));
    fs::write(root.join("data/main.db-wal"), b"later private sidecar").unwrap();
    let result=terminal.exec("storage",json!({"cmd":"if cat data/main.db data/main.db-wal 2>/dev/null; then exit 9; fi; printf STORAGE_HIDDEN","yield_ms":30000})).await;
    assert_eq!(result.output["exit_code"], 0, "{result:?}");
    assert_eq!(output(&result), "STORAGE_HIDDEN");
    terminal.shutdown().await;
    let mut overlap = SandboxConfig::read_only(&root);
    overlap.hidden_paths = vec![root.clone()];
    let terminal = manager(&root, &dir.path().join("overlap-spool"), Some(overlap));
    let result = terminal
        .exec(
            "overlap",
            json!({"cmd":"printf BAD > mutation","yield_ms":250}),
        )
        .await;
    assert!(!result.success);
    assert!(
        result.output["error"]
            .as_str()
            .unwrap()
            .contains("data root overlaps")
    );
    assert!(!root.join("mutation").exists());
    terminal.shutdown().await;
}

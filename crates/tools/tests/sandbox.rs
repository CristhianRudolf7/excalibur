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
import array, ctypes, errno, json, os, socket
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
for kind in [socket.SOCK_STREAM,socket.SOCK_SEQPACKET]:
    left,right=socket.socketpair(socket.AF_UNIX,kind)
    left.sendall(b'private-local-ipc')
    assert right.recv(64)==b'private-local-ipc'
    try:
        left.sendto(b'blocked-destination', '/tmp/forbidden-peer')
        raise AssertionError('IPC supplied a destination address')
    except OSError as error:
        assert error.errno == errno.EPERM
    descriptor=os.open('input',os.O_RDONLY)
    left.sendmsg([b'F'],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,array.array('i',[descriptor]))])
    message,ancillary,flags,address=right.recvmsg(1,socket.CMSG_SPACE(array.array('i').itemsize))
    assert message==b'F' and len(ancillary)==1
    level,kind,received=ancillary[0]
    assert level==socket.SOL_SOCKET and kind==socket.SCM_RIGHTS
    descriptors=array.array('i');descriptors.frombytes(received)
    assert len(descriptors)==1 and os.read(descriptors[0],64)==b'permitted'
    os.close(descriptors[0]);os.close(descriptor)
    left.close();right.close()
try:
    socket.socketpair(socket.AF_UNIX,socket.SOCK_DGRAM)
    raise AssertionError('retargetable UNIX datagram pair created')
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
async fn project_preserves_metadata_secrets_and_supports_directory_and_file_writes() {
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
os.mkdir('new-directory')
os.rename('input','renamed')
for path in ['.git']:
    try:
        os.rename(path, path + '-moved')
        raise AssertionError('protected mount or parent renamed')
    except OSError: pass
os.rename('src', 'src-moved')
try:
    open('src-moved/.agents/policy','w').write('BAD')
    raise AssertionError('metadata lost protection after parent rename')
except OSError: pass
print('project checks passed')
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
        fs::read(root.join("src-moved/.agents/policy")).unwrap(),
        b"nested policy"
    );
    assert_eq!(
        fs::read(root.join(".env")).unwrap(),
        b"SYNTHETIC_PRIVATE_ENV"
    );
    assert_eq!(fs::read(root.join("output")).unwrap(), b"allowed");
    assert_eq!(
        fs::read(root.join("src-moved/result")).unwrap(),
        b"nested allowed"
    );
    assert!(root.join("new-directory").is_dir());
    assert_eq!(fs::read(root.join("renamed")).unwrap(), b"source");
    let result = terminal.exec("project", json!({"cmd":"mkdir -p new-directory/next; printf second > new-directory/next/file; mv new-directory/next/file new-directory/next/renamed; cat new-directory/next/renamed", "yield_ms":30000})).await;
    assert_eq!(output(&result), "second");
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

#[tokio::test]
async fn explicit_external_read_roots_are_readonly_mask_secrets_and_do_not_expose_siblings() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let sdk = temp.path().join("host-sdk");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&sdk).unwrap();
    prepare_project_boundary(&root).unwrap();
    fs::write(sdk.join("header"), b"public SDK fixture").unwrap();
    fs::write(sdk.join(".env"), b"SYNTHETIC_HOST_SECRET").unwrap();
    fs::write(temp.path().join("host-private"), b"private sibling").unwrap();
    let mut config = SandboxConfig::project(&root);
    config.read_roots.push(sdk.clone());
    let terminal = manager(&root, &temp.path().join("spool"), Some(config));
    let sdk_literal = serde_json::to_string(&sdk).unwrap();
    let private_literal = serde_json::to_string(&temp.path().join("host-private")).unwrap();
    let script = format!(
        r#"python3 - <<'PY'
import os
sdk = {sdk_literal}
assert open(sdk + '/header').read() == 'public SDK fixture'
for path, mode in [(sdk + '/header', 'w'), (sdk + '/.env', 'r'), ({private_literal}, 'r')]:
    try:
        open(path, mode)
        raise AssertionError('host path authority escaped')
    except OSError: pass
assert os.environ['HOME'] == '/home/excalibur'
os.makedirs('build/new')
open('build/new/result', 'w').write('build input')
print('explicit SDK checks passed')
PY"#
    );
    let result = terminal
        .exec("external-sdk", json!({"cmd":script,"yield_ms":30000}))
        .await;
    assert_eq!(output(&result), "explicit SDK checks passed\n");
    assert_eq!(fs::read(sdk.join("header")).unwrap(), b"public SDK fixture");
    assert_eq!(
        fs::read(sdk.join(".env")).unwrap(),
        b"SYNTHETIC_HOST_SECRET"
    );
    terminal.shutdown().await;
}

#[tokio::test]
async fn project_builds_rust_offline_and_preserves_private_pre_exec_ipc() {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let sysroot = std::process::Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .unwrap();
    assert!(sysroot.status.success());
    let sysroot = std::path::PathBuf::from(String::from_utf8(sysroot.stdout).unwrap().trim());
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"synthetic-build\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/main.rs"),
        r#"
use std::{os::unix::process::CommandExt, process::Command};
fn main() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf SYNTHETIC-RUST-BUILD"]);
    unsafe { command.pre_exec(|| Ok(())); }
    let output = command.output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"SYNTHETIC-RUST-BUILD");
    print!("{}", String::from_utf8(output.stdout).unwrap());
}
"#,
    )
    .unwrap();
    prepare_project_boundary(&root).unwrap();
    let mut config = SandboxConfig::project(&root);
    config.read_roots.push(sysroot.clone());
    let terminal = manager(&root, &temp.path().join("spool"), Some(config));
    // The host grants its exact toolchain read-only. No Cargo cache, host HOME,
    // dependency download or inference service is involved in this std-only build.
    let toolchain_bin = serde_json::to_string(&sysroot.join("bin")).unwrap();
    let command = format!(
        "set -eu; export PATH={toolchain_bin}:$PATH; export CARGO_HOME=/tmp/cargo; cargo build --offline --quiet; mv target/debug/synthetic-build target/debug/renamed-build; ./target/debug/renamed-build"
    );
    let mut result = terminal
        .exec(
            "rust-build",
            json!({"cmd":command,"yield_ms":30000,"timeout_ms":120000}),
        )
        .await;
    if result.output["exited"] != true {
        result = terminal.io("rust-build", json!({"action":"wait_for","session_id":result.output["session_id"],"timeout_ms":120000})).await;
    }
    assert_eq!(result.output["exit_code"], 0, "{result:?}");
    assert_eq!(output(&result), "SYNTHETIC-RUST-BUILD");
    assert!(root.join("target/debug/renamed-build").is_file());
    let second = terminal.exec("rust-build", json!({"cmd":"mkdir -p target/further/nested; mv target/debug/renamed-build target/further/nested/program; ./target/further/nested/program","yield_ms":30000})).await;
    assert_eq!(output(&second), "SYNTHETIC-RUST-BUILD");
    terminal.shutdown().await;
}

#[tokio::test]
async fn sensitive_and_external_hardlink_aliases_are_hidden_while_internal_cargo_style_links_work()
{
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(root.join("ordinary")).unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(root.join(".git/config"), b"SYNTHETIC_HOST_METADATA").unwrap();
    fs::write(root.join("Jenkinsfile"), b"SYNTHETIC_HOST_PIPELINE").unwrap();
    fs::write(root.join(".env"), b"SYNTHETIC_ENV_SECRET").unwrap();
    let vault = temp.path().join("private-vault");
    fs::create_dir(&vault).unwrap();
    fs::write(vault.join("secret"), b"SYNTHETIC_VAULT_SECRET").unwrap();
    let sdk = temp.path().join("readonly-sdk");
    fs::create_dir(&sdk).unwrap();
    fs::write(sdk.join("header"), b"public SDK").unwrap();
    for (source, alias) in [
        (root.join(".git/config"), "metadata-alias"),
        (root.join(".git/config"), "metadata-second-alias"),
        (root.join("Jenkinsfile"), "pipeline-alias"),
        (root.join(".env"), "env-alias"),
        (vault.join("secret"), "vault-alias"),
        (sdk.join("header"), "sdk-alias"),
    ] {
        fs::hard_link(source, root.join("ordinary").join(alias)).unwrap();
    }
    fs::write(root.join("ordinary/public"), b"ordinary build artifact").unwrap();
    fs::hard_link(
        root.join("ordinary/public"),
        root.join("ordinary/public-alias"),
    )
    .unwrap();
    prepare_project_boundary(&root).unwrap();
    let mut config = SandboxConfig::project(&root);
    // Overlap must never count the same writable path twice and turn a link to
    // protected metadata into an apparently internal inode.
    config.write_roots.push(root.join("ordinary"));
    config.read_roots.push(sdk.clone());
    config.hidden_paths.push(vault.clone());
    let terminal = manager(&root, &temp.path().join("spool"), Some(config));
    let sdk_header = serde_json::to_string(&sdk.join("header")).unwrap();
    let script = format!(
        r#"python3 - <<'PY'
for alias in ['metadata-alias','metadata-second-alias','pipeline-alias','env-alias','vault-alias','sdk-alias']:
    for mode in ['r','w']:
        try:
            open('ordinary/'+alias,mode)
            raise AssertionError('cross-boundary hardlink alias was exposed')
        except OSError: pass
assert open({sdk_header}).read() == 'public SDK'
assert open('ordinary/public').read() == 'ordinary build artifact'
open('ordinary/public-alias','w').write('legitimate internal hardlink update')
assert open('ordinary/public').read() == 'legitimate internal hardlink update'
print('hardlink boundaries preserved')
PY"#
    );
    let result = terminal
        .exec(
            "hardlink-boundaries",
            json!({"cmd":script,"yield_ms":30000}),
        )
        .await;
    assert_eq!(output(&result), "hardlink boundaries preserved\n");
    for (source, expected) in [
        (
            root.join(".git/config"),
            b"SYNTHETIC_HOST_METADATA".as_slice(),
        ),
        (
            root.join("Jenkinsfile"),
            b"SYNTHETIC_HOST_PIPELINE".as_slice(),
        ),
        (root.join(".env"), b"SYNTHETIC_ENV_SECRET".as_slice()),
        (vault.join("secret"), b"SYNTHETIC_VAULT_SECRET".as_slice()),
        (sdk.join("header"), b"public SDK".as_slice()),
    ] {
        assert_eq!(fs::read(source).unwrap(), expected);
    }
    assert_eq!(
        fs::read(root.join("ordinary/public")).unwrap(),
        b"legitimate internal hardlink update"
    );
    terminal.shutdown().await;
}

#[test]
fn private_sandbox_sources_remain_outside_writable_host_tmpdir() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir(&root).unwrap();
    prepare_project_boundary(&root).unwrap();
    fs::write(root.join(".env"), b"SYNTHETIC_ENV_SECRET").unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "private_runtime_tmpdir_subprocess",
            "--nocapture",
        ])
        .env("EXCALIBUR_TMPDIR_TEST_ROOT", &root)
        .env("TMPDIR", &root)
        .env("TMP", &root)
        .env("TEMP", &root)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(root.join("marker")).unwrap(),
        b"private runtime verified"
    );
    assert!(fs::read_dir(&root).unwrap().all(|entry| {
        let name = entry.unwrap().file_name();
        !name.to_string_lossy().starts_with("excalibur-sandbox-")
            && !name.to_string_lossy().starts_with("excalibur-egress-")
    }));
}

#[test]
#[ignore = "isolated TMPDIR helper invoked by private_sandbox_sources_remain_outside_writable_host_tmpdir"]
fn private_runtime_tmpdir_subprocess() {
    use excalibur_sandbox::{
        PreparedSandbox,
        network::{NetworkPolicy, ProxyConfig},
    };
    let root = std::path::PathBuf::from(std::env::var_os("EXCALIBUR_TMPDIR_TEST_ROOT").unwrap());
    assert_eq!(std::env::temp_dir(), root);
    let bridge = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("excalibur-network-bridge");
    assert!(
        bridge.is_file(),
        "build the private network helper before sandbox integration tests"
    );
    let config = SandboxConfig::project_net(
        &root,
        ProxyConfig::new(NetworkPolicy::Domains(Default::default())),
        bridge,
    );
    let sandbox = PreparedSandbox::new(&config).unwrap();
    let (mut directory_mask, mut file_mask, mut broker) = (false, false, false);
    for arguments in sandbox.args.windows(3) {
        if arguments[0] != "--ro-bind" {
            continue;
        }
        let source = Path::new(&arguments[1]);
        let target = Path::new(&arguments[2]);
        let name = source.file_name().unwrap_or_default();
        let private = name == "masked-directory"
            || name == "masked-file"
            || target == Path::new("/run/excalibur/proxy.sock");
        if !private {
            continue;
        }
        assert!(
            config
                .write_roots
                .iter()
                .all(|root| !source.starts_with(root)),
            "private bind source entered writable authority: {source:?}"
        );
        directory_mask |= name == "masked-directory";
        file_mask |= name == "masked-file";
        broker |= target == Path::new("/run/excalibur/proxy.sock");
    }
    assert!(
        directory_mask && file_mask && broker,
        "all private bind source classes must be exercised"
    );
    let mut command = std::process::Command::new(&sandbox.program);
    sandbox.configure(
        &mut command,
        Path::new("/bin/sh"),
        &[
            "-c".into(),
            "printf 'private runtime verified' > marker".into(),
        ],
        false,
    );
    unsafe {
        assert_eq!(libc::fcntl(sandbox.filter_fd(), libc::F_SETFD, 0), 0);
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn bind_aliases_of_one_directory_entry_do_not_inflate_writable_hardlink_counts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(root.join("one")).unwrap();
    fs::create_dir(root.join("two")).unwrap();
    prepare_project_boundary(&root).unwrap();
    fs::write(
        root.join(".git/config"),
        b"SYNTHETIC_BIND_PROTECTED_METADATA",
    )
    .unwrap();
    fs::hard_link(root.join(".git/config"), root.join("one/alias")).unwrap();
    let result = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "bind_alias_inventory_subprocess",
            "--nocapture",
        ])
        .env("EXCALIBUR_BIND_ALIAS_TEST_ROOT", &root)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(root.join(".git/config")).unwrap(),
        b"SYNTHETIC_BIND_PROTECTED_METADATA"
    );
    assert_eq!(
        fs::read(root.join("marker")).unwrap(),
        b"bind alias accounting verified"
    );
}

#[test]
#[ignore = "private mount namespace helper invoked by bind_aliases_of_one_directory_entry_do_not_inflate_writable_hardlink_counts"]
fn bind_alias_inventory_subprocess() {
    use excalibur_sandbox::PreparedSandbox;
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let root =
        std::path::PathBuf::from(std::env::var_os("EXCALIBUR_BIND_ALIAS_TEST_ROOT").unwrap());
    let source = CString::new(root.join("one").as_os_str().as_bytes()).unwrap();
    let target = CString::new(root.join("two").as_os_str().as_bytes()).unwrap();
    unsafe {
        assert_eq!(
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null()
            ),
            0
        );
    }
    let sandbox = PreparedSandbox::new(&SandboxConfig::project(&root)).unwrap();
    let mut command = std::process::Command::new(&sandbox.program);
    let script = "python3 - <<'PY'\nfor path in ['one/alias','two/alias']:\n    for mode in ['r','w']:\n        try:\n            open(path,mode)\n            raise AssertionError('bind alias inflated the hardlink inventory')\n        except OSError: pass\nopen('marker','w').write('bind alias accounting verified')\nPY";
    sandbox.configure(
        &mut command,
        Path::new("/bin/sh"),
        &["-c".into(), script.into()],
        false,
    );
    unsafe {
        assert_eq!(libc::fcntl(sandbox.filter_fd(), libc::F_SETFD, 0), 0);
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

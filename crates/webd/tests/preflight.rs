use excalibur_webd::{Config, GIB, admits_growth, detect_chrome, disk_available};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn configuration_and_disk_admission_fail_closed() {
    assert!(Config::parse("").is_ok());
    for text in [
        "[web]\nos_uesr='nobody'",
        "[web]\ndata_dir='relative'",
        "[web]\nchrome_binary='chrome'",
        "[web]\ndisk_reserve_bytes=1",
        "[web]\nsearch_budget_bytes=0",
        "[web]\nos_user=''",
        "[other]\nenabled=true",
    ] {
        assert!(Config::parse(text).is_err(), "{text}");
    }
    assert!(admits_growth(7 * GIB, 2 * GIB, 5 * GIB));
    assert!(!admits_growth(7 * GIB - 1, 2 * GIB, 5 * GIB));
    assert!(!admits_growth(1, u64::MAX, 5 * GIB));
    assert!(disk_available(std::path::Path::new("/does-not-exist/excalibur")).is_err());
    assert!(disk_available(std::path::Path::new("/tmp")).unwrap() > 0);
}
#[test]
fn chrome_probe_requires_executable_success_and_version() {
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("chrome");
    for (body, valid) in [
        ("echo 'Google Chrome 156.0.8078.73'", true),
        ("echo 'Chromium 156.0.8078.73'", true),
        ("echo unknown", false),
        ("echo 'Google Chrome 156.0.8078.73'; exit 1", false),
    ] {
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(detect_chrome(Some(&script)).is_ok(), valid);
    }
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(detect_chrome(Some(&script)).is_err());
    assert!(detect_chrome(Some(&tmp.path().join("missing"))).is_err());
}
fn invoke(user: &str, directory: &std::path::Path, chrome: &str) -> std::process::Output {
    let config = tempfile::NamedTempFile::new().unwrap();
    fs::write(
        config.path(),
        format!(
            "[web]\nos_user='{user}'\ndata_dir='{}'\nrun_dir='{}'\nchrome_binary='{chrome}'\n",
            directory.display(),
            directory.display()
        ),
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_excalibur-webd"))
        .arg("--preflight")
        .arg(config.path())
        .output()
        .unwrap()
}
#[test]
fn root_is_rejected_before_any_runtime_resource() {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for user in ["root", "excalibur-nonexistent-test-user"] {
        let output = invoke(user, dir.path(), "/not-installed");
        assert!(!output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["ready"], false);
        assert!(report["error"].is_string());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
#[test]
fn final_identity_is_unprivileged_and_report_is_real() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let output = invoke("nobody", dir.path(), "/not-installed");
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(report["identity"]["uid"], 0);
    assert_ne!(report["identity"]["euid"], 0);
    assert_eq!(report["identity"]["capabilities"], "zero");
    if unsafe { libc::geteuid() } == 0 {
        assert_eq!(report["identity"]["uid"], 65534);
        assert_eq!(report["identity"]["gid"], 65534);
        assert_eq!(
            report["identity"]["supplementary_groups"],
            serde_json::json!([])
        );
    }
    assert!(report["chrome"].is_null());
    assert_eq!(report["sandbox_verified"], false);
    assert!(report["disk_available_bytes"].as_u64().unwrap() > 0);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn startup_exposes_only_final_identity_in_proc_and_accepts_local_fixture() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let chrome = dir.path().join("chrome");
    fs::write(
        &chrome,
        "#!/bin/sh\nsleep 1\necho 'Google Chrome 156.0.8078.73'\n",
    )
    .unwrap();
    fs::set_permissions(&chrome, fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.path().join("config.toml");
    fs::write(
        &config,
        format!(
            "[web]\nos_user='nobody'\ndata_dir='{}'\nrun_dir='{}'\nchrome_binary='{}'\n",
            dir.path().display(),
            dir.path().display(),
            chrome.display()
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_excalibur-webd"))
        .args(["--preflight"])
        .arg(&config)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut status = String::new();
    for _ in 0..100 {
        status = fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap();
        if status
            .lines()
            .find(|l| l.starts_with("Uid:"))
            .unwrap()
            .split_whitespace()
            .skip(1)
            .all(|n| n != "0")
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    for field in ["Uid:", "Gid:"] {
        assert!(
            status
                .lines()
                .find(|l| l.starts_with(field))
                .unwrap()
                .split_whitespace()
                .skip(1)
                .all(|n| n != "0"),
            "{status}"
        );
    }
    for field in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
        assert_eq!(
            status
                .lines()
                .find(|l| l.starts_with(field))
                .unwrap()
                .split_whitespace()
                .nth(1),
            Some("0000000000000000")
        );
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ready"], true);
    assert_eq!(report["chrome"]["version"], "Google Chrome 156.0.8078.73");
}

#[test]
fn ordinary_user_starts_without_resolving_configured_root_identity() {
    use std::os::unix::process::CommandExt;
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let binary = dir.path().join("webd");
    fs::copy(env!("CARGO_BIN_EXE_excalibur-webd"), &binary).unwrap();
    let config = dir.path().join("config.toml");
    fs::write(
        &config,
        "[web]\nos_user='root'\ndata_dir='/tmp'\nrun_dir='/tmp'\nchrome_binary='/not-installed'\n",
    )
    .unwrap();
    // Concurrent forked probes can briefly inherit the copy's write descriptor.
    let mut command = Command::new(binary);
    command.uid(65534).gid(65534).arg("--preflight").arg(config);
    let mut attempts = 0;
    let output = loop {
        match command.output() {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempts < 100 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            result => break result.unwrap(),
        }
    };
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["identity"]["uid"], 65534);
    assert_eq!(report["identity"]["euid"], 65534);
}

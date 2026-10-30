use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    path::Path,
    process::{Child, Command},
    time::{Duration, Instant},
};
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn start(binary: &Path, config: &Path, log: &Path, socket: &Path) -> Daemon {
    let out = fs::File::create(log).unwrap();
    let mut child = Daemon(
        Command::new(binary)
            .arg("--serve")
            .arg(config)
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(log).unwrap()
        );
        if UnixStream::connect(socket).is_ok() {
            return child;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "service socket deadline"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn rpc(socket: &Path, method: &str, key: &str, run: &str, params: Value) -> Value {
    loop {
        let mut stream = UnixStream::connect(socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut params = params.clone();
        params["op_key"] = json!(key);
        params["run_id"] = json!(run);
        params["deadline_ms"] = json!(30000);
        writeln!(
            stream,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
        )
        .unwrap();
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == 1 {
                if v["result"]["state"] == "working" {
                    break;
                }
                return v;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
#[test]
fn service_identity_socket_restart_and_parent_death() {
    if std::env::var("EXCALIBUR_BROWSER_TESTS").as_deref() == Ok("skip") {
        eprintln!("real service explicitly skipped");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (uid, gid) = if unsafe { libc::geteuid() } == 0 {
        let name = std::ffi::CString::new("excalibur-web").unwrap();
        let p = unsafe { libc::getpwnam(name.as_ptr()) };
        assert!(!p.is_null());
        unsafe { ((*p).pw_uid, (*p).pw_gid) }
    } else {
        (unsafe { libc::getuid() }, unsafe { libc::getgid() })
    };
    for path in [
        root.path().to_path_buf(),
        root.path().join("data"),
        root.path().join("run"),
    ] {
        if !path.exists() {
            fs::create_dir(&path).unwrap();
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        if unsafe { libc::geteuid() } == 0 {
            std::os::unix::fs::chown(path, Some(uid), Some(gid)).unwrap();
        }
    }
    let binary = root.path().join("webd");
    fs::copy(env!("CARGO_BIN_EXE_excalibur-webd"), &binary).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let config = root.path().join("web.toml");
    fs::write(&config,format!("[web]\nos_user='excalibur-web'\nhost_uid={uid}\ndata_dir='{}'\nrun_dir='{}'\n[web.profiles.personal]\nclass='persistent'\n[[web.profiles.personal.sites]]\nname='local'\nrw=['http://denied.test']\nlisten=true\n",root.path().join("data").display(),root.path().join("run").display())).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
    let socket = root.path().join("run/webd.sock");
    let log = root.path().join("daemon.log");
    let mut daemon = start(&binary, &config, &log, &socket);
    let status = fs::read_to_string(format!("/proc/{}/status", daemon.0.id())).unwrap();
    assert!(
        status
            .lines()
            .find(|s| s.starts_with("Uid:"))
            .unwrap()
            .split_whitespace()
            .skip(1)
            .all(|s| s == uid.to_string())
    );
    assert!(
        status
            .lines()
            .find(|s| s.starts_with("Gid:"))
            .unwrap()
            .split_whitespace()
            .skip(1)
            .all(|s| s == gid.to_string())
    );
    assert_eq!(
        status
            .lines()
            .find(|s| s.starts_with("Groups:"))
            .unwrap()
            .trim(),
        "Groups:"
    );
    for field in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
        assert!(
            status
                .lines()
                .find(|s| s.starts_with(field))
                .unwrap()
                .ends_with("0000000000000000")
        );
    }
    let e = rpc(&socket, "session.open", "e-open", "e", json!({}));
    assert!(e["error"].is_null(), "{e}");
    let p = rpc(
        &socket,
        "session.open",
        "p-open",
        "p",
        json!({"profile":"personal","site":"local"}),
    );
    assert!(p["error"].is_null(), "{p}");
    assert_eq!(e, rpc(&socket, "session.open", "e-open", "e", json!({})));
    let mut rejected = false;
    if unsafe { libc::geteuid() } == 0 {
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o711)).unwrap();
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o710)).unwrap();
        let output=Command::new("/usr/bin/python3").args(["-c","import socket,sys;s=socket.socket(socket.AF_UNIX);s.settimeout(3);s.connect(sys.argv[1]);assert s.recv(1)==b''",socket.to_str().unwrap()]).uid(65534).gid(gid).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        rejected = true;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let status_rpc = rpc(&socket, "status.get", "status", "p", json!({}));
    let instances = status_rpc["result"]["result"]["instances"]
        .as_array()
        .unwrap();
    assert_eq!(instances.len(), 2);
    let identities = instances
        .iter()
        .flat_map(|i| excalibur_browser::process::tree(i["pid"].as_u64().unwrap() as u32))
        .map(|p| (p.pid, p.start_ticks))
        .collect::<Vec<_>>();
    assert!(!identities.is_empty());
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    let started = Instant::now();
    loop {
        let remaining = identities
            .iter()
            .filter(|(pid, ticks)| {
                excalibur_browser::process::tree(*pid)
                    .iter()
                    .any(|p| p.pid == *pid && p.start_ticks == *ticks)
            })
            .collect::<Vec<_>>();
        if remaining.is_empty() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "live Chrome descendants: {remaining:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Restart uses the durable session database and safely replaces the stale socket.
    daemon = start(&binary, &config, &log, &socket);
    let list = rpc(&socket, "session.list", "list", "e", json!({}));
    assert_eq!(list["result"]["result"][0]["state"], "lost");
    let listp = rpc(&socket, "session.list", "list-p", "p", json!({}));
    assert_eq!(listp["result"]["result"][0]["state"], "ready");
    assert_eq!(
        p["result"]["result"]["session_id"],
        listp["result"]["result"][0]["session_id"]
    );
    unsafe {
        libc::kill(daemon.0.id() as i32, libc::SIGTERM);
    }
    let started = Instant::now();
    while daemon.0.try_wait().unwrap().is_none() {
        assert!(started.elapsed() < Duration::from_secs(15));
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!socket.exists());
    let evidence = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/test-evidence/browser/webd_boundary");
    fs::create_dir_all(&evidence).unwrap();
    fs::write(evidence.join("contract.json"),serde_json::to_vec_pretty(&json!({"uid":uid,"gid":gid,"identity":status,"unauthorized_peer_rejected":rejected,"instances_before_kill":instances,"remaining_live":[],"ephemeral_after_restart":list,"persistent_after_restart":listp,"clean_shutdown":true})).unwrap()).unwrap();
}

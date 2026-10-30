use excalibur_browser::{
    Class, LaunchOptions,
    process::{Process, verify},
    version::{VersionPolicy, VersionState},
};
fn options() -> LaunchOptions {
    LaunchOptions {
        binary: "/usr/bin/google-chrome-stable".into(),
        profile: "/tmp/private-profile".into(),
        proxy: "127.0.0.1:12345".parse().unwrap(),
        class: Class::Ephemeral,
        headless: true,
        listening: false,
        secret_service: false,
        extra_args: vec![],
        version_policy: VersionPolicy::default(),
    }
}
#[test]
fn flags_cannot_override_security_transport_or_profile() {
    let mut config = options();
    let args = config.arguments().unwrap();
    assert!(args.contains(&"--remote-debugging-pipe".into()));
    assert!(args.contains(&"--proxy-bypass-list=<-loopback>".into()));
    assert!(args.contains(&"--disk-cache-size=33554432".into()));
    for forbidden in [
        "--no-sandbox",
        "--disable-setuid-sandbox",
        "--single-process",
        "--no-zygote",
        "--disable-web-security",
        "--disable-site-isolation-trials",
        "--disable-features=site-per-process",
        "--disable-features=Translate,site-per-process",
        "--remote-allow-origins=*",
        "--ignore-certificate-errors",
        "--enable-unsafe-swiftshader",
        "--remote-debugging-port",
        "--remote-debugging-port=0",
        "--enable-automation",
        "--proxy-server=http://elsewhere",
        "--user-data-dir=/tmp/other",
        "--disable-features=IsolateOrigins",
    ] {
        config.extra_args = vec![forbidden.into()];
        assert!(config.arguments().is_err(), "{forbidden}");
    }
    config.extra_args.clear();
    config.proxy = "0.0.0.0:1234".parse().unwrap();
    assert!(config.arguments().is_err());
    config = options();
    config.listening = true;
    assert!(config.arguments().is_err());
    config.class = Class::Persistent;
    let args = config.arguments().unwrap();
    assert!(args.contains(&"--disk-cache-size=67108864".into()));
    assert!(args.contains(&"--disable-background-timer-throttling".into()));
}
#[test]
fn version_gaps_age_unknowns_and_ephemeral_policy() {
    let mut policy = VersionPolicy {
        stable_major: Some(156),
        ..Default::default()
    };
    for (version, expected) in [
        (156, VersionState::Current),
        (155, VersionState::Current),
        (154, VersionState::Warning),
        (153, VersionState::BlockedPersistent),
        (160, VersionState::Current),
    ] {
        assert_eq!(policy.assess(version, 100).state, expected);
    }
    policy.installed_release_unix_secs = Some(100);
    assert_eq!(
        policy.assess(156, 100 + 42 * 86400).state,
        VersionState::Current
    );
    assert_eq!(
        policy.assess(156, 101 + 42 * 86400).state,
        VersionState::BlockedPersistent
    );
    assert_eq!(
        VersionPolicy::default().assess(156, 100).state,
        VersionState::Unknown
    );
}
#[test]
fn sandbox_requires_all_four_checks_for_every_renderer() {
    let browser = Process {
        pid: 1,
        parent: 0,
        start_ticks: 1,
        kind: "browser".into(),
        seccomp: None,
        no_new_privs: None,
        user_ns: "host-user".into(),
        net_ns: "host-net".into(),
    };
    let renderer = Process {
        pid: 2,
        parent: 1,
        start_ticks: 1,
        kind: "renderer".into(),
        seccomp: Some(2),
        no_new_privs: Some(1),
        user_ns: "child-user".into(),
        net_ns: "child-net".into(),
    };
    assert_eq!(verify(&[browser.clone(), renderer.clone()]).unwrap(), 1);
    for index in 0..4 {
        let mut bad = renderer.clone();
        match index {
            0 => bad.seccomp = Some(0),
            1 => bad.no_new_privs = Some(0),
            2 => bad.user_ns = browser.user_ns.clone(),
            _ => bad.net_ns = browser.net_ns.clone(),
        };
        assert!(verify(&[browser.clone(), renderer.clone(), bad]).is_err());
    }
    assert!(verify(&[]).is_err());
}

#[tokio::test]
async fn optional_stable_metadata_is_bounded_and_refreshes_policy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for version in [156, 159] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 1024];
            let _ = socket.read(&mut buffer).await.unwrap();
            let body =
                format!("{{\"channels\":{{\"Stable\":{{\"version\":\"{version}.0.0.0\"}}}}}}");
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let mut policy = VersionPolicy {
        stable_metadata_url: Some(format!("http://{address}/stable.json")),
        ..Default::default()
    };
    policy.refresh().await.unwrap();
    assert_eq!(policy.assess(156, 0).state, VersionState::Current);
    policy.refresh().await.unwrap();
    assert_eq!(policy.assess(156, 0).state, VersionState::BlockedPersistent);
    server.await.unwrap();
    policy.stable_metadata_url = Some("http://not-a-local-fixture.invalid/".into());
    assert!(policy.refresh().await.is_err());
    assert_eq!(policy.stable_major, Some(159));
}
#[test]
fn process_tree_tracks_nested_types_and_start_times() {
    use std::fs;
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    for (pid, parent, kind, isolated) in [
        (10, 1, "", false),
        (20, 10, "zygote", true),
        (30, 20, "renderer", true),
        (40, 10, "utility", false),
        (50, 1, "renderer", false),
    ] {
        let root = dir.path().join(pid.to_string());
        fs::create_dir_all(root.join("ns")).unwrap();
        let mut fields = vec!["0".to_owned(); 22];
        fields[0] = "S".into();
        fields[1] = parent.to_string();
        fields[19] = (pid * 100).to_string();
        fs::write(
            root.join("stat"),
            format!("{pid} (Chrome process) {}", fields.join(" ")),
        )
        .unwrap();
        fs::write(root.join("status"), "Seccomp:\t2\nNoNewPrivs:\t1\n").unwrap();
        fs::write(
            root.join("cmdline"),
            if kind.is_empty() {
                "chrome".into()
            } else {
                format!("chrome --type={kind} --option=value")
            },
        )
        .unwrap();
        symlink(
            if isolated { "child-user" } else { "host-user" },
            root.join("ns/user"),
        )
        .unwrap();
        symlink(
            if isolated { "child-net" } else { "host-net" },
            root.join("ns/net"),
        )
        .unwrap();
    }
    let tree = excalibur_browser::process::tree_at(dir.path(), 10);
    assert_eq!(tree.len(), 4);
    assert_eq!(tree.iter().find(|p| p.pid == 30).unwrap().start_ticks, 3000);
    assert_eq!(verify(&tree).unwrap(), 1);
    fs::write(
        dir.path().join("30/status"),
        "Seccomp:\t0\nNoNewPrivs:\t1\n",
    )
    .unwrap();
    assert!(verify(&excalibur_browser::process::tree_at(dir.path(), 10)).is_err());
}

#[test]
fn sandbox_initialization_has_a_deadline_and_verified_pids_cannot_regress() {
    use excalibur_browser::process::SandboxGate;
    let browser = Process {
        pid: 1,
        parent: 0,
        start_ticks: 10,
        kind: "browser".into(),
        seccomp: None,
        no_new_privs: None,
        user_ns: "host".into(),
        net_ns: "host".into(),
    };
    let mut renderer = Process {
        pid: 2,
        parent: 1,
        start_ticks: 20,
        kind: "renderer".into(),
        seccomp: Some(0),
        no_new_privs: Some(1),
        user_ns: "child".into(),
        net_ns: "child".into(),
    };
    let now = std::time::Instant::now();
    let mut gate = SandboxGate::default();
    assert!(
        !gate
            .check(&[browser.clone(), renderer.clone()], now)
            .unwrap()
    );
    assert!(
        gate.check(
            &[browser.clone(), renderer.clone()],
            now + std::time::Duration::from_secs(1)
        )
        .is_err()
    );
    let mut gate = SandboxGate::default();
    renderer.seccomp = Some(2);
    assert!(
        gate.check(&[browser.clone(), renderer.clone()], now)
            .unwrap()
    );
    renderer.seccomp = Some(0);
    assert!(gate.check(&[browser, renderer], now).is_err());
}

//! Real-Chrome 9.5 block matrices (9.1) over local synthetic pages.
//!
//! Every detector kind gets a dedicated synthetic page plus control pages
//! without 9.1 markers. No external sites, accounts or paid APIs are used:
//! all hostnames resolve through the local fixture proxy. Evidence lands in
//! `target/test-evidence/browser/block_matrix/`.
use excalibur_browser::{
    sessions::{Manager, Open, Options, Profile, Site},
    version::VersionPolicy,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn identity() -> (u32, u32) {
    if unsafe { libc::geteuid() } != 0 {
        return (unsafe { libc::getuid() }, unsafe { libc::getgid() });
    }
    let name = std::ffi::CString::new("excalibur-web").unwrap();
    let p = unsafe { libc::getpwnam(name.as_ptr()) };
    assert!(!p.is_null());
    unsafe { ((*p).pw_uid, (*p).pw_gid) }
}

#[test]
fn block_matrices() {
    run_fixture("block_matrix_child", "block_matrix");
}

fn run_fixture(child: &str, suite: &str) {
    if std::env::var("EXCALIBUR_BROWSER_TESTS").as_deref() == Ok("skip") {
        eprintln!("real sessions explicitly skipped");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
    if unsafe { libc::geteuid() } == 0 {
        let (uid, gid) = identity();
        std::os::unix::fs::chown(d.path(), Some(uid), Some(gid)).unwrap();
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", child, "--nocapture"])
        .env("EXCALIBUR_SESSION_DIR", d.path())
        .output()
        .unwrap();
    let evidence = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../target/test-evidence/browser/{suite}"));
    fs::create_dir_all(&evidence).unwrap();
    fs::write(
        evidence.join("runner.log"),
        [&output.stdout[..], &output.stderr[..]].concat(),
    )
    .unwrap();
    for entry in fs::read_dir(d.path()).unwrap().filter_map(Result::ok) {
        if entry.path().extension().is_some_and(|e| e == "json") {
            fs::copy(entry.path(), evidence.join(entry.file_name())).unwrap();
        }
    }
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "privilege dropping subprocess"]
fn block_matrix_child() {
    let root = PathBuf::from(std::env::var_os("EXCALIBUR_SESSION_DIR").unwrap());
    if unsafe { libc::geteuid() } == 0 {
        let (uid, gid) = identity();
        unsafe {
            assert_eq!(libc::setgroups(0, std::ptr::null()), 0);
            assert_eq!(libc::setgid(gid), 0);
            assert_eq!(libc::setuid(uid), 0);
        }
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(contract(root));
}

fn shell(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><meta name='viewport' content='width=device-width,initial-scale=1'><title>{title}</title>{body}"
    )
}

fn route(host: &str, path: &str) -> (u16, Vec<(&'static str, &'static str)>, String) {
    let ok = vec![("Content-Type", "text/html"), ("Cache-Control", "no-store")];
    match (host, path) {
        (_, "/m/captcha-frame") => (
            200,
            ok,
            shell(
                "captcha frame",
                "<h1>synthetic</h1><iframe width='400' height='200' src='http://www.google.com/recaptcha/api2/anchor?synthetic=1'></iframe>",
            ),
        ),
        ("www.google.com", "/recaptcha/api2/anchor") => {
            (200, ok, shell("anchor", "<div>synthetic anchor</div>"))
        }
        (_, "/m/captcha-marker") => (
            200,
            ok,
            shell(
                "captcha marker",
                "<h1>synthetic</h1><div class='g-recaptcha' style='width:300px;height:80px'></div><textarea name='g-recaptcha-response'></textarea>",
            ),
        ),
        (_, "/m/site-denied") => (
            403,
            ok,
            shell(
                "denied",
                "<h1>denied</h1><p>Access to this page has been denied</p>",
            ),
        ),
        (_, "/m/site-verify") => (
            503,
            ok,
            shell(
                "verify",
                "<h1>denied</h1><p>Access to this page has been denied. Please verify you are human to continue.</p>",
            ),
        ),
        (_, "/m/cf-header") => (
            200,
            vec![
                ("Content-Type", "text/html"),
                ("Cache-Control", "no-store"),
                ("cf-mitigated", "challenge"),
            ],
            shell("challenge", "<h1>synthetic challenge</h1>"),
        ),
        (_, "/m/login-password") => (
            200,
            ok,
            shell(
                "account",
                "<h1>synthetic account</h1><form><input type='password' name='p'></form>",
            ),
        ),
        (_, "/m/otp") => (
            200,
            ok,
            shell(
                "verify",
                "<h1>synthetic</h1><form><input autocomplete='one-time-code' name='code'></form>",
            ),
        ),
        (_, "/m/otp-group") => (
            200,
            ok,
            shell(
                "verify",
                "<h1>synthetic</h1><form><input maxlength='1' inputmode='numeric'><input maxlength='1' inputmode='numeric'><input maxlength='1' inputmode='numeric'><input maxlength='1' inputmode='numeric'></form>",
            ),
        ),
        (_, "/m/control-badge") => (
            200,
            ok,
            shell(
                "control",
                "<h1>synthetic</h1><div class='g-recaptcha' style='display:none'></div>",
            ),
        ),
        (_, "/m/control-cdn") => (
            200,
            ok,
            shell(
                "control",
                "<h1>synthetic</h1><p>assets load from /cdn-cgi/challenge-platform/ without any visible challenge</p>",
            ),
        ),
        (_, "/m/control-plain") => (
            200,
            ok,
            shell("plain", "<h1>synthetic</h1><p>nothing here</p>"),
        ),
        _ => (404, ok, shell("missing", "<h1>synthetic 404</h1>")),
    }
}

async fn contract(root: PathBuf) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = match listener.local_addr().unwrap() {
        std::net::SocketAddr::V4(v) => v,
        _ => unreachable!(),
    };
    let server = tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut bytes = vec![0; 16384];
                let n = s.read(&mut bytes).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&bytes[..n]).into_owned();
                let target = req.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let (host, path) = reqwest::Url::parse(&target)
                    .map(|u| (u.host_str().unwrap_or("").to_owned(), u.path().to_owned()))
                    .unwrap_or_else(|_| ("one.test".into(), target));
                let (status, headers, body) = route(&host, &path);
                let mut head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                let _ = s.write_all(format!("{head}\r\n{body}").as_bytes()).await;
            });
        }
    });
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "personal".into(),
        Profile {
            sites: vec![Site {
                name: "local".into(),
                rw: vec!["http://one.test".into()],
                login: vec!["http://login.test".into()],
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let m = Manager::open(Options {
        data_dir: root.clone(),
        binary: "/usr/bin/google-chrome-stable".into(),
        proxy,
        profiles,
        compact: false,
        version: VersionPolicy::default(),
        chrome_args: vec![],
    })
    .await
    .unwrap();
    let monitor = m.start();
    // Ephemeral session: every synthetic origin is admissible, so each
    // detector kind can be served under its own hostname.
    let v: Value = m.open_session("owner", Open::default()).await.unwrap();
    let sid = v["session_id"].as_str().unwrap();
    let tid = v["tabs"][0]["tab_id"].as_str().unwrap();

    let mut rows = Vec::new();
    async fn navigate(m: &Manager, sid: &str, tid: &str, url: &str) -> Value {
        m.observe(
            "owner",
            sid,
            tid,
            "nav.go",
            json!({"url":url,"criteria":{"timeout_ms":8000}}),
        )
        .await
        .unwrap_or_else(|e| panic!("nav {url}: {e}"))
    }
    // (url, expected kind or "unblocked", expected confidence-word or vendor note)
    let matrix = [
        (
            "http://one.test/m/captcha-frame",
            "blocked_captcha",
            "high",
            "www.google.com",
        ),
        (
            "http://one.test/m/captcha-marker",
            "blocked_captcha",
            "medium",
            "site",
        ),
        (
            "http://one.test/m/site-denied",
            "blocked_by_site",
            "medium",
            "one.test",
        ),
        (
            "http://one.test/m/site-verify",
            "blocked_by_site",
            "high",
            "one.test",
        ),
        (
            "http://one.test/m/cf-header",
            "blocked_captcha",
            "high",
            "cloudflare",
        ),
        ("http://one.test/m/otp", "needs_2fa", "medium", "one.test"),
        (
            "http://one.test/m/otp-group",
            "needs_2fa",
            "medium",
            "one.test",
        ),
    ];
    for (url, kind, confidence, vendor) in matrix {
        let wait = navigate(&m, sid, tid, url).await;
        assert_eq!(wait["status"], "blocked", "{url}: {wait}");
        assert_eq!(wait["blocked"]["kind"], kind, "{url}: {wait}");
        assert_eq!(wait["blocked"]["confidence"], confidence, "{url}: {wait}");
        assert_eq!(wait["blocked"]["vendor"], vendor, "{url}: {wait}");
        rows.push(json!({"page":url,"kind":wait["blocked"]["kind"],"vendor":wait["blocked"]["vendor"],"confidence":wait["blocked"]["confidence"],"signals":wait["blocked"]["signals"]}));
    }
    // Control pages carry 9.1-adjacent markers but no block: an invisible
    // reCAPTCHA badge, an injected challenge-platform path and plain text.
    for url in [
        "http://one.test/m/control-badge",
        "http://one.test/m/control-cdn",
        "http://one.test/m/control-plain",
    ] {
        let wait = navigate(&m, sid, tid, url).await;
        assert!(wait["blocked"].is_null(), "{url}: unexpected block {wait}");
        rows.push(json!({"page":url,"kind":"unblocked","status":wait["status"]}));
    }
    m.end_session("owner", sid, false).await.unwrap();
    // A visible password field on a logged-in persistent site reports login.
    let site: Value = m
        .open_session(
            "owner",
            Open {
                profile: "personal".into(),
                site: Some("local".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let logged_sid = site["session_id"].as_str().unwrap();
    let logged_tid = site["tabs"][0]["tab_id"].as_str().unwrap();
    m.site_state("owner", logged_sid, "logged_in")
        .await
        .unwrap();
    let wait = navigate(
        &m,
        logged_sid,
        logged_tid,
        "http://one.test/m/login-password",
    )
    .await;
    assert_eq!(wait["status"], "blocked", "{wait}");
    assert_eq!(wait["blocked"]["kind"], "needs_login", "{wait}");
    rows.push(json!({"page":"http://one.test/m/login-password","kind":wait["blocked"]["kind"],"vendor":wait["blocked"]["vendor"],"confidence":wait["blocked"]["confidence"],"signals":wait["blocked"]["signals"]}));
    // Login origins are human-only even for input: the persistent session
    // denies them outright (C2 gate), so identity origins never load for the
    // agent. Preloaded-HSTS identity hosts would force TLS before the plain
    // HTTP fixture sees them; the denial below is the deterministic
    // real-Chrome evidence for that path.
    let denied = m
        .observe(
            "owner",
            logged_sid,
            logged_tid,
            "nav.go",
            json!({"url":"http://login.test/","criteria":{"timeout_ms":2000}}),
        )
        .await;
    assert_eq!(
        denied.unwrap_err().kind,
        "denied",
        "login origin must stay human-only"
    );
    rows.push(json!({"page":"http://login.test/","kind":"denied","reason":"human_only_origin"}));
    m.end_session("owner", logged_sid, false).await.unwrap();

    fs::write(
        root.join("matrix.json"),
        serde_json::to_vec_pretty(&json!({"rows": rows})).unwrap(),
    )
    .unwrap();
    monitor.abort();
    let _ = monitor.await;
    m.shutdown().await.unwrap();
    server.abort();
}

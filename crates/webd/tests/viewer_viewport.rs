//! Viewer 18.5 small/large viewport review over real Chrome (synthetic only).
//!
//! The intervention shell, stylesheet and client script are served from the
//! local fixture proxy; no external sites are involved. Evidence (metrics
//! plus screenshots) lands in `target/test-evidence/browser/viewer/`.
use excalibur_browser::{
    sessions::{Manager, Open, Options},
    version::VersionPolicy,
};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
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
fn viewer_viewports() {
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
        .args([
            "--ignored",
            "--exact",
            "viewer_viewport_child",
            "--nocapture",
        ])
        .env("EXCALIBUR_SESSION_DIR", d.path())
        .output()
        .unwrap();
    let evidence =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-evidence/browser/viewer");
    fs::create_dir_all(&evidence).unwrap();
    fs::write(
        evidence.join("runner.log"),
        [&output.stdout[..], &output.stderr[..]].concat(),
    )
    .unwrap();
    for entry in fs::read_dir(d.path()).unwrap().filter_map(Result::ok) {
        let keep = entry
            .path()
            .extension()
            .is_some_and(|e| e == "json" || e == "png");
        if keep {
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
fn viewer_viewport_child() {
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

async fn blob(m: &Manager, hash: &str) -> Vec<u8> {
    let mut output = vec![];
    loop {
        let result = m
            .observation_blob("owner", hash, output.len() as u64, 1048576)
            .await
            .unwrap();
        let data = result["data"].as_str().unwrap();
        output.extend(decode_b64(data));
        if output.len() as u64 == result["total_bytes"].as_u64().unwrap() {
            break;
        }
    }
    output
}

/// Minimal JPEG dimension reader (SOF0/SOF2 scan, no image dependency).
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(0..2) != Some(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        let marker = bytes[i + 1];
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if i + 4 > bytes.len() {
            return None;
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if marker == 0xC0 || marker == 0xC2 {
            if i + 9 > bytes.len() {
                return None;
            }
            let height = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            let width = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
            return Some((width, height));
        }
        i += 2 + len;
    }
    None
}

fn decode_b64(data: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut bits = 0u32;
    let mut acc = 0u32;
    for ch in data.chars() {
        let v = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            '=' => break,
            _ => continue,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
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
                let mut bytes = vec![0; 8192];
                let n = s.read(&mut bytes).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&bytes[..n]).into_owned();
                // Chrome talks to the fixture through the proxy, so the
                // request target is an absolute URI: strip it to a path.
                let target = req.split_whitespace().nth(1).unwrap_or("/");
                let path = target
                    .split('?')
                    .next()
                    .and_then(|t| t.rsplit('/').next().map(|l| format!("/{l}")))
                    .unwrap_or_default();
                let (content_type, body) = match path.as_str() {
                    "/viewer-shell" => (
                        "text/html",
                        excalibur_webd::viewer::render_shell("https://one.test", ""),
                    ),
                    "/viewer.js" => (
                        "application/javascript",
                        excalibur_webd::viewer::VIEWER_JS.into(),
                    ),
                    "/style.css" => ("text/css", excalibur_webd::viewer::STYLE_CSS.into()),
                    _ => ("text/html", "<h1>synthetic 404</h1>".into()),
                };
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    let m = Manager::open(Options {
        data_dir: root.clone(),
        binary: "/usr/bin/google-chrome-stable".into(),
        proxy,
        profiles: std::collections::BTreeMap::new(),
        compact: false,
        version: VersionPolicy::default(),
        chrome_args: vec![],
    })
    .await
    .unwrap();
    let monitor = m.start();
    let v: Value = m.open_session("owner", Open::default()).await.unwrap();
    let sid = v["session_id"].as_str().unwrap();
    let new: Value = m
        .new_tab("owner", sid, "http://one.test/viewer-shell", None)
        .await
        .unwrap();
    let tid = new["tab_id"].as_str().unwrap();

    async fn snapshot_png(m: &Manager, sid: &str, tid: &str) -> (Value, Vec<u8>) {
        let snap: Value = m
            .observe(
                "owner",
                sid,
                tid,
                "observe.snapshot",
                json!({"options":{"screenshot":true,"dom":"full"}}),
            )
            .await
            .unwrap();
        let png = blob(m, snap["screenshot_ref"]["sha256"].as_str().unwrap()).await;
        (snap, png)
    }

    // Small viewport: canvas fits the width, controls stack at >= 44 px.
    let small: Value = m
        .observe(
            "owner",
            sid,
            tid,
            "emulate.viewport",
            json!({"preset":"small"}),
        )
        .await
        .unwrap();
    assert_eq!(small["width"], json!(360));
    let (snap, png) = snapshot_png(&m, sid, tid).await;
    assert_eq!(jpeg_size(&png).map(|(w, _)| w), Some(360));
    assert!(
        snap["text"]
            .as_str()
            .unwrap_or("")
            .contains("https://one.test"),
        "committed origin header missing"
    );
    let nodes = dom_nodes(&m, &snap).await;
    let mut buttons = 0;
    for id in ["done", "giveup", "back", "reload"] {
        let node = nodes
            .iter()
            .find(|n| n["attributes"]["id"] == id)
            .unwrap_or_else(|| panic!("viewer button #{id} missing"));
        assert!(
            node["bbox"]["h"].as_f64().unwrap_or(0.) >= 44.,
            "touch target below 44 px: {node}"
        );
        buttons += 1;
    }
    assert_eq!(buttons, 4);
    let canvas = nodes
        .iter()
        .find(|n| n["tag"] == "canvas")
        .expect("viewer canvas missing");
    assert!(
        canvas["bbox"]["w"].as_f64().unwrap_or(9999.) <= 360.,
        "canvas overflows small viewport: {canvas}"
    );
    // No node extends past the 360 px viewport: no horizontal scroll.
    for node in &nodes {
        let (x, w) = (
            node["bbox"]["x"].as_f64().unwrap_or(0.),
            node["bbox"]["w"].as_f64().unwrap_or(0.),
        );
        assert!(x + w <= 360.5, "node overflows small viewport: {node}");
    }
    fs::write(root.join("small.png"), &png).unwrap();

    // Large viewport: shell renders 1:1 up to 1280 px and centered.
    let large: Value = m
        .observe(
            "owner",
            sid,
            tid,
            "emulate.viewport",
            json!({"preset":"large"}),
        )
        .await
        .unwrap();
    assert_eq!(large["width"], json!(1440));
    let (snap2, png2) = snapshot_png(&m, sid, tid).await;
    assert_eq!(jpeg_size(&png2).map(|(w, _)| w), Some(1440));
    let nodes2 = dom_nodes(&m, &snap2).await;
    let canvas2 = nodes2
        .iter()
        .find(|n| n["tag"] == "canvas")
        .expect("viewer canvas missing");
    assert!(
        (canvas2["bbox"]["w"].as_f64().unwrap_or(0.) - 1280.).abs() < 2.,
        "large canvas is not 1:1 at 1280 px: {canvas2}"
    );
    fs::write(root.join("large.png"), &png2).unwrap();

    async fn dom_nodes(m: &Manager, snap: &Value) -> Vec<Value> {
        let raw = blob(m, snap["dom_tree_ref"]["sha256"].as_str().unwrap()).await;
        serde_json::from_slice::<Value>(&raw)
            .unwrap()
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fs::write(
        root.join("viewports.json"),
        serde_json::to_vec_pretty(
            &json!({"small": {"width": small["width"]}, "large": {"width": large["width"]}}),
        )
        .unwrap(),
    )
    .unwrap();
    monitor.abort();
    let _ = monitor.await;
    m.shutdown().await.unwrap();
    server.abort();
}

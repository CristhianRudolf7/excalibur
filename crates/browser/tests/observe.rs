use excalibur_browser::{
    rpc::{Protocol, Request},
    sessions::{Manager, Open, Options, Profile, Site},
    version::VersionPolicy,
};
use excalibur_cdp::protocol::Method;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
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
fn readiness_and_observation() {
    run_fixture("observation_child", "observation");
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
    let evidence = Path::new(env!("CARGO_MANIFEST_DIR"))
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
fn observation_child() {
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
async fn eval(m: &Manager, run: &str, sid: &str, tid: &str, source: &str) -> Value {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(30) {
        let (c, s) = match m.diagnostic_client(run, sid, tid).await {
            Ok(value) => value,
            Err(error) if matches!(error.kind.as_str(), "tab_crashed" | "session_recovering") => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            Err(error) => panic!("fixture authority error: {error}"),
        };
        let frame = match c.send(Method::PageGetFrameTree, json!({}), Some(&s)).await {
            Ok(v) => v["frameTree"]["frame"]["id"].as_str().unwrap().to_owned(),
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let world = c
            .send(
                Method::PageCreateIsolatedWorld,
                json!({"frameId":frame,"worldName":"fixture"}),
                Some(&s),
            )
            .await;
        let Ok(world) = world else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let result=c.send(Method::RuntimeCallFunctionOn,json!({"executionContextId":world["executionContextId"],"functionDeclaration":format!("async function(){{{source}}}"),"awaitPromise":true,"returnByValue":true}),Some(&s)).await;
        if let Ok(v) = result
            && v["exceptionDetails"].is_null()
        {
            return v["result"]["value"].clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("fixture JS failed: {source}")
}
fn ids(v: &Value) -> (String, String) {
    (
        v["session_id"].as_str().unwrap().into(),
        v["tabs"][0]["tab_id"].as_str().unwrap().into(),
    )
}
async fn observe(m: &Manager, s: &str, t: &str, method: &str, p: Value) -> Value {
    m.observe("owner", s, t, method, p)
        .await
        .unwrap_or_else(|e| panic!("{method}: {e}"))
}
async fn navigate(m: &Manager, s: &str, t: &str, path: &str) -> Value {
    observe(
        m,
        s,
        t,
        "nav.go",
        json!({"url":format!("http://one.test{path}"),"criteria":{"timeout_ms":8000}}),
    )
    .await
}
async fn blob(m: &Manager, hash: &str) -> Vec<u8> {
    use base64::Engine;
    let mut output = vec![];
    loop {
        let result = m
            .observation_blob("owner", hash, output.len() as u64, 1048576)
            .await
            .unwrap();
        output.extend(
            base64::engine::general_purpose::STANDARD
                .decode(result["data"].as_str().unwrap())
                .unwrap(),
        );
        if output.len() as u64 == result["total_bytes"].as_u64().unwrap() {
            break;
        }
    }
    output
}
fn page(path: &str) -> String {
    let body=match path{
"/static"=>"<h1>Static</h1><button id='target'>Save</button>".into(),
"/slowdata"=>"<main id=slow>LOADING_MARKER</main><script>fetch('/slow').then(r=>r.text()).then(v=>slow.textContent=v)</script>".into(),
"/spa"=>"<main id='data'>loading</main><script>fetch('/delay').then(r=>r.text()).then(v=>{data.textContent=v;data.className='loaded';window.loaded=performance.now()})</script>".into(),
"/ws"=>"<button>Socket</button><script>const ws=new WebSocket('ws://one.test/socket');ws.onmessage=()=>{window.messages=(window.messages||0)+1}</script>".into(),
"/long"=>"<button id='target'>long</button><script>fetch('/poll')</script>".into(),
"/mutate"=>"<div id='counter'></div><script>setInterval(()=>counter.textContent=Date.now(),100)</script>".into(),
"/dialog"=>"<script>setTimeout(()=>alert('synthetic'),100)</script>".into(),
"/captcha"=>"<iframe width='320' height='140' src='http://www.google.com/recaptcha/api2/anchor'></iframe>".into(),
"/child"=>"<button id='child'>Frame button</button>".into(),
"/huge"=>(0..5000).map(|n|format!("<button>Control {n}</button>")).collect::<String>(),
"/controls"=>{
let mut body="<style>body{margin:0}button{height:22px;width:95px}iframe{position:absolute;top:450px;width:300px;height:80px}#cross{left:330px}</style><main><h1>Controls</h1>".to_owned();
for n in 0..300{body+=&format!("<button id='b{n}'>Button {n}</button>")}
body+="<div id='open'></div><div id='closed'></div><iframe src='/child'></iframe><iframe id='cross' src='http://two.test/child'></iframe><input id='password' type='password' value='PASSWORD_SENTINEL'><input autocomplete='one-time-code' value='OTP_SENTINEL'><input autocomplete='cc-number' value='CARD_SENTINEL'><button style='display:none' id='hidden'>Hidden</button><button style='opacity:0' id='transparent'>Transparent</button><button style='visibility:hidden' id='invisible'>Invisible</button><fieldset disabled><button id='disabled'>Disabled</button></fieldset><button style='position:absolute;top:9000px'>Far</button><button id='covered' style='position:absolute;left:1000px;top:10px'>Covered</button><div style='position:absolute;left:1000px;top:10px;background:black;width:95px;height:22px;z-index:99'></div></main><script>for(const [id,mode] of [['open','open'],['closed','closed']])document.getElementById(id).attachShadow({mode}).innerHTML='<button>'+mode+' shadow</button>'</script>";body
},_=>"<body>local</body>".into()};
    format!(
        "<!doctype html><meta name='viewport' content='width=device-width,initial-scale=1'><title>Fixture</title>{body}"
    )
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
                let req = String::from_utf8_lossy(&bytes[..n]);
                let url = req.split_whitespace().nth(1).unwrap_or("");
                let path = reqwest::Url::parse(url)
                    .ok()
                    .map(|u| u.path().to_owned())
                    .unwrap_or_else(|| url.into());
                if path == "/socket" {
                    use base64::Engine;
                    use sha1::{Digest, Sha1};
                    let key = req
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(name, _)| name.eq_ignore_ascii_case("Sec-WebSocket-Key"))
                                .map(|(_, v)| v.trim())
                        })
                        .unwrap();
                    let accept = base64::engine::general_purpose::STANDARD.encode(Sha1::digest(
                        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"),
                    ));
                    s.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
                    for _ in 0..100 {
                        if s.write_all(&[0x81, 1, b'z']).await.is_err() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    return;
                }
                if path == "/poll" {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                if path == "/slow" {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                if path == "/delay" {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                }
                let body = if path == "/slow" {
                    "DATA_ARRIVED".into()
                } else if path == "/delay" {
                    "DATA_READY".into()
                } else {
                    if path == "/"
                        && reqwest::Url::parse(url)
                            .ok()
                            .is_some_and(|u| u.host_str() == Some("initial.test"))
                    {
                        page("/slowdata")
                    } else {
                        page(&path)
                    }
                };
                let _=s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await;
            });
        }
    });
    let m = Manager::open(Options {
        data_dir: root.clone(),
        binary: "/usr/bin/google-chrome-stable".into(),
        proxy,
        profiles: BTreeMap::from([(
            "personal".into(),
            Profile {
                sites: vec![
                    Site {
                        name: "local".into(),
                        rw: vec!["http://one.test".into()],
                        listen: true,
                        ..Default::default()
                    },
                    Site {
                        name: "initial".into(),
                        rw: vec!["http://initial.test".into()],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        )]),
        compact: false,
        version: VersionPolicy::default(),
        chrome_args: vec![],
    })
    .await
    .unwrap();
    let monitor = m.start();
    let (s, t) = ids(&m
        .open_session(
            "owner",
            Open {
                ..Default::default()
            },
        )
        .await
        .unwrap());
    // Initial navigation must be observed even without a prior observation API call.
    let initial = m
        .new_tab("owner", &s, "http://one.test/slowdata", None)
        .await
        .unwrap();
    let initial_id = initial["tab_id"].as_str().unwrap();
    let initial_wait = observe(
        &m,
        &s,
        initial_id,
        "observe.wait_ready",
        json!({"criteria":{"timeout_ms":1200,"strict_network":true}}),
    )
    .await;
    assert_eq!(initial_wait["status"], "timeout", "{initial_wait}");
    assert_eq!(initial_wait["criteria_met"]["network"], false);
    assert!(
        initial_wait["inflight_sample"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap_or("").ends_with("/slow")),
        "{initial_wait}"
    );
    let initial_snapshot = observe(&m, &s, initial_id, "observe.snapshot", json!({})).await;
    assert_eq!(initial_snapshot["wait"]["status"], "ready");
    assert_eq!(
        eval(
            &m,
            "owner",
            &s,
            initial_id,
            "return document.querySelector('#slow').textContent"
        )
        .await,
        "DATA_ARRIVED"
    );
    m.close_tab("owner", &s, initial_id).await.unwrap();
    let fresh_slow = m
        .new_tab("owner", &s, "http://one.test/slowdata", None)
        .await
        .unwrap();
    let fresh_id = fresh_slow["tab_id"].as_str().unwrap();
    let fresh_snapshot = observe(&m, &s, fresh_id, "observe.snapshot", json!({})).await;
    assert_eq!(fresh_snapshot["wait"]["status"], "ready");
    assert_eq!(
        eval(
            &m,
            "owner",
            &s,
            fresh_id,
            "return document.querySelector('#slow').textContent"
        )
        .await,
        "DATA_ARRIVED"
    );
    m.close_tab("owner", &s, fresh_id).await.unwrap();
    let (site_session, site_tab) = ids(&m
        .open_session(
            "owner",
            Open {
                profile: "personal".into(),
                site: Some("initial".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap());
    let site_initial = observe(
        &m,
        &site_session,
        &site_tab,
        "observe.wait_ready",
        json!({"criteria":{"timeout_ms":1000,"strict_network":true}}),
    )
    .await;
    assert_eq!(site_initial["status"], "timeout", "{site_initial}");
    assert_eq!(site_initial["criteria_met"]["network"], false);
    assert!(
        site_initial["inflight_sample"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "http://initial.test/slow")
    );
    m.end_session("owner", &site_session, true).await.unwrap();
    let restored = observe(
        &m,
        &site_session,
        &site_tab,
        "observe.wait_ready",
        json!({"criteria":{"timeout_ms":1000,"strict_network":true}}),
    )
    .await;
    assert_eq!(restored["status"], "timeout", "{restored}");
    assert_eq!(restored["criteria_met"]["network"], false);
    assert!(
        restored["inflight_sample"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "http://initial.test/slow")
    );
    m.end_session("owner", &site_session, false).await.unwrap();
    // A listener has intentionally unobserved intervals; strict mode must not claim idle.
    let (ls, lt) = ids(&m
        .open_session(
            "owner",
            Open {
                profile: "personal".into(),
                site: Some("local".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap());
    let late = observe(
        &m,
        &ls,
        &lt,
        "observe.wait_ready",
        json!({"criteria":{"timeout_ms":4000}}),
    )
    .await;
    assert_eq!(late["status"], "ready", "{late}");
    assert_eq!(late["criteria_met"]["network"], false);
    assert!(
        late["degraded"]
            .as_array()
            .unwrap()
            .contains(&json!("network_unobserved"))
    );
    let late_strict = observe(
        &m,
        &ls,
        &lt,
        "observe.wait_ready",
        json!({"criteria":{"timeout_ms":700,"strict_network":true}}),
    )
    .await;
    assert_eq!(late_strict["status"], "timeout");
    assert_eq!(late_strict["criteria_met"]["network"], false);
    let fresh_navigation=observe(&m,&ls,&lt,"nav.go",json!({"url":"http://one.test/static","criteria":{"timeout_ms":4000,"strict_network":true}})).await;
    assert_eq!(fresh_navigation["status"], "ready", "{fresh_navigation}");
    assert_eq!(fresh_navigation["criteria_met"]["network"], true);
    m.end_session("owner", &ls, false).await.unwrap();
    fs::write(root.join("initial_network.json"),serde_json::to_vec_pretty(&json!({"initial_wait":initial_wait,"site_initial":site_initial,"restored":restored,"initial_snapshot_wait":initial_snapshot["wait"],"fresh_snapshot_wait":fresh_snapshot["wait"],"late":late,"late_strict":late_strict,"observed_navigation":fresh_navigation})).unwrap()).unwrap();
    let static_ready = navigate(&m, &s, &t, "/static").await;
    assert_eq!(static_ready["status"], "ready");
    assert!(
        static_ready["elapsed_ms"].as_u64().unwrap() < 1000,
        "{static_ready}"
    );
    let spa = navigate(&m, &s, &t, "/spa").await;
    assert_eq!(spa["status"], "ready");
    assert_eq!(
        eval(
            &m,
            "owner",
            &s,
            &t,
            "return document.querySelector('#data').textContent"
        )
        .await,
        "DATA_READY"
    );
    let condition = observe(
        &m,
        &s,
        &t,
        "observe.wait_ready",
        json!({"criteria":{"selector":"#data.loaded"}}),
    )
    .await;
    assert_eq!(condition["status"], "ready");
    let absent = observe(
        &m,
        &s,
        &t,
        "observe.wait_ready",
        json!({"criteria":{"text":"ABSENT_CONDITION_SENTINEL","timeout_ms":500}}),
    )
    .await;
    assert_eq!(absent["status"], "timeout");
    assert_eq!(absent["criteria_met"]["condition"], false);
    let long = navigate(&m, &s, &t, "/long").await;
    assert_eq!(long["status"], "ready", "{long}");
    assert!(
        long["degraded"]
            .as_array()
            .unwrap()
            .contains(&json!("network_busy")),
        "{long}"
    );
    let strict = observe(
        &m,
        &s,
        &t,
        "observe.wait_ready",
        json!({"criteria":{"strict_network":true,"timeout_ms":800}}),
    )
    .await;
    assert_eq!(strict["status"], "timeout");
    let socket = navigate(&m, &s, &t, "/ws").await;
    assert_eq!(socket["status"], "ready", "{socket}");
    assert!(
        socket["degraded"].as_array().unwrap().is_empty(),
        "{socket}"
    );
    let mutating = navigate(&m, &s, &t, "/mutate").await;
    assert_eq!(mutating["status"], "timeout");
    assert_eq!(mutating["criteria_met"]["dom"], false);
    let dialog = navigate(&m, &s, &t, "/dialog").await;
    assert_eq!(dialog["blocked"]["kind"], "dialog", "{dialog}");
    let (c, cdp) = m.diagnostic_client("owner", &s, &t).await.unwrap();
    c.send(
        Method::PageHandleJavaScriptDialog,
        json!({"accept":false}),
        Some(&cdp),
    )
    .await
    .unwrap();
    let captcha = navigate(&m, &s, &t, "/captcha").await;
    assert_eq!(captcha["blocked"]["kind"], "blocked_captcha", "{captcha}");
    navigate(&m, &s, &t, "/controls").await;
    let (c, cdp) = m.diagnostic_client("owner", &s, &t).await.unwrap();
    let ft = c
        .send(Method::PageGetFrameTree, json!({}), Some(&cdp))
        .await
        .unwrap();
    fs::write(root.join("frames.json"),serde_json::to_vec_pretty(&json!({"tree":ft,"attached":c.attached_sessions().iter().map(|e|e.params.clone()).collect::<Vec<_>>()})).unwrap()).unwrap();
    let frame_condition = observe(
        &m,
        &s,
        &t,
        "observe.wait_ready",
        json!({"criteria":{"selector":"#child"}}),
    )
    .await;
    assert_eq!(frame_condition["status"], "ready");
    let snap = observe(&m, &s, &t, "observe.snapshot", json!({})).await;
    fs::write(
        root.join("snapshot.json"),
        serde_json::to_vec_pretty(&snap).unwrap(),
    )
    .unwrap();
    assert_eq!(snap["wait"]["status"], "ready");
    assert!(snap["text"].as_str().unwrap().len() <= 16384);
    let bytes = blob(&m, snap["dom_tree_ref"]["sha256"].as_str().unwrap()).await;
    let nodes: Value = serde_json::from_slice(&bytes).unwrap();
    let nodes = nodes.as_array().unwrap();
    let button_count = nodes.iter().filter(|n| n["tag"] == "button").count();
    fs::write(
        root.join("nodes.json"),
        serde_json::to_vec_pretty(&nodes).unwrap(),
    )
    .unwrap();
    assert_eq!(button_count, 310, "buttons={button_count}");
    for n in 0..300 {
        assert!(
            nodes
                .iter()
                .any(|v| v["attributes"]["id"] == format!("b{n}")),
            "missing control {n}"
        );
    }
    for secret in ["PASSWORD_SENTINEL", "OTP_SENTINEL", "CARD_SENTINEL"] {
        assert!(!String::from_utf8_lossy(&bytes).contains(secret));
        assert!(!snap.to_string().contains(secret));
    }
    assert!(nodes.iter().any(|n| n["name"] == "open shadow"));
    assert!(nodes.iter().any(|n| n["name"] == "closed shadow"));
    let frames = nodes
        .iter()
        .filter(|n| n["tag"] == "button" && n["attributes"]["id"] == "child")
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert!(
        frames
            .iter()
            .all(|n| n["reference"].as_str().unwrap().starts_with('f'))
    );
    assert!(
        frames
            .iter()
            .any(|n| n["bbox"]["x"].as_f64().unwrap() > 330.)
    );
    for id in ["hidden", "transparent", "invisible", "covered"] {
        assert_eq!(
            nodes.iter().find(|n| n["attributes"]["id"] == id).unwrap()["visible"],
            false,
            "{id}"
        );
    }
    assert_eq!(
        nodes
            .iter()
            .find(|n| n["attributes"]["id"] == "disabled")
            .unwrap()["enabled"],
        false
    );
    let raster = blob(&m, snap["screenshot_ref"]["sha256"].as_str().unwrap()).await;
    let raster = image::load_from_memory_with_format(&raster, image::ImageFormat::Jpeg)
        .unwrap()
        .to_rgb8();
    let password = nodes
        .iter()
        .find(|n| n["attributes"]["id"] == "password")
        .unwrap();
    let px = (password["bbox"]["x"].as_f64().unwrap()
        + password["bbox"]["w"].as_f64().unwrap() / 2.) as u32;
    let py = (password["bbox"]["y"].as_f64().unwrap()
        + password["bbox"]["h"].as_f64().unwrap() / 2.) as u32;
    if px < raster.width() && py < raster.height() {
        assert!(raster.get_pixel(px, py).0.iter().all(|c| *c < 30));
    }
    let snap2 = observe(&m, &s, &t, "observe.snapshot", json!({})).await;
    assert!(
        snap2["elements"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["new"] == false)
    );
    let r = snap2["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "Button 0")
        .unwrap()["ref"]
        .as_str()
        .unwrap();
    let id = snap2["snapshot_id"].as_str().unwrap();
    let valid = observe(
        &m,
        &s,
        &t,
        "observe.validate_ref",
        json!({"snapshot_id":id,"ref":r}),
    )
    .await;
    assert_eq!(valid["status"], "valid", "{valid}");
    let old = observe(
        &m,
        &s,
        &t,
        "observe.validate_ref",
        json!({"snapshot_id":snap["snapshot_id"],"ref":r}),
    )
    .await;
    assert_eq!(old["reason"], "superseded");
    assert_eq!(old["remapped_ref"], r);
    let subtree = observe(
        &m,
        &s,
        &t,
        "observe.subtree",
        json!({"snapshot_id":id,"ref":r}),
    )
    .await;
    assert!(!subtree["nodes"].as_array().unwrap().is_empty());
    eval(
        &m,
        "owner",
        &s,
        &t,
        "document.querySelector('#b0').textContent='Changed';return true",
    )
    .await;
    let changed = observe(
        &m,
        &s,
        &t,
        "observe.validate_ref",
        json!({"snapshot_id":id,"ref":r}),
    )
    .await;
    assert_eq!(changed["reason"], "changed");
    eval(
        &m,
        "owner",
        &s,
        &t,
        "document.querySelector('#b0').remove();return true",
    )
    .await;
    let preserved = observe(
        &m,
        &s,
        &t,
        "observe.subtree",
        json!({"snapshot_id":id,"ref":r}),
    )
    .await;
    assert_eq!(preserved["nodes"][0]["name"], "Button 0");
    let replacement_ref = snap2["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "Button 1")
        .unwrap()["ref"]
        .clone();
    eval(&m,"owner",&s,&t,"document.querySelector('#b1').outerHTML='<button id=\"b1\">Replacement</button>';return true").await;
    let replacement = observe(
        &m,
        &s,
        &t,
        "observe.validate_ref",
        json!({"snapshot_id":id,"ref":replacement_ref}),
    )
    .await;
    assert_eq!(replacement["reason"], "changed");
    let removed = observe(
        &m,
        &s,
        &t,
        "observe.validate_ref",
        json!({"snapshot_id":id,"ref":r}),
    )
    .await;
    assert!(
        matches!(
            removed["reason"].as_str(),
            Some("node_detached" | "node_gone")
        ),
        "{removed}"
    );
    let frame_ref = frames
        .iter()
        .find(|n| n["bbox"]["x"].as_f64().unwrap() > 330.)
        .unwrap()["reference"]
        .clone();
    eval(
        &m,
        "owner",
        &s,
        &t,
        "document.querySelector('#cross').remove();return true",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let gone = observe(
        &m,
        &s,
        &t,
        "observe.validate_ref",
        json!({"snapshot_id":id,"ref":frame_ref}),
    )
    .await;
    assert_eq!(gone["reason"], "frame_gone");
    let restarted = observe(
        &m,
        &s,
        &t,
        "observe.validate_ref",
        json!({"snapshot_id":"previous-daemon:snapshot","ref":r}),
    )
    .await;
    assert_eq!(restarted["reason"], "browser_restarted");
    m.observation.invalidate(&t, "takeover").await;
    let taken = observe(
        &m,
        &s,
        &t,
        "observe.validate_ref",
        json!({"snapshot_id":id,"ref":r}),
    )
    .await;
    assert_eq!(taken["reason"], "takeover");
    assert_eq!(
        m.observation_blob(
            "intruder",
            snap["dom_tree_ref"]["sha256"].as_str().unwrap(),
            0,
            100
        )
        .await
        .unwrap_err()
        .kind,
        "denied"
    );
    let before_nav = observe(&m, &s, &t, "observe.snapshot", json!({})).await;
    let before_ref = before_nav["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "Button 2")
        .unwrap()["ref"]
        .clone();
    navigate(&m, &s, &t, "/static").await;
    let navigated = observe(
        &m,
        &s,
        &t,
        "observe.validate_ref",
        json!({"snapshot_id":before_nav["snapshot_id"],"ref":before_ref}),
    )
    .await;
    assert_eq!(navigated["reason"], "navigated");
    let background = m
        .new_tab("owner", &s, "http://one.test/static", None)
        .await
        .unwrap();
    let fresh = observe(
        &m,
        &s,
        background["tab_id"].as_str().unwrap(),
        "observe.snapshot",
        json!({}),
    )
    .await;
    assert_eq!(fresh["wait"]["status"], "ready");
    assert!(fresh["wait"]["elapsed_ms"].as_u64().unwrap() >= 500);
    let started = Instant::now();
    let background_snap = observe(&m, &s, &t, "observe.snapshot", json!({})).await;
    assert!(background_snap["screenshot_ref"].is_object());
    assert!(started.elapsed() < Duration::from_secs(5));
    m.close_tab("owner", &s, background["tab_id"].as_str().unwrap())
        .await
        .unwrap();
    let huge_ready = navigate(&m, &s, &t, "/huge").await;
    assert_eq!(huge_ready["status"], "ready");
    let huge_started = Instant::now();
    let huge = observe(&m, &s, &t, "observe.snapshot", json!({})).await;
    assert!(huge_started.elapsed() <= Duration::from_secs(3) || huge["partial"] == true);
    assert!(huge["omitted"].as_u64().unwrap() > 0);
    assert!(huge["text"].as_str().unwrap().len() <= 16384);
    let subtree_protocol = Protocol::new(m.clone());
    let subtree_response=subtree_protocol.request(Request{jsonrpc:"2.0".into(),id:json!(2),method:"observe.subtree".into(),params:json!({"run_id":"owner","deadline_ms":10000,"session_id":s,"tab_id":t,"snapshot_id":huge["snapshot_id"]})}).await;
    let large_subtree = &subtree_response["result"]["result"];
    assert_eq!(large_subtree["delivery"], "blob", "{subtree_response}");
    assert_eq!(
        large_subtree["source_blob"]["sha256"],
        huge["dom_tree_ref"]["sha256"]
    );
    assert_eq!(large_subtree["node_count"], 10008);
    assert!(
        serde_json::to_vec(&subtree_response).unwrap().len() < excalibur_browser::rpc::MAX_MESSAGE
    );
    assert!(large_subtree["nodes"].as_array().unwrap().is_empty());
    fs::write(
        root.join("subtree_delivery.json"),
        serde_json::to_vec_pretty(&subtree_response).unwrap(),
    )
    .unwrap();
    let full = blob(&m, huge["dom_tree_ref"]["sha256"].as_str().unwrap()).await;
    let full: Value = serde_json::from_slice(&full).unwrap();
    assert_eq!(
        full.as_array()
            .unwrap()
            .iter()
            .filter(|n| n["tag"] == "button")
            .count(),
        5000
    );
    navigate(&m, &s, &t, "/static").await;
    observe(&m, &s, &t, "emulate.viewport", json!({"preset":"small"})).await;
    let small = observe(
        &m,
        &s,
        &t,
        "observe.snapshot",
        json!({"options":{"screenshot":true}}),
    )
    .await;
    let screenshot = blob(&m, small["screenshot_ref"]["sha256"].as_str().unwrap()).await;
    let image = image::load_from_memory_with_format(&screenshot, image::ImageFormat::Jpeg).unwrap();
    assert_eq!(image.width(), 360);
    assert_eq!(eval(&m, "owner", &s, &t, "return innerWidth").await, 360);
    assert_eq!(
        eval(
            &m,
            "owner",
            &s,
            &t,
            "return matchMedia('(max-width: 400px)').matches"
        )
        .await,
        true
    );
    assert_eq!(m.settle_observation("owner", &s, &t).await["settled"], true);
    let console_before = observe(&m, &s, &t, "devtools.console", json!({})).await;
    eval(&m,"owner",&s,&t,"for(let i=0;i<600;i++)console.log('fixture',i);await (await fetch('/authorized?private=query',{headers:{Authorization:'Bearer AUTH_SENTINEL'}})).text();return true").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let console = observe(&m, &s, &t, "devtools.console", json!({"limit":500})).await;
    assert_eq!(console["records"].as_array().unwrap().len(), 500);
    assert!(console["next_seq"].as_u64().unwrap() > console_before["next_seq"].as_u64().unwrap());
    let network = observe(
        &m,
        &s,
        &t,
        "devtools.network",
        json!({"tainted":true,"limit":500}),
    )
    .await;
    assert!(!network.to_string().contains("AUTH_SENTINEL"));
    assert!(!network.to_string().contains("private=query"));
    assert!(
        network["records"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v.get("headers").is_none() && v.get("body").is_none())
    );
    let performance = observe(&m, &s, &t, "devtools.performance", json!({})).await;
    assert!(!performance["metrics"].as_array().unwrap().is_empty());
    let final_wait = observe(
        &m,
        &s,
        &t,
        "observe.wait_ready",
        json!({"criteria":{"timeout_ms":3000}}),
    )
    .await;
    fs::write(
        root.join("final_wait.json"),
        serde_json::to_vec_pretty(&final_wait).unwrap(),
    )
    .unwrap();
    assert_eq!(final_wait["status"], "ready", "{final_wait}");
    let protocol = Protocol::new(m.clone());
    let rpc = protocol
        .request(Request {
            jsonrpc: "2.0".into(),
            id: json!(1),
            method: "observe.wait_ready".into(),
            params: json!({"run_id":"owner","deadline_ms":5000,"criteria":{"timeout_ms":3000},"session_id":s,"tab_id":t}),
        })
        .await;
    assert_eq!(rpc["result"]["result"]["status"], "ready", "{rpc}");
    fs::write(root.join("contract.json"),serde_json::to_vec_pretty(&json!({"static":static_ready,"websocket":socket,"spa":spa,"long_poll":long,"strict":strict,"mutating":mutating,"dialog":dialog,"captcha":captcha,"snapshot":snap,"small":small,"huge":{"omitted":huge["omitted"],"partial":huge["partial"],"nodes":full.as_array().unwrap().len()},"console_count":console["records"].as_array().unwrap().len(),"network":network})).unwrap()).unwrap();
    monitor.abort();
    let _ = monitor.await;
    m.shutdown().await.unwrap();
    server.abort();
}

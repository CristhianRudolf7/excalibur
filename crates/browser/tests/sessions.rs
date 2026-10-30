use excalibur_browser::{
    rpc::{MAX_MESSAGE, Protocol, Request, Server},
    sessions::{ListenProbe, Manager, Open, Options, Profile, SessionState, Site},
    version::VersionPolicy,
};
use excalibur_cdp::protocol::Method;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
fn durable_sessions_and_protocol() {
    run_fixture("session_child", "sessions");
}
#[test]
fn listener_events_and_document_recovery() {
    run_fixture("reconciliation_child", "listener_reconciliation");
}
#[test]
fn other_sessions_stay_responsive_with_a_spinning_renderer() {
    run_fixture("spin_child", "renderer_isolation");
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
fn session_child() {
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
async fn call(p: &Arc<Protocol>, method: &str, key: &str, params: Value) -> Value {
    let mut params = params;
    params["op_key"] = json!(key);
    params["run_id"] = json!("rpc");
    params["deadline_ms"] = json!(30000);
    loop {
        let v = p
            .request(Request {
                jsonrpc: "2.0".into(),
                id: json!(1),
                method: method.into(),
                params: params.clone(),
            })
            .await;
        if v["result"]["state"] != "working" {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn contract(root: PathBuf) {
    operation_contract().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = match listener.local_addr().unwrap() {
        std::net::SocketAddr::V4(v) => v,
        _ => unreachable!(),
    };
    let proxy = tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut bytes = vec![0; 8192];
                let n = s.read(&mut bytes).await.unwrap_or(0);
                let poll = String::from_utf8_lossy(&bytes[..n])
                    .split_whitespace()
                    .nth(1)
                    .is_some_and(|s| s.ends_with("/poll"));
                if poll {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    let _ = s
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                    return;
                }
                let body = r#"<!doctype html><title>chat 0</title><body><span class='unread'></span><script>let ticks=0;setInterval(()=>{document.body.dataset.ticks=String(++ticks)},50);let n=0;(async()=>{for(;;){await (await fetch('/poll',{cache:'no-store'})).text();document.title='chat '+(++n);document.body.dataset.polls=String(n)}})();</script>"#;
                let _=s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await;
            });
        }
    });
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "personal".into(),
        Profile {
            sites: vec![
                Site {
                    name: "chat".into(),
                    rw: vec!["http://chat.test".into()],
                    ro: vec![],
                    login: vec!["http://login.test".into()],
                    listen: true,
                    wait: None,
                    credential_frames: vec![],
                    listen_probe: ListenProbe {
                        unread_selector: ".unread".into(),
                    },
                },
                Site {
                    name: "other".into(),
                    login: vec!["http://login.test".into()],
                    rw: vec!["http://other.test".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
    );
    let options = Options {
        data_dir: root.clone(),
        binary: "/usr/bin/google-chrome-stable".into(),
        proxy: address,
        profiles,
        compact: false,
        version: VersionPolicy::default(),
        chrome_args: vec![],
    };
    let m = Manager::open(options.clone()).await.unwrap();
    assert!(Manager::open(options.clone()).await.is_err());
    let monitor = m.start();
    let request = Open {
        profile: "personal".into(),
        site: Some("chat".into()),
        ..Default::default()
    };
    let (sid, tid) = ids(&m.open_session("one", request.clone()).await.unwrap());
    let (s2, t2) = ids(&m.open_session("two", request.clone()).await.unwrap());
    assert_eq!(tid, t2);
    assert_eq!(m.status().await["instances"].as_array().unwrap().len(), 1);
    assert_eq!(
        m.end_session("one", &sid, true).await.unwrap_err().kind,
        "listener_cannot_suspend"
    );
    assert_eq!(m.tabs("intruder", &sid).await.unwrap_err().kind, "denied");
    assert_eq!(
        m.new_tab("one", &sid, "http://login.test", None)
            .await
            .unwrap_err()
            .kind,
        "denied"
    );
    // Persistent storage, including session cookies, is measured on the actual binary.
    let stored=eval(&m,"one",&sid,&tid,r#"if(location.hostname!=='chat.test')throw Error('loading');document.cookie='persistent=yes;Max-Age=86400;Path=/';document.cookie='session_cookie=yes;Path=/';localStorage.setItem('fixture','saved');await new Promise((resolve,reject)=>{const r=indexedDB.open('fixture',1);r.onupgradeneeded=()=>r.result.createObjectStore('kv');r.onsuccess=()=>{const db=r.result;const tx=db.transaction('kv','readwrite');tx.objectStore('kv').put('saved','key');tx.oncomplete=()=>{db.close();resolve()};tx.onerror=reject};r.onerror=reject});return document.cookie"#).await;
    assert!(stored.as_str().unwrap().contains("persistent=yes"));
    m.site_state("one", &sid, "logged_in").await.unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await;
    eval(&m,"one",&sid,&tid,"const span=document.createElement('span');span.className='unread';document.body.append(span);return true").await;
    wait_count(&m, 0, 2).await;
    let guard = m.lease("one", &sid, &tid, false).await.unwrap();
    let queued = m.clone();
    let qs = s2.clone();
    let qt = tid.clone();
    let start = Instant::now();
    let waiting = tokio::spawn(async move { queued.lease("two", &qs, &qt, false).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!waiting.is_finished());
    drop(guard);
    drop(waiting.await.unwrap().unwrap());
    assert!(start.elapsed() >= Duration::from_millis(150));
    let guard = m.lease("one", &sid, &tid, false).await.unwrap();
    let operation = Protocol::new(m.clone());
    let params =
        json!({"op_key":"queued","run_id":"one","deadline_ms":10,"session_id":sid,"tab_id":tid});
    let working = operation
        .request(Request {
            jsonrpc: "2.0".into(),
            id: json!(7),
            method: "tab.switch".into(),
            params: params.clone(),
        })
        .await;
    assert_eq!(working["result"]["state"], "working");
    let duplicate = operation
        .request(Request {
            jsonrpc: "2.0".into(),
            id: json!(7),
            method: "tab.switch".into(),
            params: params.clone(),
        })
        .await;
    assert_eq!(working, duplicate);
    drop(guard);
    let mut params = params;
    params["deadline_ms"] = json!(30000);
    let completed = operation
        .request(Request {
            jsonrpc: "2.0".into(),
            id: json!(7),
            method: "tab.switch".into(),
            params,
        })
        .await;
    assert_eq!(completed["result"]["state"], "completed");
    operation.drain().await;
    drop(operation);
    // Full 30-second queue boundary against the same shared tab.
    let guard = m.lease("one", &sid, &tid, false).await.unwrap();
    let start = Instant::now();
    assert_eq!(
        m.lease("two", &s2, &tid, false).await.unwrap_err().kind,
        "tab_busy"
    );
    assert!(start.elapsed() >= Duration::from_secs(30));
    drop(guard);
    let (e1, et1) = ids(&m.open_session("e1", Open::default()).await.unwrap());
    let (e2, et2) = ids(&m.open_session("e2", Open::default()).await.unwrap());
    let (_, et3) = ids(&m.open_session("e3", Open::default()).await.unwrap());
    assert!(!et3.is_empty());
    assert_eq!(
        m.open_session("e4", Open::default())
            .await
            .unwrap_err()
            .kind,
        "capacity_exceeded"
    );
    // Navigate using the private diagnostic client; RPC deliberately exposes no raw CDP.
    for (run, s, t) in [("e1", &e1, &et1), ("e2", &e2, &et2)] {
        let (c, cs) = m.diagnostic_client(run, s, t).await.unwrap();
        c.send(
            Method::PageNavigate,
            json!({"url":"http://chat.test"}),
            Some(&cs),
        )
        .await
        .unwrap();
    }
    let cookie=eval(&m,"e1",&e1,&et1,"if(location.hostname!=='chat.test')throw Error('loading');document.cookie='isolated=one';return document.cookie").await;
    assert!(cookie.as_str().unwrap().contains("isolated=one"));
    assert!(
        !eval(
            &m,
            "e2",
            &e2,
            &et2,
            "if(location.hostname!=='chat.test')throw Error('loading');return document.cookie"
        )
        .await
        .as_str()
        .unwrap()
        .contains("isolated=one")
    );
    m.end_session("e2", &e2, true).await.unwrap();
    let resumed = m.tabs("e2", &e2).await.unwrap();
    assert_eq!(resumed[0]["tab_id"], et2);
    assert!(
        !eval(
            &m,
            "e2",
            &e2,
            &et2,
            "if(location.hostname!=='chat.test')throw Error('loading');return document.cookie"
        )
        .await
        .as_str()
        .unwrap()
        .contains("isolated=one")
    );
    for n in 0..3 {
        m.new_tab("e1", &e1, "about:blank", Some(format!("task{n}")))
            .await
            .unwrap();
    }
    assert_eq!(
        m.new_tab("e1", &e1, "about:blank", None)
            .await
            .unwrap_err()
            .kind,
        "tab_limit"
    );
    // Count up to the global limit using persistent non-listening sessions owned by distinct runs.
    let mut global_hit = false;
    for n in 0..12 {
        let result = m
            .open_session(
                &format!("p{n}"),
                Open {
                    profile: "personal".into(),
                    site: Some("other".into()),
                    ..Default::default()
                },
            )
            .await;
        if let Err(e) = result {
            assert_eq!(e.kind, "tab_limit");
            global_hit = true;
            break;
        }
    }
    assert!(global_hit);
    // Dispose all transient sessions before shutdown; directory must not retain their cookie marker.
    for (run, s) in [("e1", &e1), ("e2", &e2)] {
        m.end_session(run, s, false).await.unwrap();
    }
    let next = m.open_session("e1", Open::default()).await.unwrap();
    let (ns, nt) = ids(&next);
    let (c, cs) = m.diagnostic_client("e1", &ns, &nt).await.unwrap();
    c.send(
        Method::PageNavigate,
        json!({"url":"http://chat.test"}),
        Some(&cs),
    )
    .await
    .unwrap();
    assert!(
        !eval(
            &m,
            "e1",
            &ns,
            &nt,
            "if(location.hostname!=='chat.test')throw Error('loading');return document.cookie"
        )
        .await
        .as_str()
        .unwrap()
        .contains("isolated=one")
    );
    for n in 0..12 {
        let run = format!("p{n}");
        let list = m.list(&run).await;
        if let Some(s) = list.as_array().unwrap().first()
            && s["state"] == "ready"
        {
            m.end_session(&run, s["session_id"].as_str().unwrap(), false)
                .await
                .unwrap();
        }
    }
    // Idempotency, hashing and durable completion.
    let p = Protocol::new(m.clone());
    let a = call(
        &p,
        "session.open",
        "same",
        json!({"profile":"personal","site":"other"}),
    )
    .await;
    assert!(a["error"].is_null(), "{a}");
    let b = call(
        &p,
        "session.open",
        "same",
        json!({"site":"other","profile":"personal"}),
    )
    .await;
    assert_eq!(a, b);
    let conflict = call(&p, "session.open", "same", json!({"profile":"ephemeral"})).await;
    assert_eq!(conflict["error"]["message"], "idempotency_conflict");
    let rpc_session = a["result"]["result"]["session_id"].as_str().unwrap();
    let login_tab = m
        .new_tab("rpc", rpc_session, "http://other.test", None)
        .await
        .unwrap();
    let login_tid = login_tab["tab_id"].as_str().unwrap();
    let (client, cdp) = m
        .diagnostic_client("rpc", rpc_session, login_tid)
        .await
        .unwrap();
    client
        .send(
            Method::PageNavigate,
            json!({"url":"http://login.test"}),
            Some(&cdp),
        )
        .await
        .unwrap();
    eval(
        &m,
        "rpc",
        rpc_session,
        login_tid,
        "if(location.hostname!=='login.test')throw Error('loading');return true",
    )
    .await;
    let denied = m
        .lease("rpc", rpc_session, login_tid, true)
        .await
        .unwrap_err();
    assert_eq!(denied.kind, "denied");
    assert_eq!(denied.detail, "human_only_origin");
    m.end_session("rpc", rpc_session, false).await.unwrap();
    let a = call(
        &p,
        "session.open",
        "rpc-next",
        json!({"profile":"personal","site":"other"}),
    )
    .await;
    assert!(a["error"].is_null(), "{a}");
    // A real Unix socket: malformed and oversized frames do not close an authorized connection.
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let sock = root.join("webd.sock");
    let server = Server::bind(
        &sock,
        unsafe { libc::geteuid() },
        unsafe { libc::getgid() },
        p.clone(),
    )
    .unwrap();
    let serving = tokio::spawn(async move { server.run(stop).await.unwrap() });
    assert_eq!(
        fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
        0o660
    );
    let stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    write.write_all(b"invalid\n").await.unwrap();
    let mut frame = String::new();
    loop {
        frame.clear();
        tokio::time::timeout(Duration::from_secs(5), read.read_line(&mut frame))
            .await
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_str(&frame).unwrap();
        if value.get("error").is_some() {
            assert!(frame.contains("invalid_json"));
            break;
        }
        assert_eq!(value["method"], "event");
    }
    let mut huge = vec![b'x'; MAX_MESSAGE + 10];
    huge.push(b'\n');
    write.write_all(&huge).await.unwrap();
    loop {
        frame.clear();
        read.read_line(&mut frame).await.unwrap();
        if frame.contains("message_too_large") {
            break;
        }
    }
    write.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"status.get\",\"params\":{\"run_id\":\"rpc\",\"deadline_ms\":10000}}\n").await.unwrap();
    loop {
        frame.clear();
        read.read_line(&mut frame).await.unwrap();
        if serde_json::from_str::<Value>(&frame).unwrap()["id"] == 99 {
            break;
        }
    }
    assert!(frame.contains("completed"));
    stop_tx.send_replace(true);
    serving.await.unwrap();
    drop(p);
    // Persist and restore the profile; old references acquire a new document epoch.
    let before = m.tabs("one", &sid).await.unwrap()[0]["document_epoch"]
        .as_u64()
        .unwrap();
    monitor.abort();
    let _ = monitor.await;
    m.shutdown().await.unwrap();
    drop(m);
    let m = Manager::open(options.clone()).await.unwrap();
    let monitor = m.start();
    assert_eq!(
        m.site_states("personal").await.unwrap()[0]["login_state"],
        "logged_in"
    );
    let restored=eval(&m,"one",&sid,&tid,r#"if(location.hostname!=='chat.test')throw Error('loading');const value=await new Promise((resolve,reject)=>{const r=indexedDB.open('fixture');r.onsuccess=()=>{const db=r.result;const tx=db.transaction('kv');const get=tx.objectStore('kv').get('key');get.onsuccess=()=>{resolve(get.result);db.close()}};r.onerror=reject});return {cookie:document.cookie,idb:value,storage:localStorage.getItem('fixture')}"#).await;
    assert!(
        restored["cookie"]
            .as_str()
            .unwrap()
            .contains("persistent=yes")
    );
    assert_eq!(restored["idb"], "saved");
    assert_eq!(restored["storage"], "saved");
    assert!(
        m.tabs("one", &sid).await.unwrap()[0]["document_epoch"]
            .as_u64()
            .unwrap()
            > before
    );
    let sessions = m.list("e3").await;
    assert_eq!(sessions[0]["state"], "lost");
    // Kill the live listener renderer, then verify automatic reload and delivery.
    let (c, cs) = m.diagnostic_client("one", &sid, &tid).await.unwrap();
    let version = c
        .send(Method::BrowserGetVersion, json!({}), None)
        .await
        .unwrap();
    let status = m.status().await;
    let pid = status["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "personal")
        .unwrap()["pid"]
        .as_u64()
        .unwrap() as u32;
    let tree = excalibur_browser::process::tree(pid);
    for process in tree.iter().filter(|p| p.kind == "renderer") {
        unsafe {
            libc::kill(process.pid as i32, libc::SIGKILL);
        }
    }
    let _ = cs;
    assert!(
        eval(
            &m,
            "one",
            &sid,
            &tid,
            "if(location.hostname!=='chat.test')throw Error('loading');return document.title"
        )
        .await
        .is_string()
    );
    let status = m.status().await;
    let persistent_pid = status["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "personal")
        .unwrap()["pid"]
        .as_u64()
        .unwrap();
    unsafe {
        libc::kill(persistent_pid as i32, libc::SIGKILL);
    }

    eval(
        &m,
        "one",
        &sid,
        &tid,
        "if(location.hostname!=='chat.test')throw Error('loading');return true",
    )
    .await;
    assert_eq!(m.tabs("one", &sid).await.unwrap()[0]["tab_id"], tid);
    let rpc_sid = a["result"]["result"]["session_id"].as_str().unwrap();
    let other = m
        .new_tab(
            "rpc",
            rpc_sid,
            "http://other.test",
            Some("active-other-window".into()),
        )
        .await
        .unwrap();
    let other_tid = other["tab_id"].as_str().unwrap();
    let duration = std::env::var("EXCALIBUR_LISTEN_TEST_SECONDS")
        .ok()
        .map(|s| s.parse::<u64>().unwrap())
        .unwrap_or(600);
    let initial_polls = eval(
        &m,
        "one",
        &sid,
        &tid,
        "return Number(document.body.dataset.polls||0)",
    )
    .await
    .as_u64()
    .unwrap();
    let cursor = m.db.events(0).await.unwrap().last().unwrap()["cursor"]
        .as_u64()
        .unwrap();
    // The target can commit before the asynchronous document reconciliation completes.
    eval(
        &m,
        "rpc",
        rpc_sid,
        other_tid,
        "if(location.hostname!=='other.test')throw Error('loading');return true",
    )
    .await;
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let sessions = m.list("rpc").await;
        if sessions
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["session_id"] == rpc_sid && s["state"] == "ready")
        {
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "new tab did not reconcile: {sessions}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    m.switch("rpc", rpc_sid, other_tid).await.unwrap();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(duration) {
        tokio::time::sleep(Duration::from_secs(5.min(duration))).await;
        let tabs = m.tabs("two", &s2).await.unwrap();
        assert_eq!(tabs[0]["tab_id"], tid);
        assert_ne!(tabs[0]["state"], "closed");
    }
    let mut events = vec![];
    let mut after = cursor;
    loop {
        let page = m.db.events(after).await.unwrap();
        if page.is_empty() {
            break;
        }
        after = page.last().unwrap()["cursor"].as_u64().unwrap();
        events.extend(page);
    }
    let activity = events
        .iter()
        .filter(|e| e["type"] == "listener_activity")
        .collect::<Vec<_>>();
    let mut last = 0;
    for event in &activity {
        let at = event["at"].as_i64().unwrap();
        assert!(at - last >= 5);
        last = at;
        assert!(event.get("content").is_none());
    }
    let state = eval(
        &m,
        "one",
        &sid,
        &tid,
        "return {visibility:document.visibilityState,cookies:document.cookie,ticks:Number(document.body.dataset.ticks),title:document.title,polls:Number(document.body.dataset.polls||0)}",
    )
    .await;
    let poll_changes = state["polls"]
        .as_u64()
        .unwrap()
        .saturating_sub(initial_polls);
    let title_events = activity
        .iter()
        .filter(|e| {
            e["kinds"]
                .as_array()
                .is_some_and(|k| k.iter().any(|kind| kind == "title_changed"))
        })
        .count() as u64;
    eprintln!(
        "listener measurement: elapsed={} events={} state={}",
        started.elapsed().as_secs(),
        activity.len(),
        state
    );
    assert!(state["ticks"].as_u64().unwrap() > duration * 10);
    fs::write(root.join("contract.json"),serde_json::to_vec_pretty(&json!({"chrome":version,"restored":restored,"session_cookie_preserved":restored["cookie"].as_str().unwrap().contains("session_cookie=yes"),"ephemeral_isolated":true,"owner_denied":true,"tab_limits":true,"queue_timeout_seconds":30,"listener_seconds":started.elapsed().as_secs(),"initial_polls":initial_polls,"poll_changes":poll_changes,"title_events":title_events,"listener_events":activity,"listener_state":state,"renderer_recovered":true,"rpc":true})).unwrap()).unwrap();
    if duration >= 25 {
        assert!(
            poll_changes >= (duration / 20).saturating_sub(1),
            "long-poll cadence: {state}, initial={initial_polls}"
        );
        assert!(
            title_events >= poll_changes.saturating_sub(1) && title_events <= poll_changes + 1,
            "title notifications {title_events} for {poll_changes} page changes"
        );
        assert!(
            activity.len() as u64 >= (duration / 20).saturating_sub(2).max(1),
            "{events:?}"
        );
    }
    monitor.abort();
    let _ = monitor.await;
    m.shutdown().await.unwrap();
    drop(m);
    // Compact mode deliberately shares the browser, never the BrowserContext storage.
    let compact_dir = root.join("compact");
    fs::create_dir(&compact_dir).unwrap();
    fs::set_permissions(&compact_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let mut compact = options.clone();
    compact.data_dir = compact_dir;
    compact.compact = true;
    let cm = Manager::open(compact).await.unwrap();
    let (pc, pt) = ids(&cm.open_session("personal", request.clone()).await.unwrap());
    let (ec, et) = ids(&cm.open_session("compact", Open::default()).await.unwrap());
    assert_eq!(cm.status().await["instances"].as_array().unwrap().len(), 1);
    let (c, s) = cm.diagnostic_client("compact", &ec, &et).await.unwrap();
    c.send(
        Method::PageNavigate,
        json!({"url":"http://chat.test"}),
        Some(&s),
    )
    .await
    .unwrap();
    eval(&cm,"personal",&pc,&pt,"if(location.hostname!=='chat.test')throw Error('loading');document.cookie='private=personal';return true").await;
    assert!(
        !eval(
            &cm,
            "compact",
            &ec,
            &et,
            "if(location.hostname!=='chat.test')throw Error('loading');return document.cookie"
        )
        .await
        .as_str()
        .unwrap()
        .contains("private=personal")
    );
    cm.shutdown().await.unwrap();
    drop(cm);
    let separate_dir = root.join("per-site");
    fs::create_dir(&separate_dir).unwrap();
    fs::set_permissions(&separate_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let mut separate = options;
    separate.data_dir = separate_dir;
    separate.profiles.get_mut("personal").unwrap().isolation = "per_site".into();
    let sm = Manager::open(separate).await.unwrap();
    sm.open_session("site-a", request).await.unwrap();
    sm.open_session(
        "site-b",
        Open {
            profile: "personal".into(),
            site: Some("other".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(sm.status().await["instances"].as_array().unwrap().len(), 2);
    sm.shutdown().await.unwrap();
    proxy.abort();
}
#[test]
fn state_machine_and_profile_validation() {
    use SessionState::*;
    let mut s = Opening;
    s.transition(Ready).unwrap();
    s.transition(Busy).unwrap();
    s.transition(Ready).unwrap();
    s.transition(Suspended).unwrap();
    assert!(s.transition(Busy).is_err());
    s.transition(Opening).unwrap();
    s.transition(Ready).unwrap();
    s.transition(Recovering).unwrap();
    s.transition(Lost).unwrap();
    assert!(s.transition(Ready).is_err());
    let mut profiles = BTreeMap::new();
    profiles.insert("../escape".into(), Profile::default());
    assert!(excalibur_browser::sessions::validate_profiles(&profiles).is_err());
}

#[tokio::test]
async fn durable_operation_reconciliation_and_peer_credentials() {
    operation_contract().await;
}
async fn operation_contract() {
    let d = tempfile::tempdir().unwrap();
    fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let options = Options {
        data_dir: d.path().into(),
        binary: "/missing/chrome".into(),
        proxy: "127.0.0.1:12345".parse().unwrap(),
        profiles: BTreeMap::new(),
        compact: false,
        version: VersionPolicy::default(),
        chrome_args: vec![],
    };
    let m = Manager::open(options.clone()).await.unwrap();
    let db = rusqlite::Connection::open(d.path().join("webd.db")).unwrap();
    let params = json!({"run_id":"rpc","op_key":"interrupted","deadline_ms":30000,"session_id":"missing","tab_id":"missing"});
    let mut canonical = params.clone();
    canonical.as_object_mut().unwrap().remove("deadline_ms");
    use sha2::{Digest, Sha256};
    let hash = format!(
        "{:x}",
        Sha256::digest(json!({"method":"tab.close","params":canonical}).to_string())
    );
    db.execute(
        "INSERT INTO web_ops VALUES('interrupted','rpc','tab.close',?1,'dispatched',NULL,?2,?2)",
        rusqlite::params![hash, excalibur_browser::sessions::now()],
    )
    .unwrap();
    let received_params = json!({"run_id":"rpc","op_key":"received-first","deadline_ms":30000,"session_id":"missing","tab_id":"missing"});
    let mut canonical = received_params.clone();
    canonical.as_object_mut().unwrap().remove("deadline_ms");
    let hash = format!(
        "{:x}",
        Sha256::digest(json!({"method":"tab.close","params":canonical}).to_string())
    );
    db.execute(
        "INSERT INTO web_ops VALUES('received-first','rpc','tab.close',?1,'received',NULL,?2,?2)",
        rusqlite::params![hash, excalibur_browser::sessions::now()],
    )
    .unwrap();
    let p = Protocol::new(m.clone());
    let uncertain = call(
        &p,
        "tab.close",
        "interrupted",
        json!({"session_id":"missing","tab_id":"missing"}),
    )
    .await;
    assert_eq!(uncertain["result"]["state"], "uncertain");
    let received = call(
        &p,
        "tab.close",
        "received-first",
        json!({"session_id":"missing","tab_id":"missing"}),
    )
    .await;
    assert_eq!(received["error"]["message"], "unknown_session");
    let failure = call(
        &p,
        "tab.close",
        "before_dispatch",
        json!({"session_id":"missing","tab_id":"missing"}),
    )
    .await;
    assert_eq!(failure["error"]["message"], "unknown_session");
    let cursor =
        m.db.event("session_state", json!({"state":"ready"}))
            .await
            .unwrap();
    drop(p);
    drop(m);
    let m = Manager::open(options).await.unwrap();
    let p = Protocol::new(m.clone());
    assert_eq!(
        failure,
        call(
            &p,
            "tab.close",
            "before_dispatch",
            json!({"session_id":"missing","tab_id":"missing"})
        )
        .await
    );
    assert!(
        m.db.event("session_state", json!({"state":"closed"}))
            .await
            .unwrap()
            > cursor
    );
    // Same filesystem identity, deliberately not authorized by SO_PEERCRED. Root is always authorized.
    if unsafe { libc::geteuid() } != 0 {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let path = d.path().join("reject.sock");
        let server = Server::bind(
            &path,
            unsafe { libc::geteuid() } + 1,
            unsafe { libc::getgid() },
            p.clone(),
        )
        .unwrap();
        let task = tokio::spawn(async move { server.run(rx).await.unwrap() });
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        tx.send_replace(true);
        task.await.unwrap();
    }
    assert_eq!(
        db.pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
}

#[test]
#[ignore = "privilege dropping subprocess"]
fn reconciliation_child() {
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
        .block_on(reconciliation_contract(root));
}
async fn all_events(m: &Manager, mut cursor: u64) -> Vec<Value> {
    let mut events = vec![];
    loop {
        let page = m.db.events(cursor).await.unwrap();
        if page.is_empty() {
            return events;
        }
        cursor = page.last().unwrap()["cursor"].as_u64().unwrap();
        events.extend(page);
    }
}
async fn wait_session(
    m: &Manager,
    run: &str,
    sid: &str,
    state: &str,
    reason: Option<&str>,
) -> Value {
    let start = Instant::now();
    loop {
        let list = m.list(run).await;
        let session = list
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["session_id"] == sid)
            .unwrap();
        if session["state"] == state && reason.is_none_or(|r| session["blocked_reason"] == r) {
            return session.clone();
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "waiting for {state}/{reason:?}: {session}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn wait_count(m: &Manager, cursor: u64, count: i64) -> Vec<Value> {
    let start = Instant::now();
    loop {
        let events = all_events(m, cursor).await;
        if events
            .iter()
            .any(|e| e["type"] == "listener_activity" && e["count"] == count)
        {
            return events;
        }
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "missing unread {count}: {events:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn go(m: &Manager, run: &str, sid: &str, tid: &str, url: &str) {
    let (c, s) = m.diagnostic_client(run, sid, tid).await.unwrap();
    c.send(Method::PageNavigate, json!({"url":url}), Some(&s))
        .await
        .unwrap();
}
async fn reconciliation_contract(root: PathBuf) {
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = match proxy.local_addr().unwrap() {
        std::net::SocketAddr::V4(v) => v,
        _ => unreachable!(),
    };
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = proxy.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = [0; 8192];
                let n = stream.read(&mut request).await.unwrap();
                if String::from_utf8_lossy(&request[..n]).contains("/network-error") {
                    return;
                }
                let body = "<!doctype html><title>quiet</title><body></body>";
                let _=stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await;
            });
        }
    });
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "personal".into(),
        Profile {
            sites: vec![Site {
                name: "chat".into(),
                rw: vec!["http://chat.test".into()],
                ro: vec![],
                login: vec!["http://login.test".into()],
                listen: true,
                wait: None,
                credential_frames: vec![],
                listen_probe: ListenProbe {
                    unread_selector: ".unread".into(),
                },
            }],
            ..Default::default()
        },
    );
    let manager = Manager::open(Options {
        data_dir: root.clone(),
        binary: "/usr/bin/google-chrome-stable".into(),
        proxy: address,
        profiles,
        compact: false,
        version: VersionPolicy::default(),
        chrome_args: vec![],
    })
    .await
    .unwrap();
    let monitor = manager.start();
    let request = Open {
        profile: "personal".into(),
        site: Some("chat".into()),
        ..Default::default()
    };
    let (sid, tid) = ids(&manager.open_session("one", request.clone()).await.unwrap());
    let (shared, shared_tab) = ids(&manager.open_session("two", request).await.unwrap());
    assert_eq!(tid, shared_tab);
    eval(
        &manager,
        "one",
        &sid,
        &tid,
        "if(location.hostname!=='chat.test')throw Error('loading');return true",
    )
    .await;
    wait_count(&manager, 0, 0).await;
    tokio::time::sleep(Duration::from_millis(5200)).await;
    let cursor = all_events(&manager, 0).await.last().unwrap()["cursor"]
        .as_u64()
        .unwrap();
    eval(&manager,"one",&sid,&tid,"document.title='burst 1';const s=document.createElement('span');s.className='unread';document.body.replaceChildren(s);return true").await;
    wait_count(&manager, cursor, 1).await;
    let burst = Instant::now();
    eval(&manager,"one",&sid,&tid,"for(let n=2;n<=3;n++){document.title='burst '+n;document.body.replaceChildren(...Array.from({length:n},()=>{const s=document.createElement('span');s.className='unread';return s}));await new Promise(r=>setTimeout(r,250));}return true").await;
    assert!(burst.elapsed() < Duration::from_secs(5));
    wait_count(&manager, cursor, 3).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let events = all_events(&manager, cursor).await;
    let activity = events
        .iter()
        .filter(|e| e["type"] == "listener_activity")
        .collect::<Vec<_>>();
    assert_eq!(activity.len(), 2, "{activity:?}");
    assert_eq!(activity[0]["count"], 1);
    assert_eq!(activity[1]["count"], 3);
    assert_eq!(
        activity[1]["kinds"],
        json!(["title_changed", "unread_count"])
    );
    assert!(activity[1]["at"].as_i64().unwrap() - activity[0]["at"].as_i64().unwrap() >= 5);
    assert!(activity.iter().all(|e| e.get("content").is_none()));
    // Every owner of the shared listener follows the actual top-level document.
    go(&manager, "one", &sid, &tid, "http://login.test/signin").await;
    for (run, s) in [("one", &sid), ("two", &shared)] {
        wait_session(&manager, run, s, "blocked", Some("needs_login")).await;
        assert_eq!(
            manager.lease(run, s, &tid, false).await.unwrap_err().kind,
            "blocked"
        );
    }
    go(&manager, "one", &sid, &tid, "http://chat.test/?back=1").await;
    for (run, s) in [("one", &sid), ("two", &shared)] {
        wait_session(&manager, run, s, "ready", None).await;
        drop(manager.lease(run, s, &tid, false).await.unwrap());
    }
    go(
        &manager,
        "one",
        &sid,
        &tid,
        "http://chat.test/network-error",
    )
    .await;
    wait_session(&manager, "one", &sid, "blocked", Some("document_origin")).await;
    go(
        &manager,
        "one",
        &sid,
        &tid,
        "http://chat.test/?network_back=1",
    )
    .await;
    wait_session(&manager, "one", &sid, "ready", None).await;
    drop(manager.lease("one", &sid, &tid, false).await.unwrap());
    let (healthy, healthy_tab) = ids(&manager
        .open_session("healthy", Open::default())
        .await
        .unwrap());
    // A listener crashed on a human-only origin restores an allowed URL without leaking a page.
    go(&manager, "one", &sid, &tid, "http://login.test/signin").await;
    wait_session(&manager, "one", &sid, "blocked", Some("needs_login")).await;
    let (client, _) = manager.diagnostic_client("one", &sid, &tid).await.unwrap();
    let targets = client
        .send(Method::TargetGetTargets, json!({}), None)
        .await
        .unwrap();
    let count_pages = |v: &Value| {
        v["targetInfos"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["type"] == "page")
            .count()
    };
    let before = count_pages(&targets);
    assert_eq!(before, 1);
    let old_target = manager.tabs("one", &sid).await.unwrap()[0]["target_id"].clone();
    let status = manager.status().await;
    let pid = status["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "personal")
        .unwrap()["pid"]
        .as_u64()
        .unwrap() as u32;
    for process in excalibur_browser::process::tree(pid)
        .iter()
        .filter(|p| p.kind == "renderer")
    {
        unsafe {
            libc::kill(process.pid as i32, libc::SIGKILL);
        }
    }
    let started = Instant::now();
    loop {
        let tabs = manager.tabs("one", &sid).await.unwrap();
        if tabs[0]["target_id"] != old_target {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(20));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait_session(&manager, "one", &sid, "ready", None).await;
    wait_session(&manager, "two", &shared, "ready", None).await;
    eval(
        &manager,
        "one",
        &sid,
        &tid,
        "if(location.hostname!=='chat.test')throw Error('loading');return true",
    )
    .await;
    manager
        .switch("healthy", &healthy, &healthy_tab)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await;
    let after = count_pages(
        &client
            .send(Method::TargetGetTargets, json!({}), None)
            .await
            .unwrap(),
    );
    assert_eq!(before, after);
    assert_eq!(
        manager
            .tabs("one", &sid)
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fs::write(root.join("reconciliation.json"),serde_json::to_vec_pretty(&json!({"burst_events":activity,"final_unread_count":3,"login_return_ready":true,"network_return_ready":true,"shared_owners_recovered":true,"pages_before_crash":before,"pages_after_recovery":after,"other_instance_usable":true})).unwrap()).unwrap();
    monitor.abort();
    let _ = monitor.await;
    manager.shutdown().await.unwrap();
    server.abort();
}

#[test]
#[ignore = "privilege dropping subprocess"]
fn spin_child() {
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
        .block_on(spin_contract(root));
}
async fn spin_contract(root: PathBuf) {
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = match proxy.local_addr().unwrap() {
        std::net::SocketAddr::V4(v) => v,
        _ => unreachable!(),
    };
    let serving = tokio::spawn(async move {
        loop {
            let (mut stream, _) = proxy.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = [0; 8192];
                let n = stream.read(&mut request).await.unwrap();
                let line = String::from_utf8_lossy(&request[..n]);
                let body = if line.contains("spin.test") {
                    r#"<!doctype html><title>spin</title><body><script>setTimeout(()=>{document.body.dataset.spinning='yes';while(true){}},1500)</script>"#
                } else if line.contains("chat.test") {
                    r#"<!doctype html><title>chat</title><body><script>let n=0;setInterval(()=>{document.title='chat '+(++n);const s=document.createElement('span');s.className='unread';document.body.append(s)},1000)</script>"#
                } else {
                    "<!doctype html><title>healthy</title><body>healthy"
                };
                let _=stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await;
            });
        }
    });
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "personal".into(),
        Profile {
            sites: vec![Site {
                name: "chat".into(),
                rw: vec!["http://chat.test".into()],
                listen: true,
                wait: None,
                credential_frames: vec![],
                listen_probe: ListenProbe {
                    unread_selector: ".unread".into(),
                },
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let manager = Manager::open(Options {
        data_dir: root.clone(),
        binary: "/usr/bin/google-chrome-stable".into(),
        proxy: address,
        profiles,
        compact: false,
        version: VersionPolicy::default(),
        chrome_args: vec![],
    })
    .await
    .unwrap();
    let monitor = manager.start();
    let (listener, listener_tab) = ids(&manager
        .open_session(
            "listener",
            Open {
                profile: "personal".into(),
                site: Some("chat".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap());
    let (healthy, healthy_tab) = ids(&manager
        .open_session("healthy", Open::default())
        .await
        .unwrap());
    go(
        &manager,
        "healthy",
        &healthy,
        &healthy_tab,
        "http://healthy.test",
    )
    .await;
    eval(
        &manager,
        "healthy",
        &healthy,
        &healthy_tab,
        "if(location.hostname!=='healthy.test')throw Error('loading');return true",
    )
    .await;
    let (spin, spin_tab) = ids(&manager.open_session("spin", Open::default()).await.unwrap());
    go(&manager, "spin", &spin, &spin_tab, "http://spin.test").await;
    // This page shares the ephemeral browser with healthy, but has another context/renderer.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (client, cdp) = manager
        .diagnostic_client("spin", &spin, &spin_tab)
        .await
        .unwrap();
    let hung = client
        .send_with_timeout(
            Method::PageGetFrameTree,
            json!({}),
            Some(&cdp),
            Duration::from_millis(250),
        )
        .await
        .unwrap_err();
    assert!(matches!(hung, excalibur_cdp::Error::Timeout));
    let (protocol_stop, stop_rx) = tokio::sync::watch::channel(false);
    let socket = root.join("webd.sock");
    let server = Server::bind(
        &socket,
        unsafe { libc::geteuid() },
        unsafe { libc::getgid() },
        Protocol::new(manager.clone()),
    )
    .unwrap();
    let rpc_task = tokio::spawn(async move { server.run(stop_rx).await.unwrap() });
    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let cursor = all_events(&manager, 0).await.last().unwrap()["cursor"]
        .as_u64()
        .unwrap();
    let mut maxima = [0u128; 5];
    let mut samples = 0;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(16) {
        let measured = Instant::now();
        let list = tokio::time::timeout(Duration::from_secs(1), manager.list("healthy"))
            .await
            .expect("list blocked by another renderer");
        assert!(
            list.as_array()
                .unwrap()
                .iter()
                .any(|s| s["session_id"] == healthy)
        );
        maxima[0] = maxima[0].max(measured.elapsed().as_micros());
        let measured = Instant::now();
        let status = tokio::time::timeout(Duration::from_secs(1), manager.status())
            .await
            .expect("status blocked by another renderer");
        assert_eq!(status["instances"].as_array().unwrap().len(), 2);
        maxima[1] = maxima[1].max(measured.elapsed().as_micros());
        let measured = Instant::now();
        let lease = tokio::time::timeout(
            Duration::from_secs(1),
            manager.lease("healthy", &healthy, &healthy_tab, false),
        )
        .await
        .expect("independent tab lease blocked")
        .unwrap();
        maxima[2] = maxima[2].max(measured.elapsed().as_micros());
        drop(lease);
        let measured = Instant::now();
        tokio::time::timeout(
            Duration::from_secs(1),
            manager.switch("healthy", &healthy, &healthy_tab),
        )
        .await
        .expect("healthy switch blocked")
        .unwrap();
        maxima[3] = maxima[3].max(measured.elapsed().as_micros());
        let measured = Instant::now();
        let id = samples + 1;
        tokio::time::timeout(Duration::from_secs(1),async{let request=json!({"jsonrpc":"2.0","id":id,"method":"status.get","params":{"op_key":format!("read-{id}"),"run_id":"healthy","deadline_ms":750}});write.write_all(format!("{request}\n").as_bytes()).await.unwrap();loop{let mut line=String::new();assert!(read.read_line(&mut line).await.unwrap()>0);let value:Value=serde_json::from_str(&line).unwrap();if value["id"]==id{assert!(value["error"].is_null(),"{value}");break}}}).await.expect("Unix RPC read blocked");
        maxima[4] = maxima[4].max(measured.elapsed().as_micros());
        samples += 1;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // An explicit input check on the stuck tab also waits outside the global model.
    let pending_manager = manager.clone();
    let ps = spin.clone();
    let pt = spin_tab.clone();
    let pending = tokio::spawn(async move { pending_manager.lease("spin", &ps, &pt, true).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::time::timeout(Duration::from_secs(1), manager.status())
        .await
        .unwrap();
    let fault = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(fault.kind, "cdp_error");
    let events = all_events(&manager, cursor).await;
    let activity = events
        .iter()
        .filter(|e| e["type"] == "listener_activity" && e["session_id"] == listener)
        .collect::<Vec<_>>();
    assert!(activity.len() >= 3, "listener stalled: {activity:?}");
    for pair in activity.windows(2) {
        let gap = pair[1]["at"].as_i64().unwrap() - pair[0]["at"].as_i64().unwrap();
        assert!((5..=8).contains(&gap), "listener gap {gap}: {activity:?}");
    }
    assert!(activity.last().unwrap()["count"].as_i64().unwrap() >= 12);
    assert_eq!(
        manager.tabs("listener", &listener).await.unwrap()[0]["tab_id"],
        listener_tab
    );
    assert!(maxima.iter().all(|latency| *latency < 1_000_000));
    assert!(
        client.pending_count() <= 4,
        "timed-out probes accumulated pending CDP calls"
    );
    assert!(
        manager.status().await["instances"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["state"] != "crashed")
    );
    fs::write(root.join("contract.json"),serde_json::to_vec_pretty(&json!({"renderer_confirmed_unresponsive":true,"duration_seconds":started.elapsed().as_secs(),"samples":samples,"max_latency_microseconds":{"list":maxima[0],"status":maxima[1],"lease":maxima[2],"switch":maxima[3],"unix_rpc":maxima[4]},"listener_events":activity,"input_probe_bounded":true,"false_instance_crash":manager.status().await["instances"].as_array().unwrap().iter().any(|i|i["state"]=="crashed")})).unwrap()).unwrap();
    protocol_stop.send_replace(true);
    rpc_task.await.unwrap();
    monitor.abort();
    let _ = monitor.await;
    tokio::time::timeout(Duration::from_secs(6), manager.shutdown())
        .await
        .expect("shutdown stalled on spinning renderer")
        .unwrap();
    serving.abort();
}

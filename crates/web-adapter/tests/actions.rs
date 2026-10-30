use excalibur_browser::{
    observation::actions::ActionRequest,
    rpc::Protocol,
    sessions::{Manager, Open, Options, Profile, Site},
    version::VersionPolicy,
};
use excalibur_cdp::protocol::Method;
use excalibur_security::{ApprovalScope, Autonomy, CapabilityToken, Root, RunSecurity, Security};
use excalibur_store::{Database, DatabaseKind, StorageConfig, Store};
use excalibur_vault::{
    MasterKeyStore, SecretKind, SecretMetadata, SecretRef, SecretValue, Vault, VaultError,
};
use excalibur_web_adapter::{BrowserBackend, LocalBackend, WebAdapter};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    future::Future,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
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
fn actions_files_and_credentials() {
    run_fixture("action_child", "actions");
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
        !output
            .stdout
            .windows(PASSWORD.len())
            .chain(output.stderr.windows(PASSWORD.len()))
            .any(|w| w == PASSWORD.as_bytes()),
        "secret value appeared in test output"
    );
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
#[ignore = "privilege dropping subprocess"]
fn action_child() {
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
const PASSWORD: &str = "fixture-site-password-not-for-context-746912";
#[derive(Default)]
struct Keys(Mutex<BTreeMap<String, SecretValue>>);
impl MasterKeyStore for Keys {
    fn load(&self, id: &str) -> Result<Option<SecretValue>, VaultError> {
        Ok(self.0.lock().unwrap().get(id).cloned())
    }
    fn store(&self, id: &str, key: &SecretValue) -> Result<(), VaultError> {
        self.0.lock().unwrap().insert(id.into(), key.clone());
        Ok(())
    }
}
fn request(
    action: &str,
    snapshot: &Value,
    name: &str,
    value: Option<&str>,
    options: Value,
) -> ActionRequest {
    let reference = if name.is_empty() {
        Value::Null
    } else {
        snapshot["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == name)
            .unwrap_or_else(|| panic!("missing {name}: {}", snapshot["text"]))["ref"]
            .clone()
    };
    serde_json::from_value(json!({"action":action,"snapshot_id":snapshot["snapshot_id"],"ref":reference,"value":value,"options":options})).unwrap()
}
async fn snapshot(m: &Manager, run: &str, s: &str, t: &str) -> Value {
    match m.observe(run, s, t, "observe.snapshot", json!({})).await {
        Ok(v) => v,
        Err(e) => panic!(
            "snapshot: {e:?}\nstatus: {}\nevents: {:?}",
            m.status().await,
            m.db.events(0).await
        ),
    }
}
async fn perform(
    a: &WebAdapter,
    security: &Security,
    run: &str,
    op: &str,
    s: &str,
    t: &str,
    r: ActionRequest,
) -> excalibur_types::runtime::ToolResult {
    let now = excalibur_browser::sessions::now();
    match a.act(run, op, s, t, r.clone(), now).await {
        Ok(v) => v,
        Err(e) if e.contains("approval required") => {
            let id = e.split("approval required: ").nth(1).unwrap().trim();
            security
                .answer_approval(id, true, ApprovalScope::Once, "synthetic approval", now)
                .await
                .unwrap();
            a.act(run, op, s, t, r, now).await.unwrap()
        }
        Err(e) => panic!("action {op}: {e}"),
    }
}
/// Interposes on the trusted credential channel to reproduce a navigation race or a
/// host crash after the secret use mark and before webd receives the value.
struct Hooked {
    inner: LocalBackend,
    manager: Arc<Manager>,
    crash: bool,
    tab: (String, String, String),
}
impl BrowserBackend for Hooked {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            if method == "act.fill_secret" {
                if self.crash {
                    panic!("simulated host crash between secret use mark and fill");
                }
                let (run, s, t) = &self.tab;
                self.manager
                    .observe(run, s, t, "nav.go", json!({"url":"https://one.test/raced"}))
                    .await
                    .unwrap();
            }
            self.inner.request(method, params).await
        })
    }
}
#[allow(clippy::too_many_arguments)]
async fn fill_password(
    a: &WebAdapter,
    security: &Security,
    run: &str,
    op: &str,
    s: &str,
    t: &str,
    snap: &Value,
    secret: &str,
) -> Result<excalibur_types::runtime::ToolResult, String> {
    let reference = snap["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Password")
        .unwrap()["ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let snapshot = snap["snapshot_id"].as_str().unwrap();
    loop {
        let now = excalibur_browser::sessions::now();
        match a
            .credential(run, op, s, t, snapshot, &reference, secret, now)
            .await
        {
            Err(e) if e.contains("approval required") => {
                let id = e.split("approval required: ").nth(1).unwrap().trim();
                security
                    .answer_approval(id, true, ApprovalScope::Once, "fixture credential", now)
                    .await
                    .unwrap();
            }
            other => return other,
        }
    }
}
/// Every regular file below `path`, excluding Chrome's own profile directories.
fn files(path: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
    {
        let p = entry.path();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() && entry.file_name() != "profiles" {
            files(&p, out);
        } else if kind.is_file() {
            out.push(p);
        }
    }
}
fn run_command(root: &Path, program: &str, args: &[&str]) {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}: {}",
        program,
        String::from_utf8_lossy(&output.stderr)
    );
}
/// Installs the fixture CA in a profile's NSS database. The manager wipes the
/// ephemeral profile on open, so that profile is trusted after opening.
fn trust(root: &Path, profile: &str) {
    for location in [".pki/nssdb", ".local/share/pki/nssdb"] {
        let path = root.join("browser/profiles").join(profile).join(location);
        fs::create_dir_all(&path).unwrap();
        let db = format!("sql:{}", path.display());
        run_command(root, "certutil", &["-N", "-d", &db, "--empty-password"]);
        run_command(
            root,
            "certutil",
            &[
                "-A",
                "-d",
                &db,
                "-n",
                "fixture-ca",
                "-t",
                "C,,",
                "-i",
                "cert.pem",
            ],
        );
    }
    fn private_tree(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        for entry in fs::read_dir(path).unwrap().filter_map(Result::ok) {
            if entry.file_type().unwrap().is_dir() {
                private_tree(&entry.path());
            }
        }
    }
    private_tree(&root.join("browser"));
}
fn tls(root: &Path) -> Arc<rustls::ServerConfig> {
    run_command(
        root,
        "openssl",
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-keyout",
            "key.pem",
            "-out",
            "cert.pem",
            "-subj",
            "/CN=one.test",
            "-addext",
            "subjectAltName=DNS:one.test,DNS:two.test,DNS:sub.one.test",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
        ],
    );
    run_command(
        root,
        "openssl",
        &[
            "x509", "-in", "cert.pem", "-outform", "DER", "-out", "cert.der",
        ],
    );
    run_command(
        root,
        "openssl",
        &[
            "pkcs8", "-topk8", "-nocrypt", "-in", "key.pem", "-outform", "DER", "-out", "key.der",
        ],
    );
    trust(root, "personal");
    Arc::new(
        rustls::ServerConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(
                fs::read(root.join("cert.der")).unwrap(),
            )],
            rustls::pki_types::PrivateKeyDer::try_from(fs::read(root.join("key.der")).unwrap())
                .unwrap(),
        )
        .unwrap(),
    )
}
fn page(path: &str) -> String {
    if path == "/frame" {
        return "<!doctype html><button aria-label='Foreign'>Foreign</button><input type=password aria-label='Foreign password'>".into();
    }
    r#"<!doctype html><meta name=viewport content='width=device-width'><title>Action fixture</title><style>body{margin:20px}button,input,select{margin:5px}#list{position:absolute;top:400px;left:100px;width:1080px;height:200px;overflow:auto;border:1px solid}#drag{position:absolute;top:240px;left:20px;width:80px;height:60px;background:cyan}#drop{position:absolute;top:240px;left:250px;width:140px;height:80px;background:yellow}iframe{position:absolute;left:650px;top:200px;width:400px;height:130px}</style>
<button aria-label=Increment id=increment>Increment</button><input aria-label=Text id=text><select aria-label=Choice id=choice><option value=one>One</option><option value=two>Two</option></select><button aria-label=Hover id=hover>Hover</button><button aria-label=Moving id=moving>Moving</button><button aria-label=Confirm id=confirmButton>Confirm</button><button aria-label=Alert id=alertButton>Alert</button><button aria-label=Prompt id=promptButton>Prompt</button><button aria-label=Popup id=popup>Popup</button><button aria-label=Unknown id=unknown>?</button>
<form><button aria-label=Enviar id=submit>Enviar</button></form><input aria-label=Password id=password type=password autocomplete=current-password><input aria-label=File id=file type=file><a aria-label=Download id=download href=/pdf>Download</a><div role=button tabindex=0 aria-label=Drag draggable=true id=drag>Drag</div><div id=drop>Drop</div><div id=list tabindex=0 aria-label=List role=listbox><div style='height:2500px'>Long list</div></div><iframe src='https://two.test/frame'></iframe>
<script>const b=document.body.dataset;increment.onclick=()=>b.clicks=String(Number(b.clicks||0)+1);text.addEventListener('input',()=>b.text=text.value);choice.onchange=()=>b.choice=choice.value;hover.onmouseenter=()=>b.hover='yes';moving.onclick=()=>b.moving=String(Number(b.moving||0)+1);confirmButton.onclick=()=>{if(confirm('Continue?'))b.confirmed='yes'};alertButton.onclick=()=>{alert('fixture alert');b.alerted='yes'};promptButton.onclick=()=>b.prompt=prompt('Name?')||'';popup.onclick=()=>window.open('https://two.test/frame');unknown.onclick=()=>b.unknown='yes';document.querySelector('form').onsubmit=e=>{e.preventDefault();b.submits=String(Number(b.submits||0)+1)};text.onkeydown=e=>{if(e.ctrlKey&&e.key==='Enter'){e.preventDefault();b.shortcut='yes'}};file.onchange=async()=>{const data=await file.files[0].arrayBuffer();await fetch('/upload',{method:'POST',body:data});b.upload='yes'};drag.ondragstart=e=>e.dataTransfer.setData('text/plain','drag-fixture');drop.ondragover=e=>e.preventDefault();drop.ondrop=e=>{e.preventDefault();b.dropped=e.dataTransfer.getData('text/plain')};drag.onmousedown=()=>b.mouseDown='yes';drop.onmouseup=()=>b.mouseDrop='yes';list.style.top=(innerHeight/2-50)+'px';list.onscroll=()=>b.scrolled='yes';</script>"#.into()
}
async fn headers<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Vec<u8> {
    let mut bytes = vec![];
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut b = [0];
        if stream.read(&mut b).await.unwrap_or(0) == 0 {
            break;
        }
        bytes.push(b[0]);
        assert!(bytes.len() < 32768);
    }
    bytes
}
async fn serve<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut stream: S,
    head: Vec<u8>,
    uploaded: Arc<Mutex<Vec<u8>>>,
) {
    let text = String::from_utf8_lossy(&head);
    let path = text.split_whitespace().nth(1).unwrap_or("/");
    let path = if path.starts_with("http") {
        path.split_once("://")
            .unwrap()
            .1
            .find('/')
            .map(|i| &path[path.find("://").unwrap() + 3 + i..])
            .unwrap_or("/")
    } else {
        path
    };
    let length = text
        .lines()
        .find_map(|l| {
            l.to_lowercase()
                .strip_prefix("content-length:")
                .and_then(|s| s.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    if path == "/upload" {
        let mut data = vec![0; length];
        stream.read_exact(&mut data).await.unwrap();
        *uploaded.lock().unwrap() = data;
    }
    if path == "/big" {
        let _=stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=big.bin\r\nContent-Length: 314572800\r\nConnection: close\r\n\r\n").await;
        let chunk = vec![b'x'; 65536];
        for _ in 0..4800 {
            if stream.write_all(&chunk).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        return;
    }
    let data = if path == "/pdf" {
        b"%PDF-1.4\nlocal synthetic attachment\n%%EOF".to_vec()
    } else {
        page(path).into_bytes()
    };
    let extra = if path == "/pdf" {
        "Content-Disposition: attachment; filename=\"../../etc/passwd\"\r\n"
    } else {
        ""
    };
    let _=stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",data.len()).as_bytes()).await;
    let _ = stream.write_all(&data).await;
}
async fn contract(root: PathBuf) {
    let tls = tls(&root);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = match listener.local_addr().unwrap() {
        std::net::SocketAddr::V4(v) => v,
        _ => unreachable!(),
    };
    let uploaded = Arc::new(Mutex::new(vec![]));
    let uploads = uploaded.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let tls = tls.clone();
            let uploads = uploads.clone();
            tokio::spawn(async move {
                let head = headers(&mut s).await;
                if head.starts_with(b"CONNECT ") {
                    s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await
                        .unwrap();
                    if let Ok(mut stream) = tokio_rustls::TlsAcceptor::from(tls).accept(s).await {
                        let head = headers(&mut stream).await;
                        serve(stream, head, uploads).await;
                    }
                } else {
                    serve(s, head, uploads).await
                }
            });
        }
    });
    let workspace = root.join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(
        workspace.join("upload.txt"),
        b"exact synthetic upload content",
    )
    .unwrap();
    fs::write(workspace.join(".env"), b"protected synthetic config").unwrap();
    std::os::unix::fs::symlink("/etc/passwd", workspace.join("escape")).unwrap();
    let vault = Arc::new(
        Vault::open(
            root.join("private/vault"),
            Arc::new(Keys::default()),
            "fixture-master",
        )
        .unwrap(),
    );
    let secret = SecretRef::parse("secret://website").unwrap();
    vault
        .put(
            SecretMetadata {
                reference: secret.clone(),
                name: "Fixture website".into(),
                kind: SecretKind::WebsitePassword,
                usage: "local tests".into(),
                url: Some("https://one.test/".into()),
            },
            SecretValue::new(PASSWORD.as_bytes().to_vec()).unwrap(),
        )
        .unwrap();
    let db = Database::open(
        root.join("host/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let token = CapabilityToken {
        roots: vec![Root {
            path: workspace.clone(),
            write: false,
        }],
        budget: 10000,
        secrets: [secret.as_str().into()].into(),
        external_actions: [
            "browser.input",
            "browser.navigate_link",
            "browser.commit",
            "browser.upload",
            "browser.drag",
            "browser.download",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        data_scopes: ["browser.observe".into()].into(),
        ..Default::default()
    };
    let security = Security::new(db.clone(), token.clone(), vault.redactor())
        .await
        .unwrap();
    let journal = excalibur_journal::Journal::new(db.clone()).await.unwrap();
    let run = journal
        .create_run("browser-actions", json!({}))
        .await
        .unwrap();
    let supervised = journal
        .create_run("browser-approval", json!({}))
        .await
        .unwrap();
    for (id, autonomy) in [(&run, Autonomy::Full), (&supervised, Autonomy::Supervised)] {
        security
            .register_run(
                id,
                RunSecurity {
                    parent: None,
                    project: "browser-fixture".into(),
                    workspace: workspace.clone(),
                    token: token.clone(),
                    autonomy: vec![autonomy],
                    full_acknowledged: true,
                    user_requested: true,
                },
                excalibur_browser::sessions::now(),
            )
            .await
            .unwrap();
    }
    let profiles = BTreeMap::from([(
        "personal".into(),
        Profile {
            sites: vec![Site {
                name: "site".into(),
                rw: vec!["https://one.test".into()],
                ..Default::default()
            }],
            ..Default::default()
        },
    )]);
    let m = Manager::open_with_redactor(
        Options {
            data_dir: root.join("browser"),
            binary: "/usr/bin/google-chrome-stable".into(),
            proxy,
            profiles,
            compact: false,
            version: VersionPolicy::default(),
            chrome_args: vec![],
        },
        vault.redactor(),
    )
    .await
    .unwrap();
    trust(&root, "ephemeral");
    let monitor = m.start();
    let protocol = Protocol::new(m.clone());
    let backend = Arc::new(LocalBackend(protocol.clone()));
    let adapter = WebAdapter::new(backend, security.clone(), vault.clone(), None).unwrap();
    let (s, t) = ids(&m
        .open_session(
            &run,
            Open {
                credentials: vec![secret.as_str().into()],
                ..Default::default()
            },
        )
        .await
        .unwrap());
    let nav = m
        .observe(
            &run,
            &s,
            &t,
            "nav.go",
            json!({"url":"https://one.test/","criteria":{"timeout_ms":10000}}),
        )
        .await
        .unwrap();
    assert_eq!(nav["status"], "ready", "{nav}");
    let snap = snapshot(&m, &run, &s, &t).await;
    let r = request("click", &snap, "Increment", None, json!({}));
    let result = perform(&adapter, &security, &run, "click", &s, &t, r).await;
    assert!(result.success, "{result:?}");
    assert_eq!(
        eval(&m, &run, &s, &t, "return document.body.dataset.clicks").await,
        "1"
    );
    for (op, value, options) in [
        ("type1", "hello", json!({})),
        ("type2", "hello", json!({})),
        ("typekeys", "keys", json!({"key_events":true})),
    ] {
        let r = request("type", &snap, "Text", Some(value), options);
        let result = perform(&adapter, &security, &run, op, &s, &t, r).await;
        assert!(result.success, "{result:?}");
        assert_eq!(
            eval(
                &m,
                &run,
                &s,
                &t,
                "return document.getElementById('text').value"
            )
            .await,
            value
        );
    }
    for (action, name, value, options, check) in [
        (
            "select",
            "Choice",
            Some("two"),
            json!({}),
            "return document.body.dataset.choice==='two'",
        ),
        (
            "hover",
            "Hover",
            None,
            json!({}),
            "return document.body.dataset.hover==='yes'",
        ),
        (
            "scroll",
            "",
            None,
            json!({"direction":"down","amount":1}),
            "return document.getElementById('list').scrollTop>0",
        ),
    ] {
        let result = perform(
            &adapter,
            &security,
            &run,
            action,
            &s,
            &t,
            request(action, &snap, name, value, options),
        )
        .await;
        assert!(result.success, "{result:?}");
        assert_eq!(eval(&m, &run, &s, &t, check).await, true);
    }
    let mut r = request("press", &snap, "Text", None, json!({}));
    r.keys = Some("Control+Enter".into());
    assert!(
        perform(&adapter, &security, &run, "shortcut", &s, &t, r)
            .await
            .success
    );
    assert_eq!(
        eval(&m, &run, &s, &t, "return document.body.dataset.shortcut").await,
        "yes"
    );
    eval(&m,&run,&s,&t,"const r=increment.getBoundingClientRect();const c=document.createElement('div');c.id='cover';c.textContent='Overlay';Object.assign(c.style,{position:'fixed',left:r.x+'px',top:r.y+'px',width:r.width+'px',height:r.height+'px',zIndex:9999,background:'black'});document.body.append(c);return true").await;
    let covered = perform(
        &adapter,
        &security,
        &run,
        "covered",
        &s,
        &t,
        request("click", &snap, "Increment", None, json!({})),
    )
    .await;
    assert!(!covered.success, "{covered:?}");
    assert!(covered.output.to_string().contains("not_actionable"));
    assert_eq!(
        eval(&m, &run, &s, &t, "return document.body.dataset.clicks").await,
        "1"
    );
    eval(&m,&run,&s,&t,"document.getElementById('cover').remove();const start=performance.now();const e=document.getElementById('moving');function step(){const d=performance.now()-start;e.style.transform='translateX('+Math.min(d/2,150)+'px)';if(d<300)requestAnimationFrame(step)}step();return true").await;
    assert!(
        perform(
            &adapter,
            &security,
            &run,
            "moving",
            &s,
            &t,
            request("click", &snap, "Moving", None, json!({}))
        )
        .await
        .success
    );
    assert_eq!(
        eval(&m, &run, &s, &t, "return document.body.dataset.moving").await,
        "1"
    );
    eval(&m,&run,&s,&t,"document.getElementById('moving').style.transform='';document.getElementById('drag').draggable=false;document.getElementById('drop').setAttribute('role','button');document.getElementById('drop').setAttribute('aria-label','Drop');return true").await;
    let snap = snapshot(&m, &run, &s, &t).await;
    let mut drag = request("drag", &snap, "Drag", None, json!({}));
    drag.target_ref = Some(
        snap["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == "Drop")
            .unwrap()["ref"]
            .as_str()
            .unwrap()
            .into(),
    );
    let plain_drag = perform(&adapter, &security, &run, "drag", &s, &t, drag.clone()).await;
    assert!(plain_drag.success, "{plain_drag:?}");
    assert_eq!(
        eval(&m, &run, &s, &t, "return document.body.dataset.mouseDrop").await,
        "yes"
    );
    eval(
        &m,
        &run,
        &s,
        &t,
        "document.getElementById('drag').draggable=true;return true",
    )
    .await;
    drag.options.html5 = true;
    let html5 = perform(&adapter, &security, &run, "html5", &s, &t, drag).await;
    assert!(html5.success, "{html5:?}");
    assert_eq!(
        eval(&m, &run, &s, &t, "return document.body.dataset.dropped").await,
        "drag-fixture"
    );
    let alert = perform(
        &adapter,
        &security,
        &run,
        "alert",
        &s,
        &t,
        request("click", &snap, "Alert", None, json!({})),
    )
    .await;
    assert!(alert.success, "{alert:?}");
    assert_eq!(
        eval(&m, &run, &s, &t, "return document.body.dataset.alerted").await,
        "yes"
    );
    assert_eq!(alert.output["alerts"][0]["message"], "fixture alert");
    let confirm = perform(
        &adapter,
        &security,
        &run,
        "confirm",
        &s,
        &t,
        request("click", &snap, "Confirm", None, json!({})),
    )
    .await;
    assert_eq!(confirm.output["result"], "dialog_open", "{confirm:?}");
    assert!(
        adapter
            .act(
                &run,
                "blocked-dialog",
                &s,
                &t,
                request("click", &snap, "Increment", None, json!({})),
                excalibur_browser::sessions::now()
            )
            .await
            .unwrap_err()
            .contains("dialog_open")
    );
    let accepted = perform(
        &adapter,
        &security,
        &run,
        "accept-confirm",
        &s,
        &t,
        request("dialog", &snap, "", None, json!({"accept":true})),
    )
    .await;
    assert!(accepted.success, "{accepted:?}");
    assert_eq!(
        eval(&m, &run, &s, &t, "return document.body.dataset.confirmed").await,
        "yes"
    );
    let opened = perform(
        &adapter,
        &security,
        &run,
        "ephemeral-popup",
        &s,
        &t,
        request("click", &snap, "Popup", None, json!({})),
    )
    .await;
    assert!(opened.success, "{opened:?}");
    let new_tabs = opened.output["new_tabs"].as_array().unwrap();
    assert_eq!(new_tabs.len(), 1, "{opened:?}");
    assert_eq!(new_tabs[0]["owner_run"], json!(run));
    let popup_tab = new_tabs[0]["tab_id"].as_str().unwrap().to_owned();
    assert!(
        m.tabs(&run, &s)
            .await
            .unwrap()
            .to_string()
            .contains(&popup_tab)
    );
    m.close_tab(&run, &s, &popup_tab).await.unwrap();
    let now = excalibur_browser::sessions::now();
    for path in [Path::new(".env"), Path::new("escape"), Path::new("missing")] {
        assert!(
            adapter
                .stage_path(&run, "invalid", &s, &t, path, &[], now)
                .await
                .is_err()
        );
    }
    assert!(
        !root.join("browser/staging").join(&s).exists(),
        "rejected upload sources reached webd"
    );
    let staged = adapter
        .stage_path(&run, "stage", &s, &t, Path::new("upload.txt"), &[], now)
        .await
        .unwrap();
    let mut upload = request("upload", &snap, "File", None, json!({}));
    upload.files = vec![staged["staged_id"].as_str().unwrap().into()];
    let result = perform(&adapter, &security, &run, "upload", &s, &t, upload).await;
    assert!(result.success, "{result:?}");
    assert_eq!(*uploaded.lock().unwrap(), b"exact synthetic upload content");
    let unarmed = perform(
        &adapter,
        &security,
        &run,
        "unarmed",
        &s,
        &t,
        request("click", &snap, "Download", None, json!({})),
    )
    .await;
    assert!(unarmed.success, "{unarmed:?}");
    assert_eq!(
        unarmed.output["download"]["state"], "download_blocked",
        "{unarmed:?}"
    );
    assert!(
        !root.join("browser/quarantine").exists(),
        "unarmed download wrote to quarantine"
    );
    let download = loop {
        match adapter
            .download(
                &run,
                "download",
                &s,
                &t,
                json!({"url":"https://one.test/pdf"}),
                now,
            )
            .await
        {
            Ok(v) => break v,
            Err(e) if e.contains("approval required") => {
                security
                    .answer_approval(
                        e.split("approval required: ").nth(1).unwrap().trim(),
                        true,
                        ApprovalScope::Once,
                        "fixture download",
                        now,
                    )
                    .await
                    .unwrap();
            }
            Err(e) => panic!("download: {e}"),
        }
    };
    assert!(download.success, "{download:?}");
    assert_eq!(download.output["mime"], "application/pdf");
    let data = db
        .get_blob(
            download.output["blob"].as_str().unwrap().parse().unwrap(),
            1000,
        )
        .await
        .unwrap();
    assert_eq!(data, b"%PDF-1.4\nlocal synthetic attachment\n%%EOF");
    assert!(!download.output["file_name"].as_str().unwrap().contains('/'));
    let large = m
        .download(
            &run,
            &s,
            &t,
            &json!({"trigger":{"url":"https://one.test/big"},"timeout_ms":20000}),
        )
        .await
        .unwrap_err();
    assert_eq!(large.kind, "download_too_large");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let quarantine = root.join("browser/quarantine").join(&s);
    assert!(fs::read_dir(quarantine).unwrap().next().is_none());
    let (ps, pt) = ids(&m
        .open_session(
            &supervised,
            Open {
                profile: "personal".into(),
                site: Some("site".into()),
                credentials: vec![secret.as_str().into()],
                ..Default::default()
            },
        )
        .await
        .unwrap());
    let persistent = snapshot(&m, &supervised, &ps, &pt).await;
    let commit = request("click", &persistent, "Enviar", None, json!({}));
    let plan = m
        .prepare_action(&supervised, &ps, &pt, commit.clone())
        .await
        .unwrap();
    assert_eq!(plan["action_class"], "commit");
    assert_eq!(plan["risk"], "high");
    let approval = adapter
        .act(&supervised, "commit", &ps, &pt, commit.clone(), now)
        .await
        .unwrap_err();
    assert!(approval.contains("approval required"));
    assert_eq!(
        eval(
            &m,
            &supervised,
            &ps,
            &pt,
            "return document.body.dataset.submits||'0'"
        )
        .await,
        "0"
    );
    security
        .answer_approval(
            approval.split("approval required: ").nth(1).unwrap().trim(),
            true,
            ApprovalScope::Once,
            "fixture confirmation",
            now,
        )
        .await
        .unwrap();
    assert!(
        adapter
            .act(&supervised, "commit", &ps, &pt, commit.clone(), now)
            .await
            .unwrap()
            .success
    );
    assert!(
        adapter
            .act(&supervised, "commit", &ps, &pt, commit, now)
            .await
            .unwrap()
            .success
    );
    assert_eq!(
        eval(
            &m,
            &supervised,
            &ps,
            &pt,
            "return document.body.dataset.submits"
        )
        .await,
        "1"
    );
    let unknown = m
        .prepare_action(
            &supervised,
            &ps,
            &pt,
            request("click", &persistent, "Unknown", None, json!({})),
        )
        .await
        .unwrap();
    assert_eq!(unknown["action_class"], "commit");
    let foreign = m
        .prepare_action(
            &supervised,
            &ps,
            &pt,
            request("click", &persistent, "Foreign", None, json!({})),
        )
        .await
        .unwrap_err();
    assert_eq!(foreign.kind, "denied");
    assert_eq!(foreign.detail, "origin_not_writable");
    let popup = perform(
        &adapter,
        &security,
        &supervised,
        "popup",
        &ps,
        &pt,
        request("click", &persistent, "Popup", None, json!({})),
    )
    .await;
    assert!(popup.success, "{popup:?}");
    assert!(!popup.output["popup_blocked"].as_array().unwrap().is_empty());
    assert!(popup.output["new_tabs"].as_array().unwrap().is_empty());
    let baseline = vault.resolved_use_count();
    let foreign_ref = persistent["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "Foreign password")
        .unwrap()["ref"]
        .as_str()
        .unwrap();
    assert!(
        adapter
            .credential(
                &supervised,
                "foreign-credential",
                &ps,
                &pt,
                persistent["snapshot_id"].as_str().unwrap(),
                foreign_ref,
                secret.as_str(),
                now
            )
            .await
            .is_err()
    );
    assert_eq!(vault.resolved_use_count(), baseline);
    let passref = persistent["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "Password")
        .unwrap()["ref"]
        .as_str()
        .unwrap();
    assert!(
        m.check_credential_target(
            &supervised,
            &ps,
            &pt,
            persistent["snapshot_id"].as_str().unwrap(),
            passref,
            "secret://unbound",
            "https://one.test"
        )
        .await
        .unwrap_err()
        .detail
        .contains("secret_not_bound")
    );
    let credential = loop {
        match adapter
            .credential(
                &supervised,
                "credential",
                &ps,
                &pt,
                persistent["snapshot_id"].as_str().unwrap(),
                passref,
                secret.as_str(),
                now,
            )
            .await
        {
            Ok(v) => break v,
            Err(e) if e.contains("approval required") => {
                security
                    .answer_approval(
                        e.split("approval required: ").nth(1).unwrap().trim(),
                        true,
                        ApprovalScope::Once,
                        "fixture credential",
                        now,
                    )
                    .await
                    .unwrap();
            }
            Err(e) => panic!("credential: {e}"),
        }
    };
    assert!(credential.success, "{credential:?}");
    assert_eq!(
        eval(
            &m,
            &supervised,
            &ps,
            &pt,
            "return document.getElementById('password').value"
        )
        .await,
        PASSWORD
    );
    assert_eq!(vault.resolved_use_count(), baseline + 1);
    let redacted = snapshot(&m, &supervised, &ps, &pt).await;
    assert!(!redacted.to_string().contains(PASSWORD));
    assert_eq!(redacted["vision_suppressed"], true);
    assert!(redacted["screenshot_ref"].is_null());
    let pass = redacted["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Password")
        .unwrap();
    assert_eq!(pass["value"], "[redacted]");
    let blob = m
        .observation_blob(
            &supervised,
            redacted["dom_tree_ref"]["sha256"].as_str().unwrap(),
            0,
            1048576,
        )
        .await
        .unwrap();
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(blob["data"].as_str().unwrap())
        .unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains(PASSWORD));
    for entry in fs::read_dir(root.join("browser"))
        .unwrap()
        .filter_map(Result::ok)
    {
        if entry.file_name().to_string_lossy().starts_with("webd.db") {
            assert!(
                !fs::read(entry.path())
                    .unwrap()
                    .windows(PASSWORD.len())
                    .any(|w| w == PASSWORD.as_bytes())
            );
        }
    }
    let audit = security.audit_query(1000).await.unwrap();
    assert!(!serde_json::to_string(&audit).unwrap().contains(PASSWORD));
    // Original key remains single-use even after navigation and a changed target.
    let old = redacted["snapshot_id"].as_str().unwrap();
    let old_ref = pass["ref"].as_str().unwrap();
    m.observe(
        &supervised,
        &ps,
        &pt,
        "nav.go",
        json!({"url":"https://one.test/changed"}),
    )
    .await
    .unwrap();
    let before = vault.resolved_use_count();
    assert!(
        m.fill_secret(
            &supervised,
            &ps,
            &pt,
            old,
            old_ref,
            secret.as_str(),
            "https://one.test",
            SecretValue::new(PASSWORD.as_bytes().to_vec()).unwrap()
        )
        .await
        .is_err()
    );
    assert_eq!(vault.resolved_use_count(), before);
    let hooked = |crash| {
        WebAdapter::new(
            Arc::new(Hooked {
                inner: LocalBackend(protocol.clone()),
                manager: m.clone(),
                crash,
                tab: (supervised.clone(), ps.clone(), pt.clone()),
            }),
            security.clone(),
            vault.clone(),
            None,
        )
        .unwrap()
    };
    let password = "return document.getElementById('password').value";
    let changed = snapshot(&m, &supervised, &ps, &pt).await;
    let raced = fill_password(
        &hooked(false),
        &security,
        &supervised,
        "race-credential",
        &ps,
        &pt,
        &changed,
        secret.as_str(),
    )
    .await;
    assert!(!raced.as_ref().is_ok_and(|r| r.success), "{raced:?}");
    assert_eq!(vault.resolved_use_count(), before + 1);
    assert_eq!(
        eval(&m, &supervised, &ps, &pt, "return location.pathname").await,
        "/raced"
    );
    assert_eq!(eval(&m, &supervised, &ps, &pt, password).await, "");
    let current = snapshot(&m, &supervised, &ps, &pt).await;
    let crashed = fill_password(
        &hooked(true),
        &security,
        &supervised,
        "crash-credential",
        &ps,
        &pt,
        &current,
        secret.as_str(),
    )
    .await
    .unwrap_err();
    assert!(crashed.contains("reconciliation"), "{crashed}");
    assert_eq!(vault.resolved_use_count(), before + 2);
    let retried = fill_password(
        &adapter,
        &security,
        &supervised,
        "crash-credential",
        &ps,
        &pt,
        &current,
        secret.as_str(),
    )
    .await
    .unwrap_err();
    assert!(retried.contains("reconciliation"), "{retried}");
    assert_eq!(vault.resolved_use_count(), before + 2);
    assert_eq!(eval(&m, &supervised, &ps, &pt, password).await, "");
    let before = vault.resolved_use_count();
    assert!(
        m.new_tab(&run, &s, "https://one.test:pass@two.test/", None)
            .await
            .is_err()
    );
    let sub = m
        .new_tab(&run, &s, "https://sub.one.test/", None)
        .await
        .unwrap();
    let subid = sub["tab_id"].as_str().unwrap();
    let subshot = snapshot(&m, &run, &s, subid).await;
    let subref = subshot["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Password")
        .unwrap()["ref"]
        .as_str()
        .unwrap();
    assert!(
        adapter
            .credential(
                &run,
                "subdomain-credential",
                &s,
                subid,
                subshot["snapshot_id"].as_str().unwrap(),
                subref,
                secret.as_str(),
                now
            )
            .await
            .is_err()
    );
    assert_eq!(vault.resolved_use_count(), before);
    m.close_tab(&run, &s, subid).await.unwrap();
    let http = m.new_tab(&run, &s, "http://one.test/", None).await.unwrap();
    let hid = http["tab_id"].as_str().unwrap();
    let hs = snapshot(&m, &run, &s, hid).await;
    let hr = hs["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Password")
        .unwrap()["ref"]
        .as_str()
        .unwrap();
    assert!(
        adapter
            .credential(
                &run,
                "http-credential",
                &s,
                hid,
                hs["snapshot_id"].as_str().unwrap(),
                hr,
                secret.as_str(),
                now
            )
            .await
            .is_err()
    );
    assert_eq!(vault.resolved_use_count(), before);
    m.close_tab(&run, &s, hid).await.unwrap();
    m.end_session(&run, &s, false).await.unwrap();
    assert!(
        fs::read_dir(root.join("browser/staging").join(&s))
            .unwrap()
            .next()
            .is_none()
    );
    let mut stored = vec![];
    files(&root.join("browser"), &mut stored);
    files(&root.join("host"), &mut stored);
    assert!(
        stored
            .iter()
            .any(|p| p.starts_with(root.join("browser/observations")))
    );
    for path in &stored {
        assert!(
            !fs::read(path)
                .unwrap()
                .windows(PASSWORD.len())
                .any(|w| w == PASSWORD.as_bytes()),
            "secret value stored in {}",
            path.display()
        );
    }
    let events = json!(m.db.events(0).await.unwrap()).to_string();
    assert!(events.contains("credential_dispatch") && !events.contains(PASSWORD));
    fs::write(root.join("contract.json"),serde_json::to_vec_pretty(&json!({"actions":true,"moving":true,"overlay_rejected":true,"mouse_drag":plain_drag.output,"html5_drag":html5.output,"alert":alert.output,"confirm":confirm.output,"upload_exact":true,"download":download.output,"large_download":large.kind,"approval_before_commit":true,"commit_replayed_once":true,"unknown_commit":unknown,"popup":popup.output,"ephemeral_popup":opened.output["new_tabs"],"credential_filled":credential.output,"credential_resolutions":vault.resolved_use_count(),"secrets_absent":true,"scanned_files":stored.len(),"race_rejected":raced.map(|r|r.output).unwrap_or_else(|e|json!(e)),"crash_retry":retried,"vision_suppressed":redacted["vision_suppressed"]})).unwrap()).unwrap();
    monitor.abort();
    let _ = monitor.await;
    m.shutdown().await.unwrap();
    server.abort();
}

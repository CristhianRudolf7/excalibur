use excalibur_browser::{Class, Instance, LaunchOptions, Status, version::VersionPolicy};
use excalibur_cdp::{Client, Error, protocol::Method};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn dedicated_identity() -> (u32, u32) {
    if unsafe { libc::geteuid() } != 0 {
        return (unsafe { libc::getuid() }, unsafe { libc::getgid() });
    }
    let name = std::ffi::CString::new("excalibur-web").unwrap();
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 16384];
    assert_eq!(
        unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        },
        0
    );
    assert!(
        !result.is_null(),
        "dedicated browser user must be provisioned"
    );
    let entry = unsafe { entry.assume_init() };
    assert_ne!(entry.pw_uid, 0);
    assert_ne!(entry.pw_gid, 0);
    (entry.pw_uid, entry.pw_gid)
}
fn evidence_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-evidence/browser/cdp_contract")
}
#[test]
fn installed_chrome_contract() {
    if std::env::var("EXCALIBUR_BROWSER_TESTS").as_deref() == Ok("skip") {
        eprintln!("EXCALIBUR_BROWSER_TESTS=skip: real Chrome explicitly skipped");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    if unsafe { libc::geteuid() } == 0 {
        std::os::unix::fs::chown(
            directory.path(),
            Some(dedicated_identity().0),
            Some(dedicated_identity().1),
        )
        .unwrap();
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "contract_child", "--nocapture"])
        .env("EXCALIBUR_CDP_CONTRACT_DIR", directory.path())
        .output()
        .unwrap();
    fs::create_dir_all(evidence_dir()).unwrap();
    fs::write(
        evidence_dir().join("runner.log"),
        [output.stdout.as_slice(), output.stderr.as_slice()].concat(),
    )
    .unwrap();
    for entry in fs::read_dir(directory.path())
        .unwrap()
        .filter_map(Result::ok)
    {
        if entry.path().extension().is_some_and(|e| e == "json") {
            fs::copy(entry.path(), evidence_dir().join(entry.file_name())).unwrap();
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
#[ignore = "subprocess helper drops privileges before constructing Tokio"]
fn contract_child() {
    let root = PathBuf::from(
        std::env::var_os("EXCALIBUR_CDP_CONTRACT_DIR").expect("parent provides fixture"),
    );
    if unsafe { libc::geteuid() } == 0 {
        let (uid, gid) = dedicated_identity();
        unsafe {
            assert_eq!(libc::setgroups(0, std::ptr::null()), 0);
            assert_eq!(libc::setgid(gid), 0);
            assert_eq!(libc::setuid(uid), 0);
        }
    }
    assert_ne!(unsafe { libc::geteuid() }, 0);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(contract(&root));
}
async fn proxy() -> (std::net::SocketAddrV4, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = match listener.local_addr().unwrap() {
        std::net::SocketAddr::V4(a) => a,
        _ => unreachable!(),
    };
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0; 1024];
                while !request.windows(4).any(|c| c == b"\r\n\r\n") && request.len() < 65536 {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..n]);
                }
                let request = String::from_utf8_lossy(&request);
                let body = if request.starts_with("GET http://two.test/") {
                    "<html><body>local cross-origin frame</body></html>"
                } else {
                    "<html><head><title>Local CDP fixture</title></head><body><input id='text'><input id='file' type='file'><div id='box' style='width:100px;height:100px'>fixture</div><iframe src='http://two.test/frame'></iframe></body></html>"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (address, task)
}
async fn call(client: &Client, method: Method, params: Value, sid: Option<&str>) -> Value {
    client
        .send(method, params, sid)
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", method.as_str()))
}
async fn ready(client: &Client, sid: &str) {
    for _ in 0..200 {
        if let Ok(result)=client.send(Method::RuntimeCallFunctionOn,json!({"functionDeclaration":"function(){return !!document.getElementById('box')}","executionContextId":context(client,sid).await,"returnByValue":true}),Some(sid)).await&& result["result"]["value"]==true{return;}
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("local page never became ready");
}
async fn context(client: &Client, sid: &str) -> u64 {
    let tree = call(client, Method::PageGetFrameTree, json!({}), Some(sid)).await;
    let result = call(
        client,
        Method::PageCreateIsolatedWorld,
        json!({"frameId":tree["frameTree"]["frame"]["id"],"worldName":"excalibur-contract"}),
        Some(sid),
    )
    .await;
    result["executionContextId"].as_u64().unwrap()
}
async fn node(client: &Client, sid: &str, selector: &str) -> u64 {
    let doc = call(client, Method::DOMGetDocument, json!({}), Some(sid)).await;
    call(
        client,
        Method::DOMQuerySelector,
        json!({"nodeId":doc["root"]["nodeId"],"selector":selector}),
        Some(sid),
    )
    .await["nodeId"]
        .as_u64()
        .unwrap()
}
fn options(root: &Path, proxy: std::net::SocketAddrV4, name: &str) -> LaunchOptions {
    let profile = root.join(name);
    std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700)
        .create(&profile)
        .unwrap();
    LaunchOptions {
        binary: "/usr/bin/google-chrome-stable".into(),
        profile,
        proxy,
        class: Class::Ephemeral,
        headless: true,
        listening: false,
        secret_service: false,
        extra_args: vec![],
        version_policy: VersionPolicy::default(),
    }
}
async fn contract(root: &Path) {
    let (address, proxy_task) = proxy().await;
    let instance = Instance::launch(options(root, address, "profile"))
        .await
        .expect("real sandboxed Chrome required");
    let mut diagnostics = instance.events();
    tokio::spawn(async move {
        while let Ok(event) = diagnostics.recv().await {
            eprintln!("instance-event: {event:?}");
        }
    });
    let client = instance.client();
    let mut attachment_events = client.subscribe_all();
    let target = call(
        client,
        Method::TargetCreateTarget,
        json!({"url":"about:blank","newWindow":true}),
        None,
    )
    .await["targetId"]
        .as_str()
        .unwrap()
        .to_owned();
    let sid = instance.attach_page(&target).await.unwrap();
    call(client, Method::PageEnable, json!({}), Some(&sid)).await;
    call(
        client,
        Method::PageNavigate,
        json!({"url":"http://one.test/"}),
        Some(&sid),
    )
    .await;
    ready(client, &sid).await;
    let oopif = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = attachment_events.recv().await.unwrap();
            if event.method == "Target.attachedToTarget"
                && event.params["targetInfo"]["type"] == "iframe"
            {
                break event.params;
            }
        }
    })
    .await
    .expect("cross-origin fixture must produce an attached OOPIF");

    // Deliberately leave the general observer unread: it must report loss, not kill Chrome.
    call(client, Method::RuntimeEnable, json!({}), Some(&sid)).await;
    let flood_context = context(client, &sid).await;
    let mut slow_observer = client.subscribe_all();
    let mut instance_events = instance.events();
    let console_count = 10000;
    let flooded=client.send_with_timeout(Method::RuntimeCallFunctionOn,json!({"functionDeclaration":"function(){for(let i=0;i<10000;i++)console.log('synthetic-console-event',i);return 10000}","executionContextId":flood_context,"returnByValue":true}),Some(&sid),Duration::from_secs(60)).await.unwrap();
    assert_eq!(flooded["result"]["value"], console_count);
    assert!(
        matches!(
            slow_observer.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
        ),
        "fixture must exceed the ordinary observer queue"
    );
    let after_flood = call(client, Method::BrowserGetVersion, json!({}), None).await;
    let after_processes = instance.verify_sandbox().await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let flood_status = instance.status().borrow().clone();
    assert!(
        matches!(flood_status, Status::Running | Status::Degraded),
        "{flood_status:?}"
    );
    loop {
        match instance_events.try_recv() {
            Ok(event) => assert_ne!(event.kind, "instance_crashed"),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(error) => panic!("unexpected supervisor event loss: {error}"),
        }
    }
    // Lifecycle handling still works after the flood: create and attach another page.
    let fresh = call(
        client,
        Method::TargetCreateTarget,
        json!({"url":"about:blank"}),
        None,
    )
    .await["targetId"]
        .as_str()
        .unwrap()
        .to_owned();
    let fresh_sid = instance.attach_page(&fresh).await.unwrap();
    let fresh_context = context(client, &fresh_sid).await;
    let fresh_result=call(client,Method::RuntimeCallFunctionOn,json!({"functionDeclaration":"function(){return 42}","executionContextId":fresh_context,"returnByValue":true}),Some(&fresh_sid)).await;
    assert_eq!(fresh_result["result"]["value"], 42);
    fs::write(root.join("console-backpressure.json"),serde_json::to_vec_pretty(&json!({"console_events":console_count,"ordinary_observer_lagged":true,"status_after":flood_status,"version_after":after_flood["product"],"sandbox_after":after_processes,"new_target_after_flood":true,"false_instance_crashed":false})).unwrap()).unwrap();
    let other = call(
        client,
        Method::TargetCreateTarget,
        json!({"url":"about:blank"}),
        None,
    )
    .await["targetId"]
        .as_str()
        .unwrap()
        .to_owned();
    let ctx =
        call(client, Method::TargetCreateBrowserContext, json!({}), None).await["browserContextId"]
            .as_str()
            .unwrap()
            .to_owned();
    let mut records = Vec::new();
    for &method in Method::ALL {
        if method == Method::BrowserClose {
            continue;
        }
        let global = method.as_str().starts_with("Browser.")
            || method.as_str().starts_with("Target.")
            || method.as_str().starts_with("Tracing.")
            || method.as_str().starts_with("Memory.");
        let session = if global { None } else { Some(sid.as_str()) };
        let params = match method {
            Method::RuntimeReleaseObject => json!({"objectId":"nonexistent-object"}),
            Method::NetworkSetUserAgentOverride => json!({"userAgent":"Excalibur local contract"}),
            Method::TargetGetTargetInfo => json!({"targetId":target}),
            Method::DOMGetFrameOwner => json!({"frameId":"nonexistent-frame"}),
            Method::AccessibilityGetPartialAXTree => {
                json!({"nodeId":node(client,&sid,"#text").await})
            }
            Method::BrowserSetDownloadBehavior => json!({"behavior":"deny","eventsEnabled":true}),
            Method::BrowserCancelDownload => json!({"guid":"nonexistent-local-fixture"}),
            Method::BrowserSetPermission => {
                json!({"permission":{"name":"geolocation"},"setting":"denied","origin":"https://one.test","browserContextId":ctx})
            }
            Method::TargetDisposeBrowserContext => json!({"browserContextId":ctx}),
            Method::TargetCreateTarget => json!({"url":"about:blank"}),
            Method::TargetCloseTarget => json!({"targetId":other}),
            Method::TargetAttachToTarget => json!({"targetId":target,"flatten":true}),
            Method::TargetDetachFromTarget => {
                let result = call(
                    client,
                    Method::TargetAttachToTarget,
                    json!({"targetId":target,"flatten":true}),
                    None,
                )
                .await;
                json!({"sessionId":result["sessionId"]})
            }
            Method::TargetSetDiscoverTargets => json!({"discover":true}),
            Method::TargetSetAutoAttach => {
                json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true})
            }
            Method::PageNavigate => json!({"url":"http://one.test/again"}),
            Method::PageNavigateToHistoryEntry => {
                let history = call(
                    client,
                    Method::PageGetNavigationHistory,
                    json!({}),
                    Some(&sid),
                )
                .await;
                json!({"entryId":history["entries"].as_array().unwrap().last().unwrap()["id"]})
            }
            Method::PageCreateIsolatedWorld => {
                let tree = call(client, Method::PageGetFrameTree, json!({}), Some(&sid)).await;
                json!({"frameId":tree["frameTree"]["frame"]["id"],"worldName":"excalibur-contract"})
            }
            Method::PageAddScriptToEvaluateOnNewDocument => {
                json!({"source":"void 0","worldName":"excalibur-contract"})
            }
            Method::PageSetLifecycleEventsEnabled => json!({"enabled":true}),
            Method::PageHandleJavaScriptDialog => json!({"accept":false}),
            Method::PageSetInterceptFileChooserDialog => json!({"enabled":true}),
            Method::PageCaptureScreenshot => json!({"format":"jpeg","quality":60}),
            Method::PageStartScreencast => {
                json!({"format":"jpeg","quality":60,"maxWidth":1280,"maxHeight":900})
            }
            Method::PageScreencastFrameAck => json!({"sessionId":0}),
            Method::DOMQuerySelector => {
                let doc = call(client, Method::DOMGetDocument, json!({}), Some(&sid)).await;
                json!({"nodeId":doc["root"]["nodeId"],"selector":"#text"})
            }
            Method::DOMResolveNode
            | Method::DOMDescribeNode
            | Method::DOMGetBoxModel
            | Method::DOMGetContentQuads
            | Method::DOMScrollIntoViewIfNeeded
            | Method::DOMFocus
            | Method::DOMGetOuterHTML => json!({"nodeId":node(client,&sid,"#text").await}),
            Method::DOMGetNodeForLocation => json!({"x":10,"y":10}),
            Method::DOMSetFileInputFiles => {
                json!({"nodeId":node(client,&sid,"#file").await,"files":[]})
            }
            Method::DOMSnapshotCaptureSnapshot => {
                json!({"computedStyles":[],"includeDOMRects":true,"includePaintOrder":true})
            }
            Method::InputDispatchMouseEvent => json!({"type":"mouseMoved","x":15,"y":15}),
            Method::InputDispatchKeyEvent => json!({"type":"keyDown","key":"a","code":"KeyA"}),
            Method::InputInsertText => json!({"text":"synthetic text"}),
            Method::InputSetInterceptDrags => json!({"enabled":true}),
            Method::InputDispatchDragEvent => {
                json!({"type":"dragEnter","x":10,"y":10,"data":{"items":[],"dragOperationsMask":1}})
            }
            Method::RuntimeCallFunctionOn => {
                json!({"functionDeclaration":"function(){return document.title}","executionContextId":context(client,&sid).await,"returnByValue":true})
            }
            Method::RuntimeAddBinding => {
                json!({"name":"excalibur_contract","executionContextName":"excalibur-contract"})
            }
            Method::NetworkEnable => {
                json!({"maxTotalBufferSize":2097152,"maxResourceBufferSize":524288})
            }
            Method::FetchEnable => {
                json!({"patterns":[{"urlPattern":"*","resourceType":"Document"}]})
            }
            Method::FetchContinueRequest => json!({"requestId":"nonexistent-local-fixture"}),
            Method::FetchFailRequest => {
                json!({"requestId":"nonexistent-local-fixture","errorReason":"Aborted"})
            }
            Method::EmulationSetDeviceMetricsOverride => {
                json!({"width":360,"height":740,"deviceScaleFactor":1,"mobile":false})
            }
            Method::EmulationSetFocusEmulationEnabled => json!({"enabled":true}),
            Method::MemorySimulatePressureNotification => json!({"level":"moderate"}),
            Method::StorageGetUsageAndQuota => json!({"origin":"http://one.test"}),
            Method::StorageClearDataForOrigin => {
                json!({"origin":"http://one.test","storageTypes":"cache_storage"})
            }
            Method::TracingStart => {
                json!({"categories":"devtools.timeline","transferMode":"ReportEvents"})
            }
            _ => json!({}),
        };
        let result = client.send(method, params, session).await;
        match result {
            Ok(_) => records.push(json!({"method":method.as_str(),"status":"ok"})),
            Err(Error::Protocol{code,message}) if code != -32601 && matches!(method,Method::RuntimeReleaseObject|Method::DOMGetFrameOwner|Method::BrowserCancelDownload|Method::PageHandleJavaScriptDialog|Method::PageScreencastFrameAck|Method::FetchContinueRequest|Method::FetchFailRequest) => records.push(json!({"method":method.as_str(),"status":"expected_state_error","code":code,"message":message})),
            Err(error)=>panic!("contract {} failed: {error}",method.as_str()),
        }
        if matches!(
            method,
            Method::PageNavigate | Method::PageReload | Method::PageNavigateToHistoryEntry
        ) {
            ready(client, &sid).await;
        }
    }
    let processes = instance.processes();
    assert!(excalibur_browser::process::verify(&processes).unwrap() > 0);
    assert!(!root.join("profile/DevToolsActivePort").exists());
    let listeners = listeners(&processes);
    assert!(listeners.is_empty(), "Chrome tree listeners: {listeners:?}");
    // OOPIF attachment must be flattened and the independent session resumed.
    assert_eq!(oopif["targetInfo"]["type"], "iframe");
    // A real 70 MiB response invalidates only its CDP session.
    let isolated = call(
        client,
        Method::TargetCreateTarget,
        json!({"url":"about:blank"}),
        None,
    )
    .await["targetId"]
        .as_str()
        .unwrap()
        .to_owned();
    let huge_sid = instance.attach_page(&isolated).await.unwrap();
    let huge_context = context(client, &huge_sid).await;
    let prepared=call(client,Method::RuntimeCallFunctionOn,json!({"functionDeclaration":"function(){let html=document.documentElement;if(!html)html=document.appendChild(document.createElement('html'));let body=document.body;if(!body)body=html.appendChild(document.createElement('body'));body.style.display='none';body.textContent='x'.repeat(70*1024*1024);return body.textContent.length}","executionContextId":huge_context,"returnByValue":true}),Some(&huge_sid)).await;
    assert_eq!(prepared["result"]["value"], 70 * 1024 * 1024);
    let result = client
        .send_with_timeout(
            Method::DOMSnapshotCaptureSnapshot,
            json!({"computedStyles":[]}),
            Some(&huge_sid),
            Duration::from_secs(60),
        )
        .await;
    assert!(
        matches!(result, Err(Error::SessionCrashed(_))),
        "{result:?}"
    );
    call(client, Method::BrowserGetVersion, json!({}), None).await;
    call(client, Method::PageGetFrameTree, json!({}), Some(&sid)).await;
    let version = instance.version.clone();
    instance.close().await.unwrap();
    assert_eq!(*instance.status().borrow(), Status::Stopped);
    records.push(json!({"method":"Browser.close","status":"clean_shutdown"}));
    fs::write(root.join("contract.json"),serde_json::to_vec_pretty(&json!({"version":version,"commands":records,"processes":processes,"listeners":listeners,"oversized_session_isolated":true,"oversized_dom_snapshot":true,"oopif":true})).unwrap()).unwrap();
    let mut stale = options(root, address, "stale");
    stale.class = Class::Persistent;
    stale.version_policy.stable_major = Some(999);
    assert!(matches!(
        Instance::launch(stale.clone()).await,
        Err(excalibur_browser::Error::VersionBlocked(_))
    ));
    stale.profile = root.join("stale-ephemeral");
    fs::create_dir(&stale.profile).unwrap();
    fs::set_permissions(&stale.profile, fs::Permissions::from_mode(0o700)).unwrap();
    stale.class = Class::Ephemeral;
    assert_eq!(
        fs::read_dir(root.join("stale")).unwrap().count(),
        0,
        "blocked persistent profile must not be opened by Chrome"
    );
    let ephemeral = Instance::launch(stale).await.unwrap();
    assert_eq!(
        ephemeral.assessment().state,
        excalibur_browser::version::VersionState::BlockedPersistent
    );
    ephemeral.close().await.unwrap();
    let installed_major = version
        .product
        .split('/')
        .nth(1)
        .unwrap()
        .split('.')
        .next()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    let mut warning_options = options(root, address, "version-warning");
    warning_options.class = Class::Persistent;
    warning_options.version_policy.stable_major = Some(installed_major + 2);
    let warning_instance = Instance::launch(warning_options).await.unwrap();
    assert_eq!(
        warning_instance.events().recv().await.unwrap().kind,
        "version_warning"
    );
    warning_instance.close().await.unwrap();
    let metadata = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let metadata_address = metadata.local_addr().unwrap();
    let metadata_task = tokio::spawn(async move {
        for major in [installed_major, installed_major + 3] {
            let (mut stream, _) = metadata.accept().await.unwrap();
            let mut buffer = [0; 1024];
            let _ = stream.read(&mut buffer).await.unwrap();
            let body =
                json!({"channels":{"Stable":{"version":format!("{major}.0.0.0")}}}).to_string();
            stream
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
    let mut refresh_options = options(root, address, "version-refresh");
    refresh_options.class = Class::Persistent;
    refresh_options.version_policy.stable_metadata_url =
        Some(format!("http://{metadata_address}/stable.json"));
    let refresh_instance = Instance::launch(refresh_options).await.unwrap();
    assert_eq!(
        refresh_instance.assessment().state,
        excalibur_browser::version::VersionState::Current
    );
    assert_eq!(
        refresh_instance.refresh_version().await.unwrap().state,
        excalibur_browser::version::VersionState::BlockedPersistent
    );
    assert_eq!(*refresh_instance.status().borrow(), Status::Blocked);
    metadata_task.await.unwrap();
    refresh_instance.close().await.unwrap();
    let mut bad = options(root, address, "no-sandbox-helper");
    let copied = root.join("unprofiled-chrome");
    fs::copy("/opt/google/chrome/chrome", &copied).unwrap();
    fs::set_permissions(&copied, fs::Permissions::from_mode(0o755)).unwrap();
    bad.binary = copied;
    let bad_error = match Instance::launch(bad).await {
        Ok(_) => panic!("unprofiled binary without sandbox helper must fail closed"),
        Err(e) => e,
    };
    assert!(
        matches!(bad_error, excalibur_browser::Error::Sandbox(_)),
        "{bad_error}"
    );
    let victim = Instance::launch(options(root, address, "crash-victim"))
        .await
        .unwrap();
    let target = call(
        victim.client(),
        Method::TargetCreateTarget,
        json!({"url":"about:blank"}),
        None,
    )
    .await["targetId"]
        .as_str()
        .unwrap()
        .to_owned();
    let session = victim.attach_page(&target).await.unwrap();
    call(
        victim.client(),
        Method::PageEnable,
        json!({}),
        Some(&session),
    )
    .await;
    let mut renderer_events = victim.events();
    for process in
        excalibur_browser::process::tree_for_profile(victim.pid(), &root.join("crash-victim"))
            .iter()
            .filter(|p| p.kind == "renderer")
    {
        unsafe {
            libc::kill(process.pid as i32, libc::SIGKILL);
        }
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if renderer_events.recv().await.unwrap().kind == "target_crashed" {
                break;
            }
        }
    })
    .await
    .expect("renderer crash must be reported");
    call(victim.client(), Method::BrowserGetVersion, json!({}), None).await;
    let mut status = victim.status();
    let mut events = victim.events();
    assert_eq!(unsafe { libc::kill(victim.pid() as i32, libc::SIGKILL) }, 0);
    tokio::time::timeout(Duration::from_secs(5), async {
        while *status.borrow() != Status::Crashed {
            status.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "status after SIGKILL: {:?}; event: {:?}",
            *status.borrow(),
            events.try_recv()
        )
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if events.recv().await.unwrap().kind == "instance_crashed" {
                break;
            }
        }
    })
    .await
    .unwrap();
    victim.close().await.unwrap();
    fs::write(root.join("boundary.json"),serde_json::to_vec_pretty(&json!({"persistent_outdated_blocked":true,"ephemeral_outdated_available":true,"unprofiled_binary_rejected":true,"browser_crash_detected":true,"renderer_crash_detected":true,"version_warning_emitted":true,"version_refresh_blocks_persistent":true})).unwrap()).unwrap();
    proxy_task.abort();
}
fn listeners(processes: &[excalibur_browser::process::Process]) -> Vec<String> {
    let mut inodes = std::collections::HashSet::new();
    for process in processes {
        let entries = match fs::read_dir(format!("/proc/{}/fd", process.pid)) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("cannot inspect owned process descriptors: {e}"),
        };
        for entry in entries.filter_map(Result::ok) {
            if let Ok(link) = fs::read_link(entry.path()) {
                inodes.insert(link.to_string_lossy().into_owned());
            }
        }
    }
    let mut result = Vec::new();
    for process in processes {
        for table in ["tcp", "tcp6"] {
            let text = match fs::read_to_string(format!("/proc/{}/net/{table}", process.pid)) {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => panic!("cannot inspect owned process network namespace: {e}"),
            };
            for line in text.lines().skip(1) {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.get(3) == Some(&"0A")
                    && fields
                        .get(9)
                        .is_some_and(|inode| inodes.contains(&format!("socket:[{inode}]")))
                {
                    result.push(line.into());
                }
            }
        }
    }
    result
}

#[test]
fn killing_parent_removes_both_browser_trees() {
    if std::env::var("EXCALIBUR_BROWSER_TESTS").as_deref() == Ok("skip") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    if unsafe { libc::geteuid() } == 0 {
        std::os::unix::fs::chown(
            dir.path(),
            Some(dedicated_identity().0),
            Some(dedicated_identity().1),
        )
        .unwrap();
    }
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "parent_death_child", "--nocapture"])
        .env("EXCALIBUR_CDP_CONTRACT_DIR", dir.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let file = dir.path().join("parent-death.json");
    for _ in 0..400 {
        if file.exists() {
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("parent-death helper exited: {status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !file.exists() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("two Chrome instances did not become ready");
    }
    let evidence: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let live = || {
        evidence
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| {
                let pid = p["pid"].as_u64().unwrap() as u32;
                excalibur_browser::process::tree(pid)
                    .first()
                    .filter(|now| now.start_ticks == p["start_ticks"].as_u64().unwrap())
                    .map(|_| pid)
            })
            .collect::<Vec<_>>()
    };
    for _ in 0..200 {
        if live().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let remaining = live();
    // Cleanup remains restricted to the recorded identities if the assertion fails.
    for pid in &remaining {
        unsafe {
            libc::kill(*pid as i32, libc::SIGKILL);
        }
    }
    fs::create_dir_all(evidence_dir()).unwrap();
    fs::write(
        evidence_dir().join("parent-death.json"),
        serde_json::to_vec_pretty(&json!({"processes":evidence,"remaining_live":remaining}))
            .unwrap(),
    )
    .unwrap();
    assert!(
        remaining.is_empty(),
        "processes survived parent death: {remaining:?}"
    );
}
#[test]
#[ignore = "subprocess helper for real parent death with two instances"]
fn parent_death_child() {
    let root = PathBuf::from(std::env::var_os("EXCALIBUR_CDP_CONTRACT_DIR").unwrap());
    if unsafe { libc::geteuid() } == 0 {
        let (uid, gid) = dedicated_identity();
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
        .block_on(async {
            let (proxy, _task) = proxy().await;
            let first = Instance::launch(options(&root, proxy, "first"))
                .await
                .unwrap();
            let second = Instance::launch(options(&root, proxy, "second"))
                .await
                .unwrap();
            let mut pids = first.processes();
            pids.extend(second.processes());
            let temporary = root.join("pids.tmp");
            fs::write(&temporary, serde_json::to_vec(&pids).unwrap()).unwrap();
            fs::rename(temporary, root.join("parent-death.json")).unwrap();
            std::future::pending::<()>().await;
            drop((first, second));
        });
}

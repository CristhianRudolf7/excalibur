use excalibur_webd::{Config, drop_privileges, preflight};
fn run() -> Result<bool, String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2
        || (args[0] != "--preflight" && args[0] != "--check-instance" && args[0] != "--serve")
    {
        return Err(
            "usage: excalibur-webd <--preflight|--check-instance|--serve> <config.toml>".into(),
        );
    }
    let config = Config::parse(&std::fs::read_to_string(&args[1]).map_err(|e| e.to_string())?)?;
    let identity = drop_privileges(&config.web.os_user)?;
    let report = preflight(&config.web, identity);
    if args[0] == "--preflight" || !report.ready {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
        return Ok(report.ready);
    }
    let binary = report.chrome.ok_or("Chrome unavailable")?.binary;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    if args[0] == "--serve" {
        return runtime.block_on(serve(config, binary));
    }
    runtime.block_on(async {
        // Diagnostic loopback: UID-checked, deny-all. Real serving forwards
        // to the HostProxy broker (12.3); this check never browses externally.
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        let address = match proxy.local_addr().map_err(|e| e.to_string())? { std::net::SocketAddr::V4(v) => v, _ => return Err("IPv4 loopback required".into()) };
        let expected = unsafe { libc::geteuid() };
        let deny = tokio::spawn(async move {
            loop {
                match excalibur_webd::loopback::accept_owned(&proxy, expected).await {
                    Ok(Some(_)) => {},
                    Ok(None) => {},
                    Err(_) => break,
                }
            }
        });
        use std::os::unix::fs::DirBuilderExt;
        let profile = config.web.data_dir.join(format!("instance-check-{}-{}", std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()));
        std::fs::DirBuilder::new().mode(0o700).create(&profile).map_err(|e| e.to_string())?;
        struct Profile(std::path::PathBuf);
        impl Drop for Profile { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
        let profile = Profile(profile);
        let result = excalibur_browser::Instance::launch(excalibur_browser::LaunchOptions {
            binary,profile:profile.0.clone(),proxy:address,class:config.web.instance_class,headless:true,listening:false,secret_service:false,extra_args:config.web.chrome_args.clone(),version_policy:config.web.version.clone(),
        }).await;
        let result = match result {
            Ok(instance) => {
                let processes=instance.verify_sandbox().await.map_err(|e|e.to_string())?;
                let evidence = serde_json::json!({"ready":true,"version":instance.version,"warnings":instance.warnings,"assessment":instance.assessment(),"processes":processes,"sandbox_verified":true,"network":"uid_checked_loopback_deny_all"});
                instance.close().await.map_err(|e|e.to_string())?;
                println!("{}",serde_json::to_string_pretty(&evidence).map_err(|e| e.to_string())?); Ok(true)
            },
            Err(error) => Err(error.to_string()),
        };
        deny.abort(); result
    })
}
fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            println!("{}", serde_json::json!({"ready": false, "error": error}));
            std::process::exit(1);
        }
    }
}

async fn serve(config: Config, binary: std::path::PathBuf) -> Result<bool, String> {
    use excalibur_browser::{
        rpc::{Protocol, Server},
        sessions::{Manager, Options},
    };
    // Egress broker (12.3): HostProxy with pinned DNS, no private/loopback
    // answers and only web ports. The loopback listener below verifies the
    // peer UID so only the web user can reach the broker through Chrome's
    // proxy settings.
    let broker =
        excalibur_sandbox::network::HostProxy::start(excalibur_sandbox::network::ProxyConfig::new(
            excalibur_sandbox::network::NetworkPolicy::Full,
        ))
        .map_err(|e| e.to_string())?;
    let broker_path = broker.socket_path().to_owned();
    // Keep the broker alive for the whole service lifetime.
    let _broker = broker;
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let address = match proxy.local_addr().map_err(|e| e.to_string())? {
        std::net::SocketAddr::V4(v) => v,
        _ => return Err("IPv4 loopback required".into()),
    };
    let expected = unsafe { libc::geteuid() };
    let forward = tokio::spawn(async move {
        loop {
            let accepted = excalibur_webd::loopback::accept_owned(&proxy, expected).await;
            match accepted {
                Ok(Some(stream)) => {
                    let path = broker_path.clone();
                    tokio::spawn(async move {
                        let _ = excalibur_webd::loopback::forward_to_unix(stream, &path).await;
                    });
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
    });
    let manager = Manager::open(Options {
        data_dir: config.web.data_dir.clone(),
        binary,
        proxy: address,
        profiles: config.web.profiles,
        compact: config.web.compact,
        version: config.web.version,
        chrome_args: config.web.chrome_args,
    })
    .await
    .map_err(|e| e.to_string())?;
    let protocol = Protocol::new(manager.clone());
    let server = Server::bind(
        &config.web.run_dir.join("webd.sock"),
        config.web.host_uid,
        config.web.socket_gid.unwrap_or(unsafe { libc::getgid() }),
        protocol.clone(),
    )
    .map_err(|e| e.to_string())?;
    let monitor = manager.start();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let signals = tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {_=term.recv()=>{},_=tokio::signal::ctrl_c()=>{}}
        tx.send_replace(true);
    });
    println!(
        "{}",
        serde_json::json!({"ready":true,"socket":config.web.run_dir.join("webd.sock"),"network":"host_proxy_full_80_443_uid_checked_loopback"})
    );
    let result = server.run(rx).await;
    signals.abort();
    monitor.abort();
    let _ = monitor.await;
    protocol.drain().await;
    let closed = manager.shutdown().await;
    forward.abort();
    result.and(closed).map_err(|e| e.to_string())?;
    Ok(true)
}

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
        // Until the network broker is integrated, this diagnostic owns a deny-all proxy.
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        let address = match proxy.local_addr().map_err(|e| e.to_string())? { std::net::SocketAddr::V4(v) => v, _ => return Err("IPv4 loopback required".into()) };
        let deny = tokio::spawn(async move { while let Ok((stream, _)) = proxy.accept().await { drop(stream); } });
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
                let evidence = serde_json::json!({"ready":true,"version":instance.version,"warnings":instance.warnings,"assessment":instance.assessment(),"processes":processes,"sandbox_verified":true,"network":"deny_all_diagnostic_proxy"});
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
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let address = match proxy.local_addr().map_err(|e| e.to_string())? {
        std::net::SocketAddr::V4(v) => v,
        _ => return Err("IPv4 loopback required".into()),
    };
    let deny = tokio::spawn(async move {
        while let Ok((stream, _)) = proxy.accept().await {
            drop(stream)
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
        serde_json::json!({"ready":true,"socket":config.web.run_dir.join("webd.sock"),"network":"deny_all_until_broker_integration"})
    );
    let result = server.run(rx).await;
    signals.abort();
    monitor.abort();
    let _ = monitor.await;
    protocol.drain().await;
    let closed = manager.shutdown().await;
    deny.abort();
    result.and(closed).map_err(|e| e.to_string())?;
    Ok(true)
}

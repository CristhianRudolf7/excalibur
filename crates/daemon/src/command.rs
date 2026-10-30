//! Thin synchronous process adapter; shared services own all domain behavior.
use crate::{DaemonHost, HostOptions, RuntimeSet};
use excalibur_models::{GatewayConfig, ModelGateway, ModelRole, OpenAiGateway};
use excalibur_providers::{Provider, ProviderClient, Secret};
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::{BoxFuture, ModelRequest, ModelResponse};
use excalibur_vault::{Redactor, SecretValue};
use serde_json::json;
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
struct Offline;
impl ModelGateway for Offline {
    fn complete<'a>(
        &'a self,
        _: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, excalibur_models::ModelError>> {
        Box::pin(async {
            Err(excalibur_models::ModelError::Configuration(
                "no model configured".into(),
            ))
        })
    }
}
struct Options {
    action: String,
    root: PathBuf,
    data: PathBuf,
    listen: SocketAddr,
    model: Option<String>,
    endpoint: Option<String>,
}
fn options(args: &[String]) -> Result<Options, String> {
    let action = args
        .first()
        .filter(|s| s.as_str() == "start" || s.as_str() == "stop")
        .ok_or("daemon requires start or stop")?
        .clone();
    let mut root = None;
    let mut data = None;
    let mut listen = "127.0.0.1:3791".parse().unwrap();
    let mut model = None;
    let mut endpoint = None;
    let mut args = args[1..].iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("daemon option requires a value")?;
        match flag.as_str() {
            "--project" => root = Some(PathBuf::from(value)),
            "--data-dir" => data = Some(PathBuf::from(value)),
            "--listen" => {
                listen = value
                    .parse::<SocketAddr>()
                    .map_err(|_| "invalid listen address")?
            }
            "--model" => model = Some(value.clone()),
            "--base-url" => endpoint = Some(value.clone()),
            _ => return Err("unknown daemon option".into()),
        }
    }
    if !listen.ip().is_loopback() {
        return Err("standalone transport requires loopback; use an authenticated TLS reverse proxy for remote access".into());
    }
    let cwd = std::env::current_dir().map_err(|_| "current directory unavailable")?;
    let root = root
        .unwrap_or(cwd.clone())
        .canonicalize()
        .map_err(|_| "project root unavailable")?;
    let data = data.ok_or("explicit --data-dir is required")?;
    let data = if data.is_absolute() {
        data
    } else {
        cwd.join(data)
    };
    Ok(Options {
        action,
        root,
        data,
        listen,
        model,
        endpoint,
    })
}
pub fn run(args: &[String]) -> i32 {
    match execute(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{}", Redactor::default().redact_text(&e));
            1
        }
    }
}
fn execute(args: &[String]) -> Result<(), String> {
    let options = options(args)?;
    let token = std::env::var("EXCALIBUR_DAEMON_TOKEN")
        .map_err(|_| "EXCALIBUR_DAEMON_TOKEN must be configured")?;
    let redactor = Redactor::default();
    redactor.register_secret(&SecretValue::new(token.clone()).map_err(|_| "invalid daemon token")?);
    let workers = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(2)
        .saturating_sub(2)
        .max(1);
    let runtimes =
        RuntimeSet::new(workers, workers.min(4)).map_err(|_| "daemon runtimes unavailable")?;
    if options.action == "stop" {
        let result = runtimes.block_on(async {
            let response = reqwest::Client::new()
                .post(format!("http://{}/rpc", options.listen))
                .bearer_auth(&token)
                .json(&json!({"jsonrpc":"2.0","id":1,"method":"daemon.stop","params":{}}))
                .send()
                .await
                .map_err(|_| "daemon not reachable")?;
            if !response.status().is_success() {
                return Err("daemon denied shutdown".to_string());
            }
            let result: serde_json::Value = response
                .json()
                .await
                .map_err(|_| "invalid daemon response")?;
            if result.get("error").is_some() {
                return Err("daemon rejected shutdown".into());
            }
            Ok(())
        });
        runtimes.shutdown(Duration::from_secs(5));
        return result;
    }
    std::fs::create_dir_all(&options.data).map_err(|_| "data directory unavailable")?;
    let db = Database::open(
        options.data.join("excalibur.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .map_err(|e| redactor.redact_text(&e.to_string()))?;
    let model: Arc<dyn ModelGateway> = if let Some(model) =
        options.model.or_else(|| std::env::var("OPENAI_MODEL").ok())
    {
        let key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| "OPENAI_API_KEY required for configured model")?;
        redactor
            .register_secret(&SecretValue::new(key.clone()).map_err(|_| "invalid provider key")?);
        let key = Secret::new(key).map_err(|_| "invalid provider key")?;
        let provider = if let Some(endpoint) = options.endpoint {
            ProviderClient::with_base_url(Provider::OpenAi, key, &endpoint)
        } else {
            ProviderClient::new(Provider::OpenAi, key)
        }
        .map_err(|_| "provider configuration invalid")?;
        Arc::new(
            OpenAiGateway::new(provider, GatewayConfig::new(model, ModelRole::Worker))
                .map_err(|_| "model configuration invalid")?,
        )
    } else {
        Arc::new(Offline)
    };
    let work = runtimes.work();
    let mut hosting = HostOptions::project(options.root);
    hosting.context_runtime = Some(runtimes.cpu_handle());
    let host = runtimes.block_on(DaemonHost::open(
        db,
        hosting,
        model,
        None,
        &token,
        redactor.clone(),
        work.clone(),
    ))?;
    let listener = runtimes
        .block_on(tokio::net::TcpListener::bind(options.listen))
        .map_err(|_| "daemon listen failed")?;
    eprintln!(
        "Excalibur daemon listening on {}",
        listener
            .local_addr()
            .map_err(|_| "listener address unavailable")?
    );
    let orchestrator = host.orchestrator.clone();
    let master = host.master.clone();
    let (stop, mut stopping) = tokio::sync::watch::channel(false);
    let failure = host.shutdown.clone();
    let background = work.spawn(async move {
        let result = async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {_=stopping.changed()=>break,_=interval.tick()=>{}}
                if let Err(e) = orchestrator.tick().await {
                    return Err(e.to_string());
                }
                let sample = tokio::task::spawn_blocking(crate::sample_memory)
                    .await
                    .map_err(|_| "memory sampler panicked")?;
                if let Ok(sample) = sample {
                    orchestrator
                        .resource_sample(sample.total_rss_bytes, excalibur_store::unix_seconds())
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
            Ok::<(), String>(())
        }
        .await;
        if result.is_err() {
            failure.notify_one();
        }
        result
    });
    let housekeeping_lanes = host.lanes.clone();
    let housekeeping_actions = host.control_actions();
    let housekeeping_master = host.master.clone();
    let housekeeping_orchestrator = host.orchestrator.clone();
    let (housekeeping_stop, mut housekeeping_stopping) = tokio::sync::watch::channel(false);
    let failure = host.shutdown.clone();
    let housekeeping=runtimes.spawn_interactive(async move{
        let result=async move {
        let mut interval=tokio::time::interval(Duration::from_secs(1));
        loop {tokio::select!{_=housekeeping_stopping.changed()=>break,_=interval.tick()=>{}}
            housekeeping_actions.recover_task_directives().await?;
            let mut entries=Vec::new();
            for agent in housekeeping_orchestrator.active_agents().await.map_err(|e|e.to_string())? {
                let status=housekeeping_orchestrator.status(&agent.id).await.map_err(|e|e.to_string())?;
                if agent.depth>0 && !matches!(status.state.as_str(),"completed"|"completed_with_pending"|"cancelled"|"failed") {entries.push(excalibur_master::LexiconEntry{alias:agent.name,id:agent.id,kind:"agent".into()});}
            }
            for project in housekeeping_orchestrator.projects().await.map_err(|e|e.to_string())? {
                for alias in std::iter::once(project.name).chain(project.aliases){entries.push(excalibur_master::LexiconEntry{alias,id:project.id.clone(),kind:"project".into()});}
            }
            housekeeping_master.set_lexicon(entries).map_err(|e|e.to_string())?;
            let now=excalibur_store::unix_seconds();
            let last_user_message=housekeeping_master.database().read(excalibur_store::ReadLane::Interactive,|c|Ok(c.query_row("SELECT MAX(sent_at) FROM messages WHERE role='user'",[],|r|r.get::<_,Option<i64>>(0))?)).await.map_err(|e|e.to_string())?;
            housekeeping_lanes.set_user_active(last_user_message.is_some_and(|at|now.saturating_sub(at)<600));
            housekeeping_master.tick_questions(now).await.map_err(|e|e.to_string())?;
            housekeeping_master.retry_pending().await.map_err(|e|e.to_string())?;
            for delivery in housekeeping_orchestrator.pending_deliveries(now).await.map_err(|e|e.to_string())? {
                if delivery.channel.starts_with("conversation:") || delivery.channel=="conversation" {
                    let target=delivery.cross_conversation.as_ref().unwrap_or(&delivery.notification.conversation);
                    let agent=housekeeping_orchestrator.agent(&delivery.notification.agent_id).await.map_err(|e|e.to_string())?;
                    let content=if delivery.cross_conversation.is_some(){json!({"notification_id":delivery.notification.id,"origin":delivery.notification.conversation,"scope":agent.scope,"summary":"Urgent update in the origin conversation"})}else{json!({"notification_id":delivery.notification.id,"content":delivery.notification.content,"origin":delivery.notification.conversation,"scope":agent.scope})};
                    housekeeping_master.publish_notice(&format!("{}:{}",delivery.notification.id,delivery.channel),target,content,&delivery.notification.class).await.map_err(|e|e.to_string())?;
                    housekeeping_orchestrator.confirm_delivery(&delivery.notification.id,&delivery.channel).await.map_err(|e|e.to_string())?;
                }
            }
        }
        Ok::<(),String>(())
        }.await;
        if result.is_err(){failure.notify_one();}
        result
    });
    let result = runtimes.block_on(async {
        let shutdown = host.shutdown.clone();
        let connections = host.gateway.clone();
        axum::serve(listener, host.gateway.clone().router())
            .with_graceful_shutdown(async move {
                tokio::select! {_=shutdown_signal()=>{},_=shutdown.notified()=>{}}
                connections.close_connections();
            })
            .await
            .map_err(|_| "gateway server failed".to_string())
    });
    let _ = housekeeping_stop.send(true);
    let housekeeping_result = runtimes
        .block_on(housekeeping)
        .map_err(|_| "interactive housekeeping panicked")?;
    let _ = stop.send(true);
    let background_result = runtimes
        .block_on(background)
        .map_err(|_| "orchestration task panicked")?;
    runtimes.block_on(master.shutdown());
    let close = runtimes.block_on(host.close());
    runtimes.shutdown(Duration::from_secs(10));
    result?;
    housekeeping_result?;
    background_result?;
    close
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=signal.recv()=>{},_=tokio::signal::ctrl_c()=>{}}
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

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
    cgroup_root: Option<PathBuf>,
    fallback_model: Option<String>,
    fallback_endpoint: Option<String>,
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
    let mut cgroup_root = None;
    let mut fallback_model = None;
    let mut fallback_endpoint = None;
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
            "--fallback-model" => fallback_model = Some(value.clone()),
            "--fallback-base-url" => fallback_endpoint = Some(value.clone()),
            "--cgroup-root" => cgroup_root = Some(PathBuf::from(value)),
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
        cgroup_root,
        fallback_model,
        fallback_endpoint,
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
    let model: Arc<dyn ModelGateway> =
        if let Some(model) = options.model.or_else(|| std::env::var("OPENAI_MODEL").ok()) {
            configured_model(
                model,
                options.endpoint,
                "OPENAI_API_KEY",
                ModelRole::Worker,
                &redactor,
            )?
        } else {
            Arc::new(Offline)
        };
    let fallback = options
        .fallback_model
        .or_else(|| std::env::var("OPENAI_FALLBACK_MODEL").ok())
        .map(|model| {
            configured_model(
                model,
                options.fallback_endpoint,
                "OPENAI_FALLBACK_API_KEY",
                ModelRole::Master,
                &redactor,
            )
        })
        .transpose()?;
    let work = runtimes.work();
    let mut hosting = HostOptions::project(options.root);
    hosting.memory = Some(crate::MemoryOptions::default());
    hosting.context_runtime = Some(runtimes.cpu_handle());
    hosting.cgroup_root = options.cgroup_root;
    let host = runtimes.block_on(DaemonHost::open(
        db,
        hosting,
        model,
        fallback,
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
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let background_orchestrator = host.orchestrator.clone();
    let background = crate::services::supervise(
        &work,
        "orchestration",
        host.shutdown.clone(),
        stopping.clone(),
        move || {
            let orchestrator = background_orchestrator.clone();
            async move {
                orchestrator.tick().await.map_err(|e| e.to_string())?;
                let sample = tokio::task::spawn_blocking(crate::sample_memory)
                    .await
                    .map_err(|_| "memory sampler task failed")?;
                if let Ok(sample) = sample {
                    orchestrator
                        .resource_sample(sample.total_rss_bytes, excalibur_store::unix_seconds())
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            }
        },
    );
    let housekeeping_lanes = host.lanes.clone();
    let housekeeping_actions = host.control_actions();
    let housekeeping_master = host.master.clone();
    let housekeeping_orchestrator = host.orchestrator.clone();
    let housekeeping = crate::services::supervise(
        &runtimes.interactive(),
        "housekeeping",
        host.shutdown.clone(),
        stopping.clone(),
        move || {
            let (lanes, actions, master, orchestrator) = (
                housekeeping_lanes.clone(),
                housekeeping_actions.clone(),
                housekeeping_master.clone(),
                housekeeping_orchestrator.clone(),
            );
            async move { housekeeping_iteration(lanes, actions, master, orchestrator).await }
        },
    );
    // Automatic Agenda: startup missed-fire recomputation, then a wake at the
    // earliest durable reminder timer. Admission reuses the guarded host path.
    let agenda = crate::agenda::AgendaRuntime::new(
        host.orchestrator.clone(),
        Arc::new(crate::agenda::SystemClock),
    )
    .spawn(&work, host.shutdown.clone(), stopping.clone());
    let writer = crate::services::supervise_writer(
        &runtimes.interactive(),
        host.orchestrator.database().clone(),
        host.shutdown.clone(),
        stopping,
    );
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
    let _ = stop.send(true);
    let housekeeping_result = runtimes
        .block_on(housekeeping)
        .unwrap_or_else(|_| Err("housekeeping task failed".into()));
    let background_result = runtimes
        .block_on(background)
        .unwrap_or_else(|_| Err("orchestration task failed".into()));
    let agenda_result = runtimes
        .block_on(agenda)
        .unwrap_or_else(|_| Err("agenda task failed".into()));
    let writer_result = runtimes
        .block_on(writer)
        .unwrap_or_else(|_| Err("writer watcher task failed".into()));
    let orderly = result.is_ok()
        && housekeeping_result.is_ok()
        && background_result.is_ok()
        && agenda_result.is_ok()
        && writer_result.is_ok();
    let close = runtimes.block_on(host.close_with_outcome(orderly));
    runtimes.shutdown(Duration::from_secs(10));
    result
        .and(housekeeping_result)
        .and(background_result)
        .and(agenda_result)
        .and(writer_result)
        .and(close)
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

async fn housekeeping_iteration(
    lanes: crate::LaneScheduler,
    actions: Arc<crate::DaemonActions>,
    master: excalibur_master::Master,
    orchestrator: excalibur_orchestrator::Orchestrator,
) -> Result<(), String> {
    orchestrator
        .recovery()
        .heartbeat(excalibur_store::unix_seconds())
        .await
        .map_err(|e| e.to_string())?;
    actions.recover_task_directives().await?;
    let mut entries = Vec::new();
    for agent in orchestrator
        .active_agents()
        .await
        .map_err(|e| e.to_string())?
    {
        if agent.depth > 0 && !agent.name.is_empty() && agent.name.len() <= 256 {
            entries.push(excalibur_master::LexiconEntry {
                alias: agent.name,
                id: agent.id,
                kind: "agent".into(),
            });
        }
    }
    for project in orchestrator.projects().await.map_err(|e| e.to_string())? {
        for alias in std::iter::once(project.name).chain(project.aliases) {
            if !alias.is_empty() && alias.len() <= 256 {
                entries.push(excalibur_master::LexiconEntry {
                    alias,
                    id: project.id.clone(),
                    kind: "project".into(),
                });
            }
        }
    }
    let tasks=orchestrator.database().read(excalibur_store::ReadLane::Interactive,|c|{
        let mut q=c.prepare("SELECT id,code,title FROM tasks WHERE derived_state NOT IN ('completed','cancelled','failed') ORDER BY id LIMIT 10000")?;
        Ok(q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?)))?.collect::<Result<Vec<_>,_>>()?)
    }).await.map_err(|e|e.to_string())?;
    for (id, code, title) in tasks {
        for alias in code.into_iter().chain(title) {
            if !alias.is_empty() && alias.len() <= 256 {
                entries.push(excalibur_master::LexiconEntry {
                    alias,
                    id: id.clone(),
                    kind: "task".into(),
                });
            }
        }
    }
    entries.truncate(100000);
    master.set_lexicon(entries).map_err(|e| e.to_string())?;
    let now = excalibur_store::unix_seconds();
    let last=master.database().read(excalibur_store::ReadLane::Interactive,|c|Ok(c.query_row("SELECT MAX(m.sent_at) FROM messages m JOIN master_client_receipts r ON r.message_id=m.id",[],|r|r.get::<_,Option<i64>>(0))?)).await.map_err(|e|e.to_string())?;
    lanes.set_user_active(last.is_some_and(|at| now.saturating_sub(at) < 600));
    master
        .tick_questions(now)
        .await
        .map_err(|e| e.to_string())?;
    master.retry_pending().await.map_err(|e| e.to_string())?;
    deliver_notifications(&master, &orchestrator, now).await
}

/// Deliver the shared durable notification outbox without changing its scope.
pub async fn deliver_notifications(
    master: &excalibur_master::Master,
    orchestrator: &excalibur_orchestrator::Orchestrator,
    now: i64,
) -> Result<(), String> {
    for delivery in orchestrator
        .pending_deliveries(now)
        .await
        .map_err(|e| e.to_string())?
    {
        deliver_notification(master, orchestrator, &delivery).await?;
    }
    master
        .flush_notices_checked(now, move |c, origin| {
            excalibur_orchestrator::host_notification_effect_allowed(c, origin, now)
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Attempt one previously fetched outbox delivery. Authority is checked again
/// here so a revocation between polling and sending retains the pending attempt.
pub async fn deliver_notification(
    master: &excalibur_master::Master,
    orchestrator: &excalibur_orchestrator::Orchestrator,
    delivery: &excalibur_orchestrator::Delivery,
) -> Result<(), String> {
    if !delivery.channel.starts_with("conversation:") && delivery.channel != "conversation" {
        return Ok(());
    }
    let target = delivery
        .cross_conversation
        .as_ref()
        .unwrap_or(&delivery.notification.conversation);
    let host_scope = orchestrator
        .recovery()
        .notification_scope(&delivery.notification.id)
        .await
        .map_err(|e| e.to_string())?;
    // Revalidate after retrieval, immediately before projecting/publishing.
    // Revocation does not fall back to legacy agent or JSON authority.
    let authority = orchestrator
        .host_notification_authority(&delivery.notification.id)
        .await
        .map_err(|e| e.to_string())?;
    let (scope, identity) = match authority {
        excalibur_orchestrator::HostNotificationAuthority::Revoked(_) => return Ok(()),
        excalibur_orchestrator::HostNotificationAuthority::Authorized(origin) => {
            (origin.scope, json!({"host":true,"source_kind":"host"}))
        }
        excalibur_orchestrator::HostNotificationAuthority::NotHost => {
            if let Some(scope) = host_scope {
                (scope, json!({"host":true}))
            } else {
                let Ok(agent) = orchestrator.agent(&delivery.notification.agent_id).await else {
                    return Ok(());
                };
                (agent.scope, json!({"name":agent.name,"agent_id":agent.id}))
            }
        }
    };
    let content = if delivery.cross_conversation.is_some() {
        json!({"notification_id":delivery.notification.id,"origin":delivery.notification.conversation,"scope":scope,"summary":"Urgent update in the origin conversation"})
    } else {
        let mut content = json!({"notification_id":delivery.notification.id,"content":delivery.notification.content,"origin":delivery.notification.conversation,"scope":scope,"created_at":delivery.notification.created});
        content
            .as_object_mut()
            .unwrap()
            .extend(identity.as_object().unwrap().clone());
        content
    };
    let key = format!("{}:{}", delivery.notification.id, delivery.channel);
    let notice_id = delivery.notification.id.clone();
    let class = if delivery.cross_conversation.is_some() {
        "informational"
    } else {
        &delivery.notification.class
    };
    let origin = notice_id.clone();
    let published = master
        .publish_notice_checked(&key, target, content, class, &origin, move |c| {
            excalibur_orchestrator::host_notification_effect_allowed(
                c,
                &notice_id,
                excalibur_store::unix_seconds(),
            )
        })
        .await;
    if published.is_ok() {
        orchestrator
            .confirm_delivery(&delivery.notification.id, &delivery.channel)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn configured_model(
    model: String,
    endpoint: Option<String>,
    key_variable: &str,
    role: ModelRole,
    redactor: &Redactor,
) -> Result<Arc<dyn ModelGateway>, String> {
    let key = std::env::var(key_variable)
        .map_err(|_| format!("{key_variable} required for configured model"))?;
    redactor.register_secret(&SecretValue::new(key.clone()).map_err(|_| "invalid provider key")?);
    let key = Secret::new(key).map_err(|_| "invalid provider key")?;
    let provider = match endpoint {
        Some(endpoint) => ProviderClient::with_base_url(Provider::OpenAi, key, &endpoint),
        None => ProviderClient::new(Provider::OpenAi, key),
    }
    .map_err(|_| "provider configuration invalid")?;
    Ok(Arc::new(
        OpenAiGateway::new(provider, GatewayConfig::new(model, role))
            .map_err(|_| "model configuration invalid")?,
    ))
}

#[cfg(test)]
mod configuration_tests {
    #[test]
    fn fallback_is_explicit_and_has_an_independent_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let args = [
            "start",
            "--project",
            dir.path().to_str().unwrap(),
            "--data-dir",
            "private",
            "--model",
            "primary-fixture",
            "--fallback-model",
            "fallback-fixture",
            "--fallback-base-url",
            "http://127.0.0.1:9999/v1",
        ];
        let parsed = super::options(&args.map(str::to_owned)).unwrap();
        assert_eq!(parsed.fallback_model.as_deref(), Some("fallback-fixture"));
        assert_eq!(
            parsed.fallback_endpoint.as_deref(),
            Some("http://127.0.0.1:9999/v1")
        );
        assert!(parsed.endpoint.is_none());
    }
}

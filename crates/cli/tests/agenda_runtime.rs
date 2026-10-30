#![cfg(unix)]
//! Real foreground daemon: the automatic Agenda recomputes missed reminder
//! fires at start, wakes at the earliest durable timer, and SIGKILL/restart
//! does not duplicate admitted notices. Local fixtures and synthetic token only.
use excalibur_daemon::{DaemonHost, HostOptions, RuntimeSet, agenda::Agenda};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_scheduler::{
    Timing,
    reminders::{Definition, Operation},
};
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::{BoxFuture, ModelRequest, ModelResponse};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
struct Offline;
impl ModelGateway for Offline {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async { Err(ModelError::Provider) })
    }
}
struct Daemon(std::process::Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
const TOKEN: &str = "synthetic-agenda-runtime-token-000000";
fn rpc(address: SocketAddr, method: &str, params: Value) -> std::io::Result<Value> {
    let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
    write!(
        socket,
        "POST /rpc HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut response = String::new();
    socket.read_to_string(&mut response)?;
    let (_, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("missing HTTP body"))?;
    serde_json::from_str(body).map_err(std::io::Error::other)
}
fn start(project: &Path, data: &Path, log: &Path) -> (Daemon, SocketAddr) {
    let address = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_excalibur"))
            .args(["daemon", "start", "--project"])
            .arg(project)
            .arg("--data-dir")
            .arg(data)
            .args(["--listen", &address.to_string()])
            .env("EXCALIBUR_DAEMON_TOKEN", TOKEN)
            .env_remove("OPENAI_MODEL")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_FALLBACK_MODEL")
            .env_remove("OPENAI_FALLBACK_API_KEY")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(log).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if rpc(address, "daemon.recovery.status", json!({})).is_ok() {
            return (daemon, address);
        }
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon stopped: {}",
            std::fs::read_to_string(log).unwrap()
        );
        assert!(Instant::now() < deadline, "daemon startup deadline");
        std::thread::sleep(Duration::from_millis(30));
    }
}
fn result(address: SocketAddr, method: &str, params: Value) -> Value {
    let response = rpc(address, method, params).unwrap();
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}
fn notices(address: SocketAddr) -> Vec<Value> {
    result(
        address,
        "notification.list",
        json!({"conversation":"agenda"}),
    )
    .as_array()
    .unwrap()
    .iter()
    // Recovery summaries share the conversation; count reminder notices only.
    .filter(|n| {
        n["content"]
            .as_str()
            .is_some_and(|c| c.starts_with("reminder "))
    })
    .cloned()
    .collect()
}
fn with_text<'a>(all: &'a [Value], id: &str) -> Vec<&'a Value> {
    let text = format!("reminder {id}");
    all.iter().filter(|n| n["content"] == text).collect()
}
fn create(id: &str, timing: Timing) -> Operation {
    Operation::Create(Definition {
        id: id.into(),
        text: format!("reminder {id}"),
        timing,
        preferred_channel: None,
        task: None,
    })
}

/// Seed reminders through the explicit host API while no daemon is running.
fn seed(project: &Path, data: &Path, base: i64) {
    std::fs::create_dir_all(data).unwrap();
    let db = Database::open(
        data.join("excalibur.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let rt = RuntimeSet::new(2, 1).unwrap();
    let options = HostOptions::project(project.canonicalize().unwrap());
    let principal = options.master_config.principal.clone();
    let host = rt
        .block_on(DaemonHost::open(
            db,
            options,
            Arc::new(Offline),
            None,
            TOKEN,
            Default::default(),
            rt.work(),
        ))
        .unwrap();
    rt.block_on(async {
        let receipt = host
            .master
            .receive(excalibur_master::Incoming {
                conversation: "agenda".into(),
                client_id: "client".into(),
                client_msg_id: "creator".into(),
                text: "literal reminder request".into(),
                scope: Some("project:default".into()),
                timezone: "UTC".into(),
                channel: "fixture".into(),
                reply_to: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        host.master.stop_turn("agenda").await.unwrap();
        let agenda = Agenda::open(
            host.orchestrator.clone(),
            excalibur_orchestrator::HostNotificationBinding {
                principal,
                scope: "project:default".into(),
                conversation: "agenda".into(),
                source: receipt.message_id,
                approved_channels: vec![],
            },
        )
        .await
        .unwrap();
        // Three hourly fires were missed before the daemon starts.
        agenda
            .apply(
                "c-hourly".into(),
                create(
                    "hourly",
                    Timing::Every {
                        first: base - 10_000,
                        seconds: 3600,
                    },
                ),
            )
            .await
            .unwrap();
        agenda
            .apply("c-soon".into(), create("soon", Timing::At(base + 10)))
            .await
            .unwrap();
    });
    rt.block_on(host.close()).unwrap();
}

#[test]
fn foreground_daemon_runs_agenda_with_missed_policy_min_wake_and_restart_dedupe() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let data = temp.path().join("data");
    let base = excalibur_store::unix_seconds();
    seed(&project, &data, base);
    let (mut daemon, address) = start(&project, &data, &temp.path().join("boot-0.log"));
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut early_check = false;
    let all = loop {
        let before = excalibur_store::unix_seconds();
        let all = notices(address);
        if !with_text(&all, "hourly").is_empty() && before < base + 10 {
            assert!(
                with_text(&all, "soon").is_empty(),
                "future reminder admitted before its time"
            );
            early_check = true;
        }
        if all.len() >= 2 {
            break all;
        }
        assert!(Instant::now() < deadline, "agenda notices not admitted");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        early_check,
        "missed fire must be admitted at startup, before the next timer"
    );
    let hourly = with_text(&all, "hourly");
    assert_eq!(hourly.len(), 1, "run_once_now admits one missed fire");
    assert_eq!(hourly[0]["created"], base - 10_000 + 2 * 3600);
    assert_eq!(with_text(&all, "soon")[0]["created"], base + 10);
    assert!(excalibur_store::unix_seconds() >= base + 10);

    // SIGKILL and restart: startup catch-up runs again but fire IDs dedupe.
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    let (mut daemon, address) = start(&project, &data, &temp.path().join("boot-1.log"));
    assert_eq!(
        result(address, "daemon.recovery.status", json!({}))["recent_crashes"],
        1
    );
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(notices(address).len(), 2, "no duplicate after restart");
    result(address, "daemon.stop", json!({}));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            assert!(status.success(), "agenda task stopped with the daemon");
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not stop");
        std::thread::sleep(Duration::from_millis(30));
    }
}

//! Agenda user surface end to end: the master's `reminders` fast tool driven
//! by a scripted local model, and the authenticated `agenda.reminders` RPC over
//! local HTTP, both against the real daemon host, guarded notification path,
//! scheduler reminder domain and automatic runtime. No network or paid API.
use excalibur_daemon::{
    DaemonHost, HostOptions, RuntimeSet,
    agenda::{AgendaClock, AgendaRuntime, ManualClock},
};
use excalibur_master::Incoming;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::Notification;
use excalibur_store::{Database, DatabaseKind};
use excalibur_types::runtime::*;
use excalibur_vault::{Redactor, SecretValue};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};

const TOKEN: &str = "synthetic-agenda-tools-token-00000000";

struct Offline;
impl ModelGateway for Offline {
    fn complete(&self, _: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async { Err(ModelError::Provider) })
    }
}
type Call = (ModelRequest, oneshot::Sender<ModelResponse>);
struct Scripted(mpsc::UnboundedSender<Call>);
impl ModelGateway for Scripted {
    fn complete(&self, request: ModelRequest) -> BoxFuture<'_, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            self.0
                .send((request, tx))
                .map_err(|_| ModelError::Provider)?;
            rx.await.map_err(|_| ModelError::Provider)
        })
    }
}
fn reply(call: Option<(&str, Value)>, id: &str) -> ModelResponse {
    let mut message = Message::text("assistant", if call.is_some() { "" } else { "noted" });
    if let Some((name, args)) = call {
        message.tool_calls.push(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        });
    }
    ModelResponse {
        message,
        usage: Usage::default(),
    }
}
async fn next(rx: &mut mpsc::UnboundedReceiver<Call>) -> Call {
    tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
fn tool_result(request: &ModelRequest, id: &str) -> Value {
    let message = request
        .messages
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some(id))
        .expect("tool result in the next model request");
    serde_json::from_str(&message.content).unwrap()
}

struct Fixture {
    rt: RuntimeSet,
    host: DaemonHost,
    redactor: Redactor,
    _temp: tempfile::TempDir,
}
fn fixture(model: Arc<dyn ModelGateway>) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let rt = RuntimeSet::new(2, 1).unwrap();
    let redactor = Redactor::default();
    let host = rt
        .block_on(DaemonHost::open(
            db,
            HostOptions::project(root),
            model,
            None,
            TOKEN,
            redactor.clone(),
            rt.work(),
        ))
        .unwrap();
    Fixture {
        rt,
        host,
        redactor,
        _temp: temp,
    }
}
async fn serve(host: &DaemonHost) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    let router = host.gateway.clone().router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
}
async fn rpc(url: &str, method: &str, params: Value) -> Value {
    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(TOKEN)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    response.json().await.unwrap()
}
/// Accept a user message through the RPC and return its message id.
async fn accepted(
    host: &DaemonHost,
    url: &str,
    conversation: &str,
    id: &str,
    zone: &str,
) -> String {
    let body = rpc(
        url,
        "conversation.send",
        json!({"conversation":conversation,"client_id":"fixture","client_msg_id":id,"text":"reminder request","scope":"project:default","timezone":zone}),
    )
    .await;
    assert!(body.get("error").is_none(), "{body}");
    host.master.stop_turn(conversation).await.unwrap();
    body["result"]["message_id"].as_str().unwrap().to_owned()
}
async fn reminders(
    url: &str,
    conversation: &str,
    scope: &str,
    source: &str,
    operation: &str,
    args: Value,
) -> Value {
    rpc(
        url,
        "agenda.reminders",
        json!({"conversation":conversation,"scope":scope,"source":source,"operation":operation,"args":args}),
    )
    .await
}
fn ok(body: &Value) -> &Value {
    assert!(body.get("error").is_none(), "{body}");
    &body["result"]
}
fn error(body: &Value) -> String {
    body["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an error: {body}"))
        .to_owned()
}
async fn notices(host: &DaemonHost, conversation: &str) -> Vec<Notification> {
    let mut all = host.orchestrator.notifications(conversation).await.unwrap();
    all.retain(|n| n.content.starts_with("reminder "));
    all
}
async fn wait_for(host: &DaemonHost, conversation: &str, count: usize, within: Duration) {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let current = notices(host, conversation).await.len();
        if current >= count {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {count} notices, have {current}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
/// Wait until the supervised runtime is idle (no pass for 2.5 s).
async fn settled(runtime: &AgendaRuntime) {
    loop {
        let before = runtime.passes();
        tokio::time::sleep(Duration::from_millis(2500)).await;
        if runtime.passes() == before && before > 0 {
            return;
        }
    }
}
/// Proleptic Gregorian civil date of a day count since the Unix epoch.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[test]
fn master_tool_creates_through_the_real_master_and_wakes_the_runtime() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let f = fixture(Arc::new(Scripted(tx)));
    let base = excalibur_store::unix_seconds();
    f.rt.block_on(async {
        let clock = ManualClock::new(base);
        let runtime = AgendaRuntime::new(f.host.orchestrator.clone(), clock.clone());
        let (stop, stopping) = tokio::sync::watch::channel(false);
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let task = runtime
            .clone()
            .spawn(&tokio::runtime::Handle::current(), shutdown, stopping);
        settled(&runtime).await;
        let idle = runtime.passes();

        // The tool is advertised to the master model as a fast tool.
        f.host
            .master
            .receive(Incoming {
                conversation: "tool-chat".into(),
                client_id: "fixture".into(),
                client_msg_id: "m1".into(),
                text: "remind me to water the plants now".into(),
                scope: Some("project:default".into()),
                timezone: "UTC".into(),
                channel: "test".into(),
                reply_to: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        let (request, respond) = next(&mut rx).await;
        assert!(request.tools.iter().any(|t| t.name == "reminders"));
        respond
            .send(reply(
                Some((
                    "reminders",
                    json!({"action":"create","text":"reminder water plants","timing":{"at":base}}),
                )),
                "call-create",
            ))
            .unwrap();
        let (request, respond) = next(&mut rx).await;
        let created = tool_result(&request, "call-create");
        assert_eq!(created["reminder"]["generation"], 1, "{created}");
        assert_eq!(created["reminder"]["state"], "Active");
        assert_eq!(created["reminder"]["next_fire_utc"], base);
        let id = created["reminder"]["id"].as_str().unwrap().to_owned();

        // A forged authority field is refused by the approved schema.
        respond
            .send(reply(
                Some((
                    "reminders",
                    json!({"action":"list","scope":"project:other","principal":"intruder"}),
                )),
                "call-forged",
            ))
            .unwrap();
        let (request, respond) = next(&mut rx).await;
        assert!(
            tool_result(&request, "call-forged")["error"]
                .as_str()
                .unwrap()
                .contains("schema")
        );
        respond
            .send(reply(
                Some(("reminders", json!({"action":"list"}))),
                "call-list",
            ))
            .unwrap();
        let (request, respond) = next(&mut rx).await;
        let listed = tool_result(&request, "call-list");
        assert_eq!(listed["reminders"][0]["id"], id.as_str(), "{listed}");
        respond.send(reply(None, "done")).unwrap();

        // The mutation signal woke the idle runtime: the clock never moved and
        // the monotonic re-check (60 s) is longer than this deadline.
        wait_for(&f.host, "tool-chat", 1, Duration::from_secs(20)).await;
        assert!(runtime.passes() > idle);
        assert_eq!(clock.now(), base);
        let delivered = notices(&f.host, "tool-chat").await;
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].content, "reminder water plants");

        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    });
    f.rt.block_on(f.host.close()).unwrap();
}

#[test]
fn rpc_replays_by_key_and_denies_other_scopes_sources_and_conversations() {
    let f = fixture(Arc::new(Offline));
    let base = excalibur_store::unix_seconds();
    f.rt.block_on(async {
        let url = serve(&f.host).await;
        let a = accepted(&f.host, &url, "chat-a", "a1", "UTC").await;
        let b = accepted(&f.host, &url, "chat-b", "b1", "UTC").await;
        let create = json!({"action":"create","text":"reminder call the bank","timing":{"at":base + 3600}});

        // Exact keyed replay returns the identical snapshot.
        let first = reminders(&url, "chat-a", "project:default", &a, "op-1", create.clone()).await;
        let replay = reminders(&url, "chat-a", "project:default", &a, "op-1", create.clone()).await;
        assert_eq!(ok(&first), ok(&replay));
        let id = ok(&first)["reminder"]["id"].as_str().unwrap().to_owned();
        let reused = reminders(
            &url,
            "chat-a",
            "project:default",
            &a,
            "op-1",
            json!({"action":"create","text":"reminder something else","timing":{"at":base + 3600}}),
        )
        .await;
        assert!(error(&reused).contains("reused"), "{reused}");

        // Generation CAS: a stale generation is refused; the current one wins.
        let snooze = json!({"action":"snooze","id":id,"generation":1,"timing":{"at":base + 7200}});
        let snoozed = reminders(&url, "chat-a", "project:default", &a, "op-2", snooze.clone()).await;
        assert_eq!(ok(&snoozed)["reminder"]["generation"], 2);
        assert_eq!(ok(&snoozed)["reminder"]["next_fire_utc"], base + 7200);
        let stale = reminders(&url, "chat-a", "project:default", &a, "op-3", snooze).await;
        assert!(error(&stale).contains("generation"), "{stale}");

        // Scope denial: the accepted source belongs to project:default, so a
        // client-named general scope cannot bind it, and an unregistered
        // project is refused before the core.
        let general = reminders(&url, "chat-a", "general", &a, "op-4", json!({"action":"list"})).await;
        assert!(error(&general).contains("not authorized"), "{general}");
        let unknown = reminders(&url, "chat-a", "project:other", &a, "op-5", json!({"action":"list"})).await;
        assert!(body_error_any(&unknown));
        // A source accepted in another conversation is not authority here.
        let foreign = reminders(&url, "chat-a", "project:default", &b, "op-6", json!({"action":"list"})).await;
        assert!(error(&foreign).contains("not authorized"), "{foreign}");
        // A fabricated source id is refused.
        let forged = reminders(&url, "chat-a", "project:default", "msg_forged", "op-7", json!({"action":"list"})).await;
        assert!(error(&forged).contains("not authorized"), "{forged}");
        // Conversation B's own namespace neither sees nor mutates A's reminder.
        let listed = reminders(&url, "chat-b", "project:default", &b, "op-8", json!({"action":"list"})).await;
        assert_eq!(ok(&listed)["reminders"], json!([]));
        let cross = reminders(
            &url,
            "chat-b",
            "project:default",
            &b,
            "op-9",
            json!({"action":"delete","id":id,"generation":2}),
        )
        .await;
        assert!(error(&cross).contains("does not exist"), "{cross}");
        // Unauthenticated callers never reach the Agenda.
        let anonymous = reqwest::Client::new()
            .post(&url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"agenda.reminders","params":{}}))
            .send()
            .await
            .unwrap();
        assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Secret literals are rejected, never stored or silently redacted.
        let secret = reminders(
            &url,
            "chat-a",
            "project:default",
            &a,
            "op-10",
            json!({"action":"create","text":"reminder wifi password: hunter2","timing":{"at":base + 60}}),
        )
        .await;
        assert!(error(&secret).contains("secret literal"), "{secret}");
        assert!(!secret.to_string().contains("hunter2"));

        // AR-10: a create that succeeded replays exactly even after the
        // redactor learns a matching secret; a new key with that text fails.
        let token_text = "reminder rotate key zq81-synthetic-rotation-value";
        let created = reminders(
            &url,
            "chat-a",
            "project:default",
            &a,
            "op-11",
            json!({"action":"create","text":token_text,"timing":{"at":base + 600}}),
        )
        .await;
        let snapshot = ok(&created).clone();
        f.redactor
            .register_secret(&SecretValue::new("zq81-synthetic-rotation-value").unwrap());
        let replayed = reminders(
            &url,
            "chat-a",
            "project:default",
            &a,
            "op-11",
            json!({"action":"create","text":token_text,"timing":{"at":base + 600}}),
        )
        .await;
        assert_eq!(
            ok(&replayed)["reminder"]["id"],
            snapshot["reminder"]["id"],
            "{replayed}"
        );
        assert_eq!(
            ok(&replayed)["reminder"]["generation"],
            snapshot["reminder"]["generation"]
        );
        let fresh = reminders(
            &url,
            "chat-a",
            "project:default",
            &a,
            "op-12",
            json!({"action":"create","text":token_text,"timing":{"at":base + 600}}),
        )
        .await;
        assert!(error(&fresh).contains("secret literal"), "{fresh}");

        // Complete and delete through the same surface.
        let done = reminders(
            &url,
            "chat-a",
            "project:default",
            &a,
            "op-13",
            json!({"action":"complete","id":id,"generation":2}),
        )
        .await;
        assert_eq!(ok(&done)["reminder"]["state"], "Completed");
        assert_eq!(ok(&done)["reminder"]["next_fire_utc"], Value::Null);
        let rotate = snapshot["reminder"]["id"].as_str().unwrap();
        let deleted = reminders(
            &url,
            "chat-a",
            "project:default",
            &a,
            "op-14",
            json!({"action":"delete","id":rotate,"generation":1}),
        )
        .await;
        assert_eq!(ok(&deleted)["reminder"]["state"], "Deleted");
    });
    f.rt.block_on(f.host.close()).unwrap();
}
fn body_error_any(body: &Value) -> bool {
    body.get("error").is_some()
}

#[test]
fn time_zone_change_recomputes_civil_reminders_and_the_runtime_follows() {
    let f = fixture(Arc::new(Offline));
    let base = excalibur_store::unix_seconds();
    let day = base.div_euclid(86_400) + 2;
    let (y, m, d) = civil(day);
    let local = format!("{y:04}-{m:02}-{d:02}T09:00:00");
    let utc_nine = day * 86_400 + 9 * 3600;
    // America/Lima has no daylight saving time: UTC-5 all year.
    let lima_nine = utc_nine + 5 * 3600;
    f.rt.block_on(async {
        let url = serve(&f.host).await;
        let source = accepted(&f.host, &url, "zone-chat", "z1", "UTC").await;
        // The civil zone defaults to the zone recorded with the source message.
        let civil = reminders(
            &url,
            "zone-chat",
            "project:default",
            &source,
            "z-create",
            json!({"action":"create","id":"civil","text":"reminder civil nine","timing":{"local_at":{"local":local}}}),
        )
        .await;
        assert_eq!(ok(&civil)["reminder"]["next_fire_utc"], utc_nine, "{civil}");
        assert_eq!(ok(&civil)["reminder"]["timing"]["LocalAt"]["timezone"], "UTC");
        let fixed = reminders(
            &url,
            "zone-chat",
            "project:default",
            &source,
            "z-fixed",
            json!({"action":"create","id":"fixed","text":"reminder fixed utc","timing":{"at":utc_nine + 60}}),
        )
        .await;
        ok(&fixed);
        let cron = reminders(
            &url,
            "zone-chat",
            "project:default",
            &source,
            "z-cron",
            json!({"action":"create","id":"daily","text":"reminder daily","timing":{"cron":{"expression":"0 9 * * *","not_before":utc_nine}}}),
        )
        .await;
        assert_eq!(ok(&cron)["reminder"]["next_fire_utc"].as_i64().unwrap() % 86_400, 9 * 3600);

        let clock = ManualClock::new(base);
        let runtime = AgendaRuntime::new(f.host.orchestrator.clone(), clock.clone());
        let (stop, stopping) = tokio::sync::watch::channel(false);
        let task = runtime.clone().spawn(
            &tokio::runtime::Handle::current(),
            Arc::new(tokio::sync::Notify::new()),
            stopping,
        );
        settled(&runtime).await;

        let change = json!({"action":"change_timezone","timezone":"America/Lima"});
        let report = reminders(&url, "zone-chat", "project:default", &source, "z-zone", change.clone()).await;
        let report = ok(&report).clone();
        assert_eq!(report["timezone"], "America/Lima");
        let changed = report["changed"].as_array().unwrap();
        assert_eq!(changed.len(), 2, "{report}");
        let civil_move = changed.iter().find(|c| c["id"] == "civil").unwrap();
        assert_eq!(civil_move["old_timezone"], "UTC");
        assert_eq!(civil_move["previous_next"], utc_nine);
        assert_eq!(civil_move["next"], lima_nine);
        assert_eq!(civil_move["generation"], 2);
        let cron_move = changed.iter().find(|c| c["id"] == "daily").unwrap();
        assert_eq!(cron_move["next"].as_i64().unwrap() % 86_400, 14 * 3600);
        assert!(!changed.iter().any(|c| c["id"] == "fixed"), "UTC timing unchanged");
        // Keyed replay returns the same report; the zone is not re-applied.
        let again = reminders(&url, "zone-chat", "project:default", &source, "z-zone", change).await;
        assert_eq!(ok(&again), &report);
        let listed = reminders(&url, "zone-chat", "project:default", &source, "z-list", json!({"action":"list"})).await;
        let civil_row = ok(&listed)["reminders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "civil")
            .unwrap()
            .clone();
        assert_eq!(civil_row["timing"]["LocalAt"]["timezone"], "America/Lima");
        assert_eq!(civil_row["next_fire_utc"], lima_nine);

        // The runtime follows the recomputed durable timer: nothing at the
        // old UTC instant (except the unchanged UTC reminder), the civil
        // reminder at the new one.
        clock.set(utc_nine + 120);
        wait_for(&f.host, "zone-chat", 1, Duration::from_secs(20)).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let early = notices(&f.host, "zone-chat").await;
        assert_eq!(early.len(), 1);
        assert_eq!(early[0].content, "reminder fixed utc");
        clock.set(lima_nine + 1);
        wait_for(&f.host, "zone-chat", 3, Duration::from_secs(20)).await;
        let all = notices(&f.host, "zone-chat").await;
        let civil_notice = all
            .iter()
            .find(|n| n.content == "reminder civil nine")
            .expect("civil reminder delivered after the zone change");
        assert_eq!(civil_notice.created, lima_nine);
        assert!(all.iter().any(|n| n.content == "reminder daily"));

        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    });
    f.rt.block_on(f.host.close()).unwrap();
}

//! Read-only browser observation, readiness, reference validation and private blobs.
mod access;
pub mod actions;
pub mod detectors;
mod files;
mod tree;
mod wait;
use crate::sessions::{Fault, Result, Session, Tab};
use excalibur_cdp::{Client, protocol::Method};
use excalibur_store::{BlobOwner, Store};
use excalibur_vault::Redactor;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::OnceCell;
pub use tree::{Element, ElementKey, Rect, TreeNode};
pub use wait::{WaitCriteria, WaitResult};
#[derive(Clone)]
pub(crate) struct Context {
    pub session: Session,
    pub tab: Tab,
    pub client: Client,
    pub logged_in: bool,
}
#[derive(Clone, Debug)]
pub(crate) struct Frame {
    pub id: String,
    pub loader: String,
    pub url: String,
    pub security_origin: String,
    pub session: String,
    pub target: String,
    pub parent: Option<String>,
    pub offset: (f64, f64),
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotOptions {
    pub screenshot: bool,
    pub dom: String,
    pub extent: String,
    pub wait: Option<WaitCriteria>,
}
impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            screenshot: false,
            dom: "interactive".into(),
            extent: "viewport".into(),
            wait: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlobRef {
    pub sha256: String,
    pub bytes: u64,
    pub media_type: String,
    pub storage_compression: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub snapshot_id: String,
    pub tab_id: String,
    pub url: String,
    pub title: String,
    pub load_state: String,
    pub screenshot_ref: Option<BlobRef>,
    pub dom_tree_ref: BlobRef,
    pub elements: Vec<Element>,
    pub text: String,
    pub omitted: usize,
    pub partial: bool,
    pub missing: Vec<String>,
    pub wait: WaitResult,
    pub untrusted: bool,
    pub vision_recommended: bool,
    #[serde(default)]
    pub vision_suppressed: bool,
    pub document_epoch: u64,
}
#[derive(Clone)]
struct Saved {
    snapshot: Snapshot,
    run: String,
    tab: String,
    keys: HashMap<String, ElementKey>,
    fingerprints: HashMap<String, String>,
    invalidated: u64,
    target: String,
    session: String,
}
#[derive(Default)]
struct Book {
    frames: HashMap<String, u64>,
    refs: HashMap<ElementKey, String>,
    current: Option<String>,
    saved: HashMap<String, Saved>,
    invalidation: u64,
    reason: Option<String>,
}
struct EventState {
    sessions: Vec<String>,
    inflight: HashMap<(String, String), (tokio::time::Instant, String, String)>,
    console: VecDeque<Value>,
    network: VecDeque<Value>,
    sequence: u64,
    dropped: u64,
    dialog: Option<Value>,
    response_blocked: Option<Value>,
    /// Live top-level document HTTP status per frame (`Network.responseReceived`
    /// with `type == "Document"`). Wired into `wait` so `elevate_with_status`
    /// corroborates probe blocks against the real network state instead of
    /// staying unit-only. Cleared when a new navigation starts so a stale
    /// status never elevates the next document.
    document_statuses: std::collections::HashMap<String, i64>,
    file_chooser: bool,
    tainted: bool,
    lagged: bool,
    navigation_pending: bool,
    unobserved_sessions: HashMap<String, String>,
    last_network_activity: tokio::time::Instant,
    active_actions: usize,
    active_navigation: usize,
    action_alerts: Vec<Value>,
}
struct TabObserver {
    target: String,
    session: String,
    state: Arc<Mutex<EventState>>,
    book: tokio::sync::Mutex<Book>,
    last_ready: Mutex<Option<(BTreeMap<String, String>, WaitResult)>>,
    task: tokio::task::AbortHandle,
    previous: Option<Arc<TabObserver>>,
    initialized: tokio::sync::Mutex<std::collections::HashSet<String>>,
    network_users: std::sync::atomic::AtomicUsize,
    network_enabled: Mutex<std::collections::HashSet<String>>,
}
impl Drop for TabObserver {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub struct Observation {
    root: PathBuf,
    store: OnceCell<excalibur_store::Database>,
    tabs: Mutex<HashMap<String, Arc<TabObserver>>>,
    pub redactor: Redactor,
    generation: String,
    next_ref: std::sync::atomic::AtomicU64,
    pub(crate) actions: actions::ActionState,
}
impl Observation {
    pub(crate) fn new(root: PathBuf, redactor: Redactor) -> Self {
        Self {
            root,
            store: OnceCell::new(),
            tabs: Mutex::new(HashMap::new()),
            redactor,
            generation: uuid::Uuid::new_v4().to_string(),
            next_ref: std::sync::atomic::AtomicU64::new(1),
            actions: Default::default(),
        }
    }
    async fn store(&self) -> Result<&excalibur_store::Database> {
        self.store
            .get_or_try_init(|| async {
                let path = self.root.join("observations");
                tokio::task::spawn_blocking(move || {
                    use std::os::unix::fs::DirBuilderExt;
                    if !path.exists() {
                        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
                    }
                    use std::os::unix::fs::MetadataExt;
                    let m = std::fs::symlink_metadata(&path)?;
                    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0
                    {
                        return Err(Fault::new("denied", "observation storage must be private"));
                    }
                    excalibur_store::Database::open(
                        path.join("observations.db"),
                        excalibur_store::DatabaseKind::Main,
                        Default::default(),
                    )
                    .map_err(|e| Fault::new("storage_error", e))
                })
                .await
                .map_err(|e| Fault::new("storage_error", e))?
            })
            .await
    }
    async fn blob(
        &self,
        run: &str,
        snapshot: &str,
        slot: &str,
        bytes: Vec<u8>,
        mime: &str,
    ) -> Result<BlobRef> {
        let size = bytes.len() as u64;
        let hash = self
            .store()
            .await?
            .put_blob(
                bytes,
                BlobOwner {
                    kind: "browser_snapshot".into(),
                    id: format!("{run}:{snapshot}"),
                    slot: slot.into(),
                },
            )
            .await
            .map_err(|e| Fault::new("storage_error", e))?;
        Ok(BlobRef {
            sha256: hash.as_str().into(),
            bytes: size,
            media_type: mime.into(),
            storage_compression: "zstd".into(),
        })
    }
    pub(crate) async fn read_blob(
        &self,
        db: &crate::sessions::Database,
        run: &str,
        hash: &str,
        offset: u64,
        length: usize,
    ) -> Result<Value> {
        let owner = run.to_owned();
        let h = hash.to_owned();
        let allowed=db.call(move|c|{Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM web_observation_blobs WHERE hash=?1 AND owner_run=?2 AND created_at>?3)",rusqlite::params![h,owner,crate::sessions::now()-86400],|r|r.get::<_,bool>(0))?)}).await?;
        if !allowed {
            return Err(Fault::new("denied", "blob_owner"));
        }
        if !(1..=1024 * 1024).contains(&length) {
            return Err(Fault::new("invalid_params", "blob window 1..1048576"));
        }
        let blob = self
            .store()
            .await?
            .read_blob_window(
                hash.parse().map_err(|e| Fault::new("invalid_params", e))?,
                offset,
                length,
            )
            .await
            .map_err(|e| Fault::new("storage_error", e))?;
        use base64::Engine;
        Ok(
            json!({"offset":blob.offset,"total_bytes":blob.total_bytes,"data":base64::engine::general_purpose::STANDARD.encode(blob.bytes)}),
        )
    }
    fn tracker(&self, ctx: &Context) -> Arc<TabObserver> {
        let target = ctx.tab.target_id.as_deref().unwrap_or("");
        let session = ctx.tab.cdp_session.as_deref().unwrap_or("");
        let mut tabs = self.tabs.lock().unwrap();
        if let Some(old) = tabs.get(&ctx.tab.id)
            && old.target == target
            && old.session == session
        {
            return old.clone();
        }
        let state = Arc::new(Mutex::new(EventState {
            sessions: vec![session.into()],
            inflight: HashMap::new(),
            console: VecDeque::new(),
            network: VecDeque::new(),
            sequence: 0,
            dropped: 0,
            dialog: None,
            response_blocked: None,
            document_statuses: Default::default(),
            file_chooser: false,
            tainted: false,
            lagged: false,
            navigation_pending: false,
            unobserved_sessions: Default::default(),
            last_network_activity: tokio::time::Instant::now(),
            active_actions: 0,
            active_navigation: 0,
            action_alerts: vec![],
        }));
        let weak = Arc::downgrade(&state);
        let mut events = ctx.client.subscribe_observation();
        let mut console = ctx.client.subscribe_console();
        let redactor = self.redactor.clone();
        let dialog_client = ctx.client.clone();
        let task = tokio::spawn(async move {
            loop {
                let (received, is_console) = tokio::select! {biased;event=events.recv()=>(event,false),event=console.recv()=>(event,true)};
                match received {
                    Ok(event) => {
                        let Some(state) = weak.upgrade() else { break };
                        let mut s = state.lock().unwrap();
                        if !event
                            .session_id
                            .as_ref()
                            .is_some_and(|id| s.sessions.contains(id))
                        {
                            continue;
                        }
                        if event.method == "Page.javascriptDialogOpening"
                            && ((event.params["type"] == "alert" && s.active_actions > 0)
                                || (event.params["type"] == "beforeunload"
                                    && s.active_navigation > 0))
                        {
                            let client = dialog_client.clone();
                            let sid = event.session_id.clone();
                            tokio::spawn(async move {
                                let _ = client
                                    .send_with_timeout(
                                        Method::PageHandleJavaScriptDialog,
                                        json!({"accept":true}),
                                        sid.as_deref(),
                                        Duration::from_millis(500),
                                    )
                                    .await;
                            });
                        }
                        event_record(&mut s, event, &redactor);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        if let Some(s) = weak.upgrade() {
                            let mut state = s.lock().unwrap();
                            if is_console {
                                state.dropped += n;
                            } else {
                                state.lagged = true;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let previous = tabs.get(&ctx.tab.id).cloned();
        if let Some(old) = &previous {
            old.task.abort();
        }
        let tracker = Arc::new(TabObserver {
            previous,
            network_users: Default::default(),
            network_enabled: Default::default(),
            initialized: Default::default(),
            target: target.into(),
            session: session.into(),
            state,
            book: Default::default(),
            last_ready: Default::default(),
            task: task.abort_handle(),
        });
        tabs.insert(ctx.tab.id.clone(), tracker.clone());
        tracker
    }
    pub async fn invalidate(&self, tab: &str, reason: &str) {
        let tracker = self.tabs.lock().unwrap().get(tab).cloned();
        if let Some(t) = tracker {
            let mut book = t.book.lock().await;
            book.invalidation += 1;
            book.reason = Some(reason.to_owned());
            *t.last_ready.lock().unwrap() = None;
        }
    }
    /// Attach while the target is still blank so no initial request can precede Network.enable.
    pub(crate) async fn prepare_tab(
        &self,
        session: &Session,
        tab: &Tab,
        client: Client,
    ) -> Result<()> {
        if tab.listen {
            return Ok(());
        }
        let mut blank = tab.clone();
        blank.url = "about:blank".into();
        let ctx = Context {
            session: session.clone(),
            tab: blank,
            client,
            logged_in: false,
        };
        let frame = Frame {
            id: tab.target_id.clone().unwrap_or_default(),
            loader: String::new(),
            url: "about:blank".into(),
            security_origin: String::new(),
            session: tab.cdp_session.clone().unwrap_or_default(),
            target: tab.target_id.clone().unwrap_or_default(),
            parent: None,
            offset: (0., 0.),
        };
        self.initialize(&ctx, &[frame]).await?;
        Ok(())
    }
    pub fn mark_tainted(&self, tab: &str) {
        if let Some(t) = self.tabs.lock().unwrap().get(tab) {
            t.state.lock().unwrap().tainted = true;
        }
    }
    pub(crate) async fn frames(&self, ctx: &Context) -> Result<Vec<Frame>> {
        let sid = ctx
            .tab
            .cdp_session
            .as_deref()
            .ok_or_else(|| Fault::new("session_recovering", "CDP session absent"))?;
        let tree = ctx
            .client
            .send_with_timeout(
                Method::PageGetFrameTree,
                json!({}),
                Some(sid),
                Duration::from_millis(500),
            )
            .await?;
        let mut frames = vec![];
        collect_frames(
            &tree["frameTree"],
            sid,
            ctx.tab.target_id.as_deref().unwrap_or(""),
            None,
            &mut frames,
        );
        let attached = ctx.client.attached_sessions();
        for frame in &mut frames {
            if frame.parent.is_some()
                && let Some(e) = attached.iter().find(|e| {
                    e.params["targetInfo"]["type"] == "iframe"
                        && e.params["targetInfo"]["targetId"] == frame.id
                })
            {
                frame.session = e.params["sessionId"].as_str().unwrap_or(sid).into();
                frame.target = frame.id.clone();
            }
        }
        // Chrome omits out-of-process subtrees from the parent's frame tree.
        // Reconcile attachments by parent frame identity, never by origin or backend ID.
        let mut seen = frames
            .iter()
            .map(|f| f.id.clone())
            .collect::<std::collections::HashSet<_>>();
        for _ in 0..attached.len() {
            let mut added = false;
            for e in &attached {
                if e.params["targetInfo"]["type"] != "iframe" {
                    continue;
                }
                let info = &e.params["targetInfo"];
                let Some(id) = info["targetId"].as_str() else {
                    continue;
                };
                if seen.contains(id) {
                    continue;
                }
                let parent = info["parentFrameId"].as_str().or(info["parentId"].as_str());
                let Some(parent) = parent.filter(|p| seen.contains(*p)) else {
                    continue;
                };
                let Some(session) = e.params["sessionId"].as_str() else {
                    continue;
                };
                let tree = ctx
                    .client
                    .send_with_timeout(
                        Method::PageGetFrameTree,
                        json!({}),
                        Some(session),
                        Duration::from_millis(500),
                    )
                    .await;
                if let Ok(tree) = tree {
                    collect_frames(
                        &tree["frameTree"],
                        session,
                        id,
                        Some(parent.into()),
                        &mut frames,
                    );
                } else {
                    frames.push(Frame {
                        id: id.into(),
                        loader: String::new(),
                        url: info["url"].as_str().unwrap_or("").into(),
                        security_origin: String::new(),
                        session: session.into(),
                        target: id.into(),
                        parent: Some(parent.into()),
                        offset: (0., 0.),
                    });
                }
                seen.insert(id.to_owned());
                added = true;
            }
            if !added {
                break;
            }
        }
        for idx in 1..frames.len() {
            let parent = frames[idx]
                .parent
                .as_ref()
                .and_then(|id| frames.iter().find(|f| &f.id == id))
                .cloned();
            if let Some(parent) = parent {
                let owner = ctx
                    .client
                    .send_with_timeout(
                        Method::DOMGetFrameOwner,
                        json!({"frameId":frames[idx].id}),
                        Some(&parent.session),
                        Duration::from_millis(300),
                    )
                    .await;
                if let Ok(owner) = owner
                    && let Ok(model) = ctx
                        .client
                        .send_with_timeout(
                            Method::DOMGetBoxModel,
                            json!({"backendNodeId":owner["backendNodeId"]}),
                            Some(&parent.session),
                            Duration::from_millis(300),
                        )
                        .await
                {
                    let quad = &model["model"]["content"];
                    frames[idx].offset = (
                        parent.offset.0 + quad[0].as_f64().unwrap_or(0.),
                        parent.offset.1 + quad[1].as_f64().unwrap_or(0.),
                    );
                }
            }
        }
        let tracker = self.tracker(ctx);
        {
            let mut state = tracker.state.lock().unwrap();
            state.sessions = frames.iter().map(|f| f.session.clone()).collect();
            let sessions = state.sessions.clone();
            state.inflight.retain(|(sid, _), _| sessions.contains(sid));
            state
                .unobserved_sessions
                .retain(|sid, _| sessions.contains(sid));
        }
        Ok(frames)
    }
}
fn collect_frames(
    tree: &Value,
    session: &str,
    target: &str,
    parent: Option<String>,
    out: &mut Vec<Frame>,
) {
    if let Some(id) = tree["frame"]["id"].as_str() {
        out.push(Frame {
            id: id.into(),
            url: tree["frame"]["url"].as_str().unwrap_or("").into(),
            security_origin: tree["frame"]["securityOrigin"]
                .as_str()
                .unwrap_or("")
                .into(),
            loader: tree["frame"]["loaderId"].as_str().unwrap_or("").into(),
            session: session.into(),
            target: target.into(),
            parent,
            offset: (0., 0.),
        });
        if let Some(children) = tree["childFrames"].as_array() {
            for child in children {
                collect_frames(child, session, target, Some(id.into()), out)
            }
        }
    }
}
fn clean_url(url: &str, tainted: bool) -> String {
    let Ok(mut u) = reqwest::Url::parse(url) else {
        return String::new();
    };
    let _ = u.set_username("");
    let _ = u.set_password(None);
    if tainted {
        u.set_query(None);
        u.set_fragment(None)
    }
    u.to_string()
}
fn event_record(s: &mut EventState, e: excalibur_cdp::Event, r: &Redactor) {
    let sid = e.session_id.unwrap_or_default();
    let p = e.params;
    match e.method.as_str() {
        "Page.javascriptDialogOpening" => {
            if s.active_actions > 0 && p["type"] == "alert" {
                if s.action_alerts.len() == 16 {
                    s.action_alerts.remove(0);
                }
                s.action_alerts.push(json!({"type":"alert","message":r.redact_text(p["message"].as_str().unwrap_or("")),"untrusted":true}));
            }
            s.dialog = Some(
                json!({"kind":"dialog","type":p["type"],"message":r.redact_text(p["message"].as_str().unwrap_or("")),"confidence":"high"}),
            )
        }
        "Page.javascriptDialogClosed" => s.dialog = None,
        "Page.fileChooserOpened" => s.file_chooser = true,
        "Page.frameRequestedNavigation" | "Page.frameStartedNavigating" => {
            s.navigation_pending = true
        }
        "Page.frameNavigated" | "Page.domContentEventFired" => {
            if e.method == "Page.frameNavigated" && p["frame"]["parentId"].is_null() {
                if s.response_blocked
                    .as_ref()
                    .is_some_and(|b| b["url"] != p["frame"]["url"])
                {
                    s.response_blocked = None;
                }
                s.lagged = false;
                let loader = p["frame"]["loaderId"].as_str().unwrap_or("");
                if s.unobserved_sessions
                    .get(&sid)
                    .is_some_and(|previous| previous != loader)
                {
                    s.unobserved_sessions.remove(&sid);
                }
                s.inflight.retain(|(_, id), _| id == loader);
                s.last_network_activity = tokio::time::Instant::now();
            }
            s.navigation_pending = false;
            s.file_chooser = false;
        }
        "Network.requestWillBeSent" => {
            s.last_network_activity = tokio::time::Instant::now();
            let key = (sid.clone(), p["requestId"].as_str().unwrap_or("").into());
            let kind = p["type"].as_str().unwrap_or("");
            if !matches!(
                kind,
                "WebSocket" | "EventSource" | "Ping" | "Media" | "Prefetch"
            ) {
                s.inflight.insert(
                    key,
                    (
                        tokio::time::Instant::now(),
                        clean_url(p["request"]["url"].as_str().unwrap_or(""), s.tainted),
                        kind.into(),
                    ),
                );
            }
            ring(
                s,
                false,
                json!({"event":"request","url":clean_url(p["request"]["url"].as_str().unwrap_or(""),s.tainted),"method":p["request"]["method"],"type":kind}),
                r,
            );
        }
        "Network.loadingFinished" | "Network.loadingFailed" | "Network.requestServedFromCache" => {
            if s.inflight
                .contains_key(&(sid.clone(), p["requestId"].as_str().unwrap_or("").into()))
            {
                s.last_network_activity = tokio::time::Instant::now();
            }
            let request = p["requestId"].as_str().unwrap_or("");
            s.inflight.retain(|(_, id), _| id != request);
        }
        "Network.responseReceived" => {
            if p["type"] == "Document" {
                if let Some(status) = p["response"]["status"].as_i64()
                    && let Some(frame_id) = p["frameId"].as_str()
                {
                    s.document_statuses.insert(frame_id.to_owned(), status);
                }
                if p["response"]["headers"].as_object().is_some_and(|h| {
                    h.iter().any(|(k, v)| {
                        k.eq_ignore_ascii_case("cf-mitigated") && v.as_str() == Some("challenge")
                    })
                }) {
                    s.response_blocked = Some(
                        json!({"kind":"captcha","vendor":"cloudflare","signals":["cf-mitigated"],"confidence":"high","url":clean_url(p["response"]["url"].as_str().unwrap_or(""),s.tainted),"frame_id":p["frameId"]}),
                    );
                }
            }
            ring(
                s,
                false,
                json!({"event":"response","url":clean_url(p["response"]["url"].as_str().unwrap_or(""),s.tainted),"status":p["response"]["status"],"type":p["type"],"encoded_bytes":p["response"]["encodedDataLength"],"timing":p["response"]["timing"]}),
                r,
            );
        }
        "Runtime.consoleAPICalled" => {
            let args = p["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|v| {
                            v["value"]
                                .as_str()
                                .or(v["description"].as_str())
                                .unwrap_or("[object]")
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            ring(
                s,
                true,
                json!({"level":p["type"],"text":args.chars().take(8192).collect::<String>()}),
                r,
            )
        }
        "Runtime.exceptionThrown" => ring(
            s,
            true,
            json!({"level":"error","text":p["exceptionDetails"]["text"],"description":p["exceptionDetails"]["exception"]["description"]}),
            r,
        ),
        "Log.entryAdded" => ring(
            s,
            true,
            json!({"level":p["entry"]["level"],"text":p["entry"]["text"]}),
            r,
        ),
        _ => {}
    }
}
fn ring(s: &mut EventState, console: bool, mut v: Value, r: &Redactor) {
    s.sequence += 1;
    v["seq"] = json!(s.sequence);
    let ring = if console {
        &mut s.console
    } else {
        &mut s.network
    };
    if ring.len() == 500 {
        ring.pop_front();
        s.dropped += 1;
    }
    ring.push_back(r.redact_json(&v));
}

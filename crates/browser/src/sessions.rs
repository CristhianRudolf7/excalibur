//! Durable session ownership and lifecycle. No raw CDP method is exposed by RPC.
use crate::{Class, Instance, LaunchOptions, Status, version::VersionPolicy};
use excalibur_cdp::protocol::Method;
use fs2::FileExt;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    net::SocketAddrV4,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, OwnedMutexGuard};

pub type Result<T> = std::result::Result<T, Fault>;
#[derive(Clone, Debug, Serialize, Deserialize, thiserror::Error)]
#[error("{kind}: {detail}")]
pub struct Fault {
    pub kind: String,
    pub detail: String,
}
impl Fault {
    pub fn new(kind: &str, detail: impl ToString) -> Self {
        Self {
            kind: kind.into(),
            detail: detail.to_string(),
        }
    }
}
impl From<rusqlite::Error> for Fault {
    fn from(e: rusqlite::Error) -> Self {
        Self::new("storage_error", e)
    }
}
impl From<std::io::Error> for Fault {
    fn from(e: std::io::Error) -> Self {
        Self::new("io_error", e)
    }
}
impl From<crate::Error> for Fault {
    fn from(e: crate::Error) -> Self {
        let k = match &e {
            crate::Error::Sandbox(_) => "sandbox_unavailable",
            crate::Error::VersionBlocked(_) => "version_blocked",
            _ => "browser_error",
        };
        Self::new(k, e)
    }
}
impl From<excalibur_cdp::Error> for Fault {
    fn from(e: excalibur_cdp::Error) -> Self {
        Self::new("cdp_error", e)
    }
}
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Site {
    pub name: String,
    pub rw: Vec<String>,
    pub login: Vec<String>,
    pub listen: bool,
    pub listen_probe: ListenProbe,
    pub wait: Option<crate::observation::WaitCriteria>,
    pub credential_frames: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListenProbe {
    pub unread_selector: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    pub class: Class,
    pub isolation: String,
    pub sites: Vec<Site>,
}
impl Default for Profile {
    fn default() -> Self {
        Self {
            class: Class::Persistent,
            isolation: "shared_instance".into(),
            sites: vec![],
        }
    }
}
#[derive(Clone)]
pub struct Options {
    pub data_dir: PathBuf,
    pub binary: PathBuf,
    pub proxy: SocketAddrV4,
    pub profiles: BTreeMap<String, Profile>,
    pub compact: bool,
    pub version: VersionPolicy,
    pub chrome_args: Vec<String>,
}
impl Options {
    pub fn validate(&self) -> Result<()> {
        validate_profiles(&self.profiles)?;
        if !self.data_dir.is_absolute() {
            return Err(Fault::new(
                "invalid_config",
                "absolute data directory required",
            ));
        }
        if self.compact
            && !self
                .profiles
                .values()
                .any(|p| p.class == Class::Persistent && p.isolation == "shared_instance")
        {
            return Err(Fault::new(
                "invalid_config",
                "compact mode requires a shared persistent profile",
            ));
        }
        Ok(())
    }
}
pub fn validate_profiles(profiles: &BTreeMap<String, Profile>) -> Result<()> {
    for (name, p) in profiles {
        if !safe_name(name)
            || name == "ephemeral"
            || name.contains("--")
            || p.class != Class::Persistent
            || !matches!(p.isolation.as_str(), "shared_instance" | "per_site")
        {
            return Err(Fault::new(
                "invalid_config",
                "invalid persistent profile or isolation",
            ));
        }
        let mut names = std::collections::HashSet::new();
        for s in &p.sites {
            if !safe_name(&s.name) || !names.insert(&s.name) || s.rw.is_empty() {
                return Err(Fault::new(
                    "invalid_config",
                    "site requires unique name and document origins",
                ));
            }
            for o in s.rw.iter().chain(&s.login) {
                if origin(o).as_deref() != Some(o.as_str()) {
                    return Err(Fault::new(
                        "invalid_config",
                        "site entries must be exact HTTP(S) origins",
                    ));
                }
            }
            if let Some(wait) = &s.wait {
                wait.validate()
                    .map_err(|e| Fault::new("invalid_config", e))?;
                if s.listen && wait.strict_network {
                    return Err(Fault::new(
                        "invalid_config",
                        "listening sites require non-strict network readiness",
                    ));
                }
            }
            if s.listen_probe.unread_selector.len() > 2048 {
                return Err(Fault::new("invalid_config", "unread selector too long"));
            }
        }
    }
    Ok(())
}
fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn origin(s: &str) -> Option<String> {
    let u = reqwest::Url::parse(s).ok()?;
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some()
    {
        return None;
    }
    Some(u.origin().ascii_serialization())
}
fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o777 != 0o700 {
        return Err(Fault::new(
            "denied",
            "private directory ownership/mode required",
        ));
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Opening,
    Ready,
    Busy,
    Blocked,
    HumanControl,
    Suspended,
    Recovering,
    Lost,
    Closing,
    Closed,
}
impl SessionState {
    pub fn transition(&mut self, next: Self) -> Result<()> {
        use SessionState::*;
        if *self == next {
            return Ok(());
        }
        let allowed = matches!(
            (&*self, &next),
            (Opening, Ready | Blocked | Lost | Recovering | Closing)
                | (
                    Ready,
                    Busy | Blocked | Suspended | Closing | Lost | Recovering
                )
                | (Busy, Ready | Blocked | Lost | Recovering | Closing)
                | (
                    Blocked,
                    HumanControl | Ready | Closing | Suspended | Recovering | Lost
                )
                | (HumanControl, Ready | Blocked | Lost | Recovering | Closing)
                | (Suspended, Opening | Closing | Lost)
                | (Recovering, Ready | Lost | Blocked | Closing)
                | (Closing, Closed)
        );
        if !allowed {
            return Err(Fault::new(
                "invalid_transition",
                format!("{self:?} -> {next:?}"),
            ));
        }
        *self = next;
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    #[serde(rename = "session_id", alias = "id")]
    pub id: String,
    pub owner_run: String,
    pub profile: String,
    pub context_id: Option<String>,
    pub instance_id: String,
    pub mode: String,
    pub site: Option<String>,
    pub listen: bool,
    pub state: SessionState,
    #[serde(default)]
    pub blocked_reason: Option<String>,
    pub origins: Vec<String>,
    pub credentials: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
impl Session {
    fn set_state(&mut self, next: SessionState, reason: Option<&str>) -> Result<()> {
        self.state.transition(next)?;
        self.blocked_reason = if self.state == SessionState::Blocked {
            reason.map(str::to_owned)
        } else {
            None
        };
        Ok(())
    }
}
#[derive(Clone, Default)]
struct PendingActivity {
    session_id: String,
    profile: String,
    site: Option<String>,
    title_changed: bool,
    unread_changed: bool,
    count: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tab {
    #[serde(rename = "tab_id", alias = "id")]
    pub id: String,
    pub session_id: String,
    pub target_id: Option<String>,
    pub cdp_session: Option<String>,
    pub owner_run: String,
    pub label: Option<String>,
    pub url: String,
    pub title: String,
    pub state: String,
    pub document_epoch: u64,
    pub listen: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Open {
    pub profile: String,
    pub mode: String,
    pub site: Option<String>,
    pub credentials: Vec<String>,
    pub label: Option<String>,
}
impl Default for Open {
    fn default() -> Self {
        Self {
            profile: "ephemeral".into(),
            mode: "headless".into(),
            site: None,
            credentials: vec![],
            label: None,
        }
    }
}
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProbeKey {
    tab: String,
    target: String,
    session: String,
    epoch: u64,
}
impl ProbeKey {
    fn of(tab: &Tab) -> Option<Self> {
        Some(Self {
            tab: tab.id.clone(),
            target: tab.target_id.clone()?,
            session: tab.cdp_session.clone()?,
            epoch: tab.document_epoch,
        })
    }
    fn matches(&self, m: &Model) -> bool {
        m.tabs.get(&self.tab).and_then(Self::of).as_ref() == Some(self)
    }
}
struct ProbeResult {
    key: ProbeKey,
    url: Option<String>,
    count: Option<i64>,
    crashed: bool,
}
#[derive(Default)]
struct Model {
    sessions: BTreeMap<String, Session>,
    tabs: BTreeMap<String, Tab>,
    instances: HashMap<String, Arc<Instance>>,
    active: HashMap<String, String>,
    retries: HashMap<String, VecDeque<tokio::time::Instant>>,
    pending_retry: HashMap<String, tokio::time::Instant>,
    idle_since: HashMap<String, tokio::time::Instant>,
    failed: std::collections::HashSet<String>,
    last_activity: HashMap<String, tokio::time::Instant>,
    pending_activity: HashMap<String, PendingActivity>,
    tab_retry: HashMap<String, tokio::time::Instant>,
    titles: HashMap<String, String>,
    counts: HashMap<String, i64>,
    streams: HashMap<String, tokio::sync::broadcast::Receiver<excalibur_cdp::Event>>,
    busy: HashMap<String, usize>,
    lifecycle: HashMap<String, tokio::sync::broadcast::Receiver<excalibur_cdp::Event>>,
    health: HashMap<String, tokio::time::Instant>,
    unread_health: HashMap<String, tokio::time::Instant>,
    frame_sources: HashMap<String, ProbeKey>,
    probes: tokio::task::JoinSet<ProbeResult>,
    pending_probes: HashMap<String, (ProbeKey, tokio::task::Id)>,
    modes: HashMap<String, String>,
}
/// SQLite is accessed only on Tokio's blocking pool, with a single writer mutex.
pub struct Database {
    connection: Arc<std::sync::Mutex<Connection>>,
}
impl Database {
    pub(crate) async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let c = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            unsafe {
                libc::setpriority(libc::PRIO_PROCESS, 0, 10);
            };
            {
                let mut connection = c
                    .lock()
                    .map_err(|_| Fault::new("storage_error", "writer poisoned"))?;
                f(&mut connection)
            }
        })
        .await
        .map_err(|e| Fault::new("storage_error", e))?
    }
    pub async fn open(path: PathBuf) -> Result<Self> {
        let c = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            let m = file.metadata()?;
            if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
                return Err(Fault::new("denied", "database must be private"));
            }
            let c = Connection::open(path)?;
            c.busy_timeout(Duration::from_secs(5))?;
            let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
            if version > 1 {
                return Err(Fault::new(
                    "storage_error",
                    "unsupported webd database version",
                ));
            }
            c.execute_batch(include_str!("sessions.sql"))?;
            Ok(c)
        })
        .await
        .map_err(|e| Fault::new("storage_error", e))??;
        Ok(Self {
            connection: Arc::new(std::sync::Mutex::new(c)),
        })
    }
    async fn save(&self, m: &mut Model) -> Result<()> {
        for session in m.sessions.values_mut() {
            session.updated_at = now();
        }
        let sessions = m.sessions.values().cloned().collect::<Vec<_>>();
        let tabs = m.tabs.values().cloned().collect::<Vec<_>>();
        self.call(move|c|{let tx=c.transaction()?; for s in sessions {tx.execute("INSERT OR REPLACE INTO web_sessions VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",params![s.id,s.owner_run,s.profile,s.context_id,s.mode,s.site,s.listen,serde_json::to_string(&s.state).unwrap().trim_matches('"'),json!(s.origins).to_string(),json!(s.credentials).to_string(),s.created_at,now(),s.instance_id,json!(s).to_string()])?;} for t in tabs {tx.execute("INSERT OR REPLACE INTO web_tabs VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![t.id,t.session_id,t.target_id,t.owner_run,t.label,t.url,t.title,t.state,t.document_epoch,now(),json!(t).to_string()])?;}tx.commit()?;Ok(())}).await
    }
    pub async fn event(&self, kind: &str, data: Value) -> Result<u64> {
        let kind = kind.to_owned();
        self.call(move |c| {
            c.execute(
                "INSERT INTO web_events(kind,data,at) VALUES(?1,?2,?3)",
                params![kind, data.to_string(), now()],
            )?;
            Ok(c.last_insert_rowid() as u64)
        })
        .await
    }
    pub async fn events(&self, after: u64) -> Result<Vec<Value>> {
        self.call(move|c|{let mut stmt=c.prepare("SELECT cursor,kind,data,at FROM web_events WHERE cursor>?1 ORDER BY cursor LIMIT 256")?;let rows=stmt.query_map([after],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)))?;let mut out=vec![];for row in rows{let (cursor,kind,data,at)=row?;let mut v:Value=serde_json::from_str(&data).map_err(|e|Fault::new("storage_error",e))?;v["cursor"]=json!(cursor);v["type"]=json!(kind);v["at"]=json!(at);out.push(v)}Ok(out)}).await
    }
}
#[derive(Debug)]
pub struct TabLease {
    guard: Option<OwnedMutexGuard<()>>,
    manager: Weak<Manager>,
    session: String,
}
impl Drop for TabLease {
    fn drop(&mut self) {
        drop(self.guard.take());
        if let (Some(manager), Ok(runtime)) = (
            self.manager.upgrade(),
            tokio::runtime::Handle::try_current(),
        ) {
            let session = self.session.clone();
            runtime.spawn(async move {
                let mut m = manager.model.lock().await;
                if let Some(count) = m.busy.get_mut(&session) {
                    *count = count.saturating_sub(1);
                    if *count == 0 && m.sessions[&session].state == SessionState::Busy {
                        if m.sessions
                            .get_mut(&session)
                            .unwrap()
                            .set_state(SessionState::Ready, None)
                            .is_err()
                        {
                            return;
                        }
                        let _ = manager.db.save(&mut m).await;
                        let _ = manager
                            .db
                            .event(
                                "session_state",
                                json!({"session_id":session,"state":"ready"}),
                            )
                            .await;
                    }
                }
            });
        }
    }
}
pub struct Manager {
    pub observation: crate::observation::Observation,
    weak: Weak<Manager>,
    secret_service: bool,
    pub db: Database,
    options: Options,
    model: Mutex<Model>,
    leases: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
    _lock: fs::File,
}
impl Manager {
    pub async fn open(options: Options) -> Result<Arc<Self>> {
        Self::open_with_redactor(options, excalibur_vault::Redactor::default()).await
    }
    pub async fn open_with_redactor(
        options: Options,
        redactor: excalibur_vault::Redactor,
    ) -> Result<Arc<Self>> {
        options.validate()?;
        private_dir(&options.data_dir)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(options.data_dir.join("webd.lock"))?;
        lock.try_lock_exclusive()
            .map_err(|_| Fault::new("already_running", "data directory has a live manager"))?;
        private_dir(&options.data_dir.join("profiles"))?;
        let ephemeral = options.data_dir.join("profiles/ephemeral");
        if ephemeral.exists() {
            remove_stale_lock(&ephemeral)?;
            private_dir(&ephemeral)?;
            fs::remove_dir_all(&ephemeral)?;
        }
        let db = Database::open(options.data_dir.join("webd.db")).await?;
        let profiles = options.profiles.clone();
        let root = options.data_dir.clone();
        db.call(move|c|{let tx=c.transaction()?;for (name,p) in profiles {tx.execute("INSERT INTO web_profiles(name,class,dir,config,updated_at) VALUES(?1,'persistent',?2,?3,?4) ON CONFLICT(name) DO UPDATE SET config=excluded.config,updated_at=excluded.updated_at",params![name,root.join("profiles").join(&name).to_string_lossy(),json!(p).to_string(),now()])?;for site in p.sites {tx.execute("INSERT OR IGNORE INTO web_site_state VALUES(?1,?2,'unknown',?3)",params![name,site.name,now()])?;}}tx.execute("INSERT OR IGNORE INTO web_profiles(name,class,dir,config,updated_at) VALUES('ephemeral','ephemeral','','{}',?1)",[now()])?;tx.execute("DELETE FROM web_ops WHERE updated_at < ?1 AND state IN ('completed','failed')",[now()-86400])?;tx.execute("DELETE FROM web_events WHERE at < ?1",[now()-86400])?;tx.commit()?;Ok(())}).await?;
        let (sessions, tabs) = db
            .call(|c| {
                fn rows(c: &Connection, table: &str) -> Result<Vec<String>> {
                    let mut s = c.prepare(&format!("SELECT record FROM {table}"))?;
                    Ok(s.query_map([], |r| r.get(0))?
                        .collect::<std::result::Result<Vec<String>, _>>()?)
                }
                Ok((rows(c, "web_sessions")?, rows(c, "web_tabs")?))
            })
            .await?;
        let mut model = Model::default();
        for row in sessions {
            let mut s: Session =
                serde_json::from_str(&row).map_err(|e| Fault::new("storage_error", e))?;
            if s.state == SessionState::Closing {
                s.set_state(SessionState::Closed, None)?;
            }
            if !matches!(s.state, SessionState::Closed | SessionState::Lost) {
                s.context_id = None;
                if s.profile == "ephemeral" {
                    s.set_state(SessionState::Lost, None)?;
                } else if s.state != SessionState::Suspended {
                    s.set_state(SessionState::Recovering, None)?;
                }
            }
            model.sessions.insert(s.id.clone(), s);
        }
        for row in tabs {
            let mut t: Tab =
                serde_json::from_str(&row).map_err(|e| Fault::new("storage_error", e))?;
            t.target_id = None;
            t.cdp_session = None;
            t.document_epoch += 1;
            if let Some(session) = model.sessions.get(&t.session_id) {
                match session.state {
                    SessionState::Closed => t.state = "closed".into(),
                    SessionState::Lost => t.state = "crashed".into(),
                    _ => {}
                }
            }
            model.tabs.insert(t.id.clone(), t);
        }
        let secret_service =
            tokio::task::spawn_blocking(crate::installation::secret_service_available)
                .await
                .unwrap_or(false);
        let manager = Arc::new_cyclic(|weak| Self {
            observation: crate::observation::Observation::new(options.data_dir.clone(), redactor),
            secret_service,
            weak: weak.clone(),
            db,
            options,
            model: Mutex::new(model),
            leases: Default::default(),
            _lock: lock,
        });
        {
            let mut m = manager.model.lock().await;
            let ids = m
                .sessions
                .values()
                .filter(|s| s.state == SessionState::Recovering)
                .map(|s| s.id.clone())
                .collect::<Vec<_>>();
            for sid in ids {
                if let Err(e) = manager.restore(&mut m, &sid).await {
                    let terminal = matches!(
                        e.kind.as_str(),
                        "version_blocked"
                            | "sandbox_unavailable"
                            | "unknown_profile"
                            | "unknown_site"
                            | "denied"
                    );
                    let key = m.sessions[&sid].instance_id.clone();
                    m.sessions.get_mut(&sid).unwrap().set_state(
                        if terminal {
                            SessionState::Blocked
                        } else {
                            SessionState::Recovering
                        },
                        Some(&e.kind),
                    )?;
                    if !terminal {
                        m.pending_retry
                            .insert(key, tokio::time::Instant::now() + Duration::from_secs(1));
                    }
                    manager
                        .db
                        .event(
                            "session_state",
                            json!({"session_id":sid,"state":m.sessions[&sid].state,"error":e}),
                        )
                        .await?;
                }
            }
            manager.db.save(&mut m).await?;
        }
        Ok(manager)
    }
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_millis(200));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                let Some(manager) = weak.upgrade() else { break };
                if let Err(e) = manager.tick().await {
                    eprintln!("browser lifecycle: {}", e.kind)
                }
            }
        })
    }
    fn key(&self, profile: &str, site: Option<&str>) -> Result<String> {
        if profile == "ephemeral" {
            if self.options.compact {
                return self
                    .options
                    .profiles
                    .iter()
                    .find(|(_, p)| p.isolation == "shared_instance")
                    .map(|(n, _)| n.clone())
                    .ok_or_else(|| Fault::new("invalid_config", "compact profile absent"));
            }
            return Ok("ephemeral".into());
        }
        let p = self
            .options
            .profiles
            .get(profile)
            .ok_or_else(|| Fault::new("unknown_profile", profile))?;
        if p.isolation == "per_site" {
            Ok(format!(
                "{}--{}",
                profile,
                site.ok_or_else(|| Fault::new("invalid_params", "site required"))?
            ))
        } else {
            Ok(profile.into())
        }
    }
    fn site(&self, profile: &str, site: Option<&str>) -> Result<Option<Site>> {
        if profile == "ephemeral" {
            if site.is_some() {
                return Err(Fault::new(
                    "invalid_params",
                    "ephemeral site is not configured",
                ));
            }
            return Ok(None);
        }
        let p = self
            .options
            .profiles
            .get(profile)
            .ok_or_else(|| Fault::new("unknown_profile", profile))?;
        Ok(Some(
            p.sites
                .iter()
                .find(|s| Some(s.name.as_str()) == site)
                .cloned()
                .ok_or_else(|| Fault::new("unknown_site", "configured site required"))?,
        ))
    }
    async fn instance(&self, m: &mut Model, key: &str, mode: &str) -> Result<Arc<Instance>> {
        if let Some(i) = m.instances.get(key) {
            if m.modes.get(key).is_some_and(|current| current != mode) {
                return Err(Fault::new(
                    "mode_unavailable",
                    "instance already running in another mode",
                ));
            }
            return Ok(i.clone());
        }
        if m.failed.contains(key) {
            return Err(Fault::new(
                "instance_failed",
                "restart limit; host must restart explicitly",
            ));
        }
        let persistent = key != "ephemeral";
        let profile = key.split("--").next().unwrap();
        let config = self.options.profiles.get(profile);
        let listening = config.is_some_and(|p| p.sites.iter().any(|s| s.listen));
        let dir = self.options.data_dir.join("profiles").join(key);
        private_dir(&dir)?;
        if persistent {
            remove_stale_lock(&dir)?;
            let default = dir.join("Default");
            private_dir(&default)?;
            let pref = default.join("Preferences");
            let mut value = if pref.exists() {
                serde_json::from_slice::<Value>(&fs::read(&pref)?)
                    .map_err(|e| Fault::new("profile_error", e))?
            } else {
                json!({})
            };
            value["session"]["restore_on_startup"] = json!(1);
            let tmp = default.join("Preferences.webd.tmp");
            let mut file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&tmp)?;
            use std::io::Write;
            file.write_all(value.to_string().as_bytes())?;
            file.sync_all()?;
            fs::rename(tmp, pref)?;
        }
        let instance = Arc::new(
            Instance::launch(LaunchOptions {
                binary: self.options.binary.clone(),
                profile: dir,
                proxy: self.options.proxy,
                class: if persistent {
                    Class::Persistent
                } else {
                    Class::Ephemeral
                },
                headless: mode != "headed",
                listening,
                secret_service: self.secret_service,
                extra_args: self.options.chrome_args.clone(),
                version_policy: self.options.version.clone(),
            })
            .await?,
        );
        instance
            .client()
            .send(
                Method::TargetSetDiscoverTargets,
                json!({"discover":true}),
                None,
            )
            .await?;
        if persistent {
            let version = instance.version.product.clone();
            let name = profile.to_owned();
            self.db
                .call(move |c| {
                    c.execute(
                        "UPDATE web_profiles SET chrome_version=?1,updated_at=?2 WHERE name=?3",
                        params![version, now(), name],
                    )?;
                    Ok(())
                })
                .await?;
        }
        let targets = instance
            .client()
            .send(Method::TargetGetTargets, json!({}), None)
            .await?;
        if let Some(infos) = targets["targetInfos"].as_array() {
            for info in infos {
                if info["type"] == "page" {
                    instance
                        .client()
                        .send(
                            Method::TargetCloseTarget,
                            json!({"targetId":info["targetId"]}),
                            None,
                        )
                        .await?;
                }
            }
        }
        m.streams
            .insert(key.to_owned(), instance.client().subscribe_all());
        m.lifecycle
            .insert(key.to_owned(), instance.client().subscribe_lifecycle());
        m.instances.insert(key.to_owned(), instance.clone());
        m.modes.insert(key.into(), mode.into());
        m.idle_since.remove(key);
        Ok(instance)
    }
    pub async fn open_session(&self, run: &str, request: Open) -> Result<Value> {
        if request.credentials.len() > 32 || request.label.as_ref().is_some_and(|s| s.len() > 256) {
            return Err(Fault::new("invalid_params", "credential or label limit"));
        }
        if run.is_empty()
            || run.len() > 256
            || request
                .credentials
                .iter()
                .any(|r| excalibur_vault::SecretRef::parse(r.clone()).is_err() || r.len() > 512)
        {
            return Err(Fault::new(
                "invalid_params",
                "run and credential references required",
            ));
        }
        if !matches!(request.mode.as_str(), "headless" | "headed") {
            return Err(Fault::new("mode_unavailable", "unknown mode"));
        }
        if request.mode == "headed"
            && std::env::var_os("DISPLAY").is_none()
            && std::env::var_os("WAYLAND_DISPLAY").is_none()
        {
            return Err(Fault::new(
                "mode_unavailable",
                "no display; intervention interface is not available",
            ));
        }
        let site = self.site(&request.profile, request.site.as_deref())?;
        let key = self.key(&request.profile, request.site.as_deref())?;
        let mut m = self.model.lock().await;
        let existing = m
            .sessions
            .values()
            .find(|s| {
                s.owner_run == run
                    && s.profile == request.profile
                    && s.site == request.site
                    && !matches!(s.state, SessionState::Closed | SessionState::Lost)
            })
            .map(|s| s.id.clone());
        if let Some(sid) = existing {
            if m.sessions[&sid].mode != request.mode {
                return Err(Fault::new(
                    "mode_unavailable",
                    "existing session is in another mode",
                ));
            }
            if m.sessions[&sid].state == SessionState::Suspended {
                self.restore(&mut m, &sid).await?;
                self.db.save(&mut m).await?;
            }
            let mut value = self.session_value(&m, &sid, true);
            value["sites"] = self.site_states(&m.sessions[&sid].profile).await?;
            return Ok(value);
        }
        let context = if request.profile == "ephemeral" {
            let owners = m
                .sessions
                .values()
                .filter(|s| {
                    s.profile == "ephemeral"
                        && s.context_id.is_some()
                        && !matches!(s.state, SessionState::Closed | SessionState::Lost)
                })
                .count();
            if owners >= 3 {
                return Err(Fault::new("capacity_exceeded", "3 ephemeral contexts"));
            }
            let i = self.instance(&mut m, &key, &request.mode).await?;
            Some(
                i.client()
                    .send(
                        Method::TargetCreateBrowserContext,
                        json!({"disposeOnDetach":true}),
                        None,
                    )
                    .await?["browserContextId"]
                    .as_str()
                    .ok_or_else(|| Fault::new("cdp_error", "missing context"))?
                    .into(),
            )
        } else {
            self.instance(&mut m, &key, &request.mode).await?;
            None
        };
        let mut download_policy = json!({"behavior":"deny","eventsEnabled":true});
        if let Some(c) = &context {
            download_policy["browserContextId"] = json!(c)
        }
        m.instances[&key]
            .client()
            .send(Method::BrowserSetDownloadBehavior, download_policy, None)
            .await?;
        let sid = id();
        let s = Session {
            id: sid.clone(),
            owner_run: run.into(),
            profile: request.profile,
            context_id: context,
            instance_id: key,
            mode: request.mode,
            site: request.site,
            listen: site.as_ref().is_some_and(|s| s.listen),
            state: SessionState::Opening,
            blocked_reason: None,
            origins: site.as_ref().map(|s| s.rw.clone()).unwrap_or_default(),
            credentials: request.credentials,
            created_at: now(),
            updated_at: now(),
        };
        m.sessions.insert(sid.clone(), s);
        self.db.save(&mut m).await?;
        let shared = if site.as_ref().is_some_and(|s| s.listen) {
            m.tabs
                .values()
                .find(|t| {
                    t.listen
                        && m.sessions.get(&t.session_id).is_some_and(|s| {
                            s.profile == m.sessions[&sid].profile && s.site == m.sessions[&sid].site
                        })
                        && t.state != "closed"
                })
                .map(|t| t.id.clone())
        } else {
            None
        };
        let opened = if let Some(tab) = shared {
            Ok(tab)
        } else {
            let url = site
                .as_ref()
                .and_then(|s| s.rw.first())
                .cloned()
                .unwrap_or_else(|| "about:blank".into());
            self.new_tab_locked(&mut m, &sid, &url, request.label).await
        };
        match opened {
            Ok(tab) => {
                m.active.insert(run.into(), tab);
                m.sessions
                    .get_mut(&sid)
                    .unwrap()
                    .set_state(SessionState::Ready, None)?;
                self.db.save(&mut m).await?;
                self.db
                    .event("session_state", json!({"session_id":sid,"state":"ready"}))
                    .await?;
                let mut value = self.session_value(&m, &sid, false);
                value["sites"] = self.site_states(&m.sessions[&sid].profile).await?;
                Ok(value)
            }
            Err(e) => {
                let s = m.sessions.get_mut(&sid).unwrap();
                s.set_state(SessionState::Lost, None)?;
                if let Some(context) = s.context_id.take() {
                    let key = s.instance_id.clone();
                    let _ = m.instances[&key]
                        .client()
                        .send(
                            Method::TargetDisposeBrowserContext,
                            json!({"browserContextId":context}),
                            None,
                        )
                        .await;
                }
                self.db.save(&mut m).await?;
                Err(e)
            }
        }
    }
    fn visible(m: &Model, s: &Session, t: &Tab) -> bool {
        t.session_id == s.id
            || (s.profile != "ephemeral"
                && t.listen
                && m.sessions
                    .get(&t.session_id)
                    .is_some_and(|owner| owner.profile == s.profile && owner.site == s.site))
    }
    fn session_value(&self, m: &Model, sid: &str, resumed: bool) -> Value {
        let s = &m.sessions[sid];
        json!({"session_id":sid,"session":s,"profile":s.profile,"instance_id":s.instance_id,"state":s.state,"mode":s.mode,"resumed":resumed,"tabs":m.tabs.values().filter(|t|Self::visible(m,s,t)&&t.state!="closed").collect::<Vec<_>>()})
    }
    fn owned<'a>(m: &'a Model, run: &str, sid: &str) -> Result<&'a Session> {
        let s = m
            .sessions
            .get(sid)
            .ok_or_else(|| Fault::new("unknown_session", sid))?;
        if s.owner_run != run {
            return Err(Fault::new("denied", "session_owner"));
        }
        match s.state {
            SessionState::Closed | SessionState::Closing => Err(Fault::new("session_closed", sid)),
            SessionState::Lost => Err(Fault::new("session_lost", sid)),
            SessionState::Recovering => Err(Fault::new("session_recovering", sid)),
            _ => Ok(s),
        }
    }
    async fn new_tab_locked(
        &self,
        m: &mut Model,
        sid: &str,
        url: &str,
        label: Option<String>,
    ) -> Result<String> {
        if url.len() > 8192 || label.as_ref().is_some_and(|s| s.len() > 256) {
            return Err(Fault::new("invalid_params", "URL or label limit"));
        }
        let s = m.sessions[sid].clone();
        let total = m
            .tabs
            .values()
            .filter(|t| t.target_id.is_some() && t.state != "closed")
            .count();
        let count = m
            .tabs
            .values()
            .filter(|t| t.owner_run == s.owner_run && t.target_id.is_some() && t.state != "closed")
            .count();
        if total >= 12 || count >= 4 {
            return Err(Fault::new(
                "tab_limit",
                if count >= 4 { "4 per run" } else { "12 global" },
            ));
        }
        self.document_allowed(&s, url, false)?;
        let instance = m
            .instances
            .get(&s.instance_id)
            .ok_or_else(|| Fault::new("session_recovering", sid))?
            .clone();
        let mut params = json!({"url":"about:blank","newWindow":true,"background":false});
        if let Some(c) = &s.context_id {
            params["browserContextId"] = json!(c)
        }
        let created = instance
            .client()
            .send(Method::TargetCreateTarget, params, None)
            .await?;
        let target = created["targetId"]
            .as_str()
            .ok_or_else(|| Fault::new("cdp_error", "no target"))?
            .to_owned();
        let tid = id();
        let setup = async {
            let cdp = instance.attach_page(&target).await?;
            instance
                .client()
                .send(Method::PageEnable, json!({}), Some(&cdp))
                .await?;
            if s.listen {
                instance
                    .client()
                    .send(
                        Method::EmulationSetFocusEmulationEnabled,
                        json!({"enabled":true}),
                        Some(&cdp),
                    )
                    .await?;
                self.install_probe(&instance, &cdp, &s).await?;
            }
            let tab = Tab {
                id: tid.clone(),
                session_id: sid.into(),
                target_id: Some(target.clone()),
                cdp_session: Some(cdp.clone()),
                owner_run: s.owner_run.clone(),
                label: label.clone(),
                url: "about:blank".into(),
                title: String::new(),
                state: "loading".into(),
                document_epoch: 1,
                listen: s.listen,
            };
            self.observation
                .prepare_tab(&s, &tab, instance.client().clone())
                .await?;
            if url != "about:blank" {
                instance
                    .client()
                    .send(Method::PageNavigate, json!({"url":url}), Some(&cdp))
                    .await?;
            }
            Ok::<_, Fault>(cdp)
        }
        .await;
        let cdp = match setup {
            Ok(c) => c,
            Err(e) => {
                let _ = instance
                    .client()
                    .send(Method::TargetCloseTarget, json!({"targetId":target}), None)
                    .await;
                return Err(e);
            }
        };
        let t = Tab {
            id: tid.clone(),
            session_id: sid.into(),
            target_id: Some(target),
            cdp_session: Some(cdp),
            owner_run: s.owner_run,
            label,
            url: url.into(),
            title: String::new(),
            state: if url == "about:blank" {
                "ready"
            } else {
                "loading"
            }
            .into(),
            document_epoch: 1,
            listen: s.listen,
        };
        m.tabs.insert(tid.clone(), t);
        self.db.save(m).await?;
        self.db
            .event(
                "tab_state",
                json!({"tab_id":tid,"state":"loading","document_epoch":1}),
            )
            .await?;
        Ok(tid)
    }
    pub fn document_allowed(&self, s: &Session, url: &str, input: bool) -> Result<()> {
        if url == "about:blank" && !input {
            return Ok(());
        }
        let o = origin(url).ok_or_else(|| Fault::new("denied", "unsupported_scheme"))?;
        if s.profile == "ephemeral" {
            return Ok(());
        }
        let site = self.site(&s.profile, s.site.as_deref())?.unwrap();
        if site.login.contains(&o) {
            return Err(Fault::new("denied", "human_only_origin"));
        }
        if !site.rw.contains(&o) {
            return Err(Fault::new("denied", "document_origin"));
        }
        Ok(())
    }
    async fn install_probe(&self, i: &Instance, cdp: &str, s: &Session) -> Result<()> {
        let site = self.site(&s.profile, s.site.as_deref())?.unwrap();
        if site.listen_probe.unread_selector.is_empty() {
            return Ok(());
        }
        let selector = json!(site.listen_probe.unread_selector).to_string();
        let origins = json!(site.rw).to_string();
        let source = format!(
            "(()=>{{ if(!{origins}.includes(location.origin))return; let last=-1;const poll=()=>{{try{{const n=document.querySelectorAll({selector}).length;if(n!==last){{last=n;globalThis.__excalibur_unread(String(n))}}}}catch{{}}}};new MutationObserver(poll).observe(document,{{subtree:true,childList:true,attributes:true}});poll();}})()"
        );
        i.client()
            .send(Method::RuntimeEnable, json!({}), Some(cdp))
            .await?;
        i.client()
            .send(
                Method::RuntimeAddBinding,
                json!({"name":"__excalibur_unread","executionContextName":"excalibur-listener"}),
                Some(cdp),
            )
            .await?;
        i.client()
            .send(
                Method::PageAddScriptToEvaluateOnNewDocument,
                json!({"source":source,"worldName":"excalibur-listener"}),
                Some(cdp),
            )
            .await?;
        Ok(())
    }
    pub async fn list(&self, run: &str) -> Value {
        let m = self.model.lock().await;
        json!(
            m.sessions
                .values()
                .filter(|s| s.owner_run == run)
                .collect::<Vec<_>>()
        )
    }
    pub async fn tabs(&self, run: &str, sid: &str) -> Result<Value> {
        self.await_available(run, sid).await?;
        let mut m = self.model.lock().await;
        Self::owned(&m, run, sid)?;
        if m.sessions[sid].state == SessionState::Suspended {
            self.restore(&mut m, sid).await?;
            self.db.save(&mut m).await?;
        }
        let s = &m.sessions[sid];
        Ok(json!(
            m.tabs
                .values()
                .filter(|t| Self::visible(&m, s, t) && t.state != "closed")
                .map(|t| {
                    let mut v = json!(t);
                    v["active"] = json!(m.active.get(run) == Some(&t.id));
                    v
                })
                .collect::<Vec<_>>()
        ))
    }
    pub async fn new_tab(
        &self,
        run: &str,
        sid: &str,
        url: &str,
        label: Option<String>,
    ) -> Result<Value> {
        self.await_available(run, sid).await?;
        let mut m = self.model.lock().await;
        Self::owned(&m, run, sid)?;
        if m.sessions[sid].listen {
            return Err(Fault::new("denied", "listener has one shared tab"));
        }
        if m.sessions[sid].state == SessionState::Suspended {
            self.restore(&mut m, sid).await?;
        }
        let tid = self.new_tab_locked(&mut m, sid, url, label).await?;
        m.active.insert(run.into(), tid.clone());
        Ok(json!(m.tabs[&tid]))
    }
    async fn await_available(&self, run: &str, sid: &str) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let state = {
                let m = self.model.lock().await;
                let s = m
                    .sessions
                    .get(sid)
                    .ok_or_else(|| Fault::new("unknown_session", sid))?;
                if s.owner_run != run {
                    return Err(Fault::new("denied", "session_owner"));
                }
                s.state.clone()
            };
            if !matches!(state, SessionState::Opening | SessionState::Recovering) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Fault::new(
                    "session_recovering",
                    "30 second recovery deadline",
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    async fn accessible(&self, run: &str, sid: &str, tid: &str) -> Result<()> {
        let m = self.model.lock().await;
        let s = Self::owned(&m, run, sid)?;
        let t = m
            .tabs
            .get(tid)
            .ok_or_else(|| Fault::new("unknown_tab", tid))?;
        if !Self::visible(&m, s, t) {
            return Err(Fault::new("denied", "tab_owner"));
        }
        if t.state == "crashed" {
            return Err(Fault::new("tab_crashed", tid));
        }
        if t.state == "closed" {
            return Err(Fault::new("tab_closed", tid));
        }
        Ok(())
    }
    /// The guard is the exclusive lease; future observation/action code must hold it through CDP dispatch.
    pub async fn lease(&self, run: &str, sid: &str, tid: &str, input: bool) -> Result<TabLease> {
        {
            let mut m = self.model.lock().await;
            let state = Self::owned(&m, run, sid)?.state.clone();
            if state == SessionState::Suspended {
                self.restore(&mut m, sid).await?;
                self.db.save(&mut m).await?;
            }
        }
        self.accessible(run, sid, tid).await?;
        let lock = self
            .leases
            .lock()
            .unwrap()
            .entry(tid.into())
            .or_default()
            .clone();
        let guard = tokio::time::timeout(Duration::from_secs(30), lock.lock_owned())
            .await
            .map_err(|_| Fault::new("tab_busy", "30 second queue deadline"))?;
        self.accessible(run, sid, tid).await?;
        if input {
            let (key, tree) = self.fresh_frame(run, sid, tid).await?;
            let m = self.model.lock().await;
            if !key.matches(&m) {
                return Err(Fault::new(
                    "session_recovering",
                    "document changed during admission",
                ));
            }
            let current = tree["frameTree"]["frame"]["url"]
                .as_str()
                .ok_or_else(|| Fault::new("denied", "unknown document origin"))?;
            self.document_allowed(&m.sessions[sid], current, true)?;
        }
        {
            let m = self.model.lock().await;
            match m.sessions[sid].state {
                SessionState::Blocked => return Err(Fault::new("blocked", sid)),
                SessionState::HumanControl => return Err(Fault::new("blocked", "human_control")),
                _ => {}
            }
        }
        {
            let mut m = self.model.lock().await;
            *m.busy.entry(sid.into()).or_default() += 1;
            m.sessions
                .get_mut(sid)
                .unwrap()
                .set_state(SessionState::Busy, None)?;
            self.db.save(&mut m).await?;
            self.db
                .event("session_state", json!({"session_id":sid,"state":"busy"}))
                .await?;
        }
        Ok(TabLease {
            guard: Some(guard),
            manager: self.weak.clone(),
            session: sid.into(),
        })
    }
    pub async fn switch(&self, run: &str, sid: &str, tid: &str) -> Result<Value> {
        let _lease = self.lease(run, sid, tid, false).await?;
        let mut m = self.model.lock().await;
        let t = &m.tabs[tid];
        let s = &m.sessions[&t.session_id];
        if let Some(cdp) = &t.cdp_session {
            m.instances[&s.instance_id]
                .client()
                .send(Method::PageBringToFront, json!({}), Some(cdp))
                .await?;
        }
        m.active.insert(run.into(), tid.into());
        Ok(json!({"tab_id":tid,"active":true}))
    }
    pub async fn close_tab(&self, run: &str, sid: &str, tid: &str) -> Result<Value> {
        let _lease = self.lease(run, sid, tid, false).await?;
        let mut m = self.model.lock().await;
        if m.tabs[tid].listen {
            return Err(Fault::new(
                "denied",
                "listener tab requires closing its session",
            ));
        }
        self.stop_tab(&mut m, tid, false).await?;
        self.db.save(&mut m).await?;
        Ok(json!({"tab_id":tid,"state":"closed"}))
    }
    async fn stop_tab(&self, m: &mut Model, tid: &str, suspend: bool) -> Result<()> {
        let t = m.tabs[tid].clone();
        if let Some(target) = t.target_id {
            let s = &m.sessions[&t.session_id];
            if let Some(i) = m.instances.get(&s.instance_id) {
                i.client()
                    .send(Method::TargetCloseTarget, json!({"targetId":target}), None)
                    .await?;
            }
        }
        let t = m.tabs.get_mut(tid).unwrap();
        t.target_id = None;
        t.cdp_session = None;
        t.state = if suspend { "suspended" } else { "closed" }.into();
        t.document_epoch += 1;
        self.db
            .event(
                "tab_state",
                json!({"tab_id":tid,"state":t.state,"document_epoch":t.document_epoch}),
            )
            .await?;
        Ok(())
    }
    pub async fn end_session(&self, run: &str, sid: &str, suspend: bool) -> Result<Value> {
        let tids = {
            let m = self.model.lock().await;
            let existing = m
                .sessions
                .get(sid)
                .ok_or_else(|| Fault::new("unknown_session", sid))?;
            if existing.owner_run != run {
                return Err(Fault::new("denied", "session_owner"));
            }
            if (existing.state == SessionState::Closed && !suspend)
                || (existing.state == SessionState::Suspended && suspend)
            {
                return Ok(json!({"session_id":sid,"state":existing.state}));
            }
            let s = Self::owned(&m, run, sid)?;
            if suspend && s.listen {
                return Err(Fault::new("listener_cannot_suspend", sid));
            }
            m.tabs
                .values()
                .filter(|t| Self::visible(&m, s, t) && t.state != "closed")
                .map(|t| t.id.clone())
                .collect::<Vec<_>>()
        };
        let mut guards = vec![];
        for tid in &tids {
            let lock = self
                .leases
                .lock()
                .unwrap()
                .entry(tid.clone())
                .or_default()
                .clone();
            guards.push(
                tokio::time::timeout(Duration::from_secs(30), lock.lock_owned())
                    .await
                    .map_err(|_| Fault::new("tab_busy", tid))?,
            );
        }
        let mut frames = vec![];
        if suspend {
            for tid in &tids {
                frames.push(self.fresh_frame(run, sid, tid).await?);
            }
        }
        let mut m = self.model.lock().await;
        for (key, frame) in frames {
            if !key.matches(&m) {
                return Err(Fault::new(
                    "session_recovering",
                    "document changed during suspension",
                ));
            }
            if let Some(url) = frame["frameTree"]["frame"]["url"].as_str() {
                m.tabs.get_mut(&key.tab).unwrap().url = url.into();
            }
        }
        let s = Self::owned(&m, run, sid)?.clone();
        self.observation.cleanup_uploads(Some(sid));
        if m.sessions[sid].state == SessionState::Busy {
            m.sessions
                .get_mut(sid)
                .unwrap()
                .set_state(SessionState::Ready, None)?;
        }
        if !suspend {
            m.sessions
                .get_mut(sid)
                .unwrap()
                .set_state(SessionState::Closing, None)?;
            self.db.save(&mut m).await?;
            self.db
                .event("session_state", json!({"session_id":sid,"state":"closing"}))
                .await?;
        }
        for tid in tids {
            if m.tabs[&tid].session_id != sid {
                continue;
            }
            let another = m.sessions.values().any(|o| {
                o.id != sid
                    && o.profile == s.profile
                    && o.site == s.site
                    && !matches!(o.state, SessionState::Closed | SessionState::Lost)
            });
            if s.listen && another {
                let replacement = m
                    .sessions
                    .values()
                    .find(|o| {
                        o.id != sid
                            && o.profile == s.profile
                            && o.site == s.site
                            && !matches!(o.state, SessionState::Closed | SessionState::Lost)
                    })
                    .unwrap()
                    .id
                    .clone();
                let new_owner = m.sessions[&replacement].owner_run.clone();
                let t = m.tabs.get_mut(&tid).unwrap();
                t.session_id = replacement;
                t.owner_run = new_owner;
            } else {
                self.stop_tab(&mut m, &tid, suspend).await?;
            }
        }
        if let Some(context) = &s.context_id
            && let Some(i) = m.instances.get(&s.instance_id)
        {
            i.client()
                .send(
                    Method::TargetDisposeBrowserContext,
                    json!({"browserContextId":context}),
                    None,
                )
                .await?;
        }
        let session = m.sessions.get_mut(sid).unwrap();
        session.context_id = None;
        session.set_state(
            if suspend {
                SessionState::Suspended
            } else {
                SessionState::Closed
            },
            None,
        )?;
        self.db.save(&mut m).await?;
        self.db
            .event(
                "session_state",
                json!({"session_id":sid,"state":m.sessions[sid].state}),
            )
            .await?;
        Ok(json!({"session_id":sid,"state":m.sessions[sid].state}))
    }
    async fn revive_tab(&self, m: &mut Model, tid: &str) -> Result<()> {
        let old = m.tabs[tid].clone();
        let s = m.sessions[&old.session_id].clone();
        let url = if self.document_allowed(&s, &old.url, false).is_ok() {
            old.url.clone()
        } else if let Some(site) = self.site(&s.profile, s.site.as_deref())? {
            site.rw
                .first()
                .cloned()
                .ok_or_else(|| Fault::new("denied", "no recovery origin"))?
        } else {
            "about:blank".into()
        };
        self.document_allowed(&s, &url, false)?;
        let i = m.instances[&s.instance_id].clone();
        let mut args = json!({"url":"about:blank","newWindow":true});
        if let Some(c) = &s.context_id {
            args["browserContextId"] = json!(c)
        }
        let target = i
            .client()
            .send(Method::TargetCreateTarget, args, None)
            .await?["targetId"]
            .as_str()
            .ok_or_else(|| Fault::new("cdp_error", "no target"))?
            .to_owned();
        let setup = async {
            let cdp = i.attach_page(&target).await?;
            i.client()
                .send(Method::PageEnable, json!({}), Some(&cdp))
                .await?;
            if s.listen {
                i.client()
                    .send(
                        Method::EmulationSetFocusEmulationEnabled,
                        json!({"enabled":true}),
                        Some(&cdp),
                    )
                    .await?;
                self.install_probe(&i, &cdp, &s).await?;
            }
            let mut tab = old.clone();
            tab.target_id = Some(target.clone());
            tab.cdp_session = Some(cdp.clone());
            self.observation
                .prepare_tab(&s, &tab, i.client().clone())
                .await?;
            i.client()
                .send(Method::PageNavigate, json!({"url":url}), Some(&cdp))
                .await?;
            if let Some(previous) = &old.target_id
                && let Err(error) = i
                    .client()
                    .send(
                        Method::TargetCloseTarget,
                        json!({"targetId":previous}),
                        None,
                    )
                    .await
            {
                let infos = i
                    .client()
                    .send(Method::TargetGetTargets, json!({}), None)
                    .await?;
                if infos["targetInfos"].as_array().is_some_and(|infos| {
                    infos
                        .iter()
                        .any(|v| v["targetId"].as_str() == Some(previous))
                }) {
                    return Err(Fault::from(error));
                }
            }
            Ok::<_, Fault>(cdp)
        }
        .await;
        let cdp = match setup {
            Ok(cdp) => cdp,
            Err(error) => {
                let _ = i
                    .client()
                    .send(Method::TargetCloseTarget, json!({"targetId":target}), None)
                    .await;
                return Err(error);
            }
        };
        let tab = m.tabs.get_mut(tid).unwrap();
        tab.target_id = Some(target);
        tab.url = url;
        tab.cdp_session = Some(cdp);
        tab.document_epoch += 1;
        tab.state = "loading".into();
        self.db.event("tab_state",json!({"tab_id":tid,"state":"loading","reason":"renderer_restarted","document_epoch":tab.document_epoch})).await?;
        Ok(())
    }
    async fn restore(&self, m: &mut Model, sid: &str) -> Result<()> {
        let restore_tabs = m
            .tabs
            .values()
            .filter(|t| t.session_id == sid && t.state != "closed")
            .count();
        let run = &m.sessions[sid].owner_run;
        let total = m
            .tabs
            .values()
            .filter(|t| t.session_id != sid && t.target_id.is_some() && t.state != "closed")
            .count();
        let owned = m
            .tabs
            .values()
            .filter(|t| {
                t.session_id != sid
                    && &t.owner_run == run
                    && t.target_id.is_some()
                    && t.state != "closed"
            })
            .count();
        if total + restore_tabs > 12 || owned + restore_tabs > 4 {
            return Err(Fault::new("tab_limit", "restoring tabs exceeds capacity"));
        }
        if m.sessions[sid].profile == "ephemeral"
            && m.sessions
                .values()
                .filter(|s| s.context_id.is_some())
                .count()
                >= 3
        {
            return Err(Fault::new("capacity_exceeded", "3 ephemeral contexts"));
        }
        let s = m.sessions[sid].clone();
        if s.profile == "ephemeral" {
            let i = self.instance(m, &s.instance_id, &s.mode).await?;
            let context = i
                .client()
                .send(
                    Method::TargetCreateBrowserContext,
                    json!({"disposeOnDetach":true}),
                    None,
                )
                .await?;
            m.sessions.get_mut(sid).unwrap().context_id =
                context["browserContextId"].as_str().map(str::to_owned);
        } else {
            self.site(&s.profile, s.site.as_deref())?;
            self.instance(m, &s.instance_id, &s.mode).await?;
        }
        let tids = m
            .tabs
            .values()
            .filter(|t| t.session_id == sid && t.state != "closed")
            .map(|t| t.id.clone())
            .collect::<Vec<_>>();
        for tid in tids {
            self.revive_tab(m, &tid).await?;
        }
        if m.sessions[sid].state == SessionState::Suspended {
            m.sessions
                .get_mut(sid)
                .unwrap()
                .set_state(SessionState::Opening, None)?;
        }
        m.sessions
            .get_mut(sid)
            .unwrap()
            .set_state(SessionState::Ready, None)?;
        self.db.event("session_state",json!({"session_id":sid,"state":"ready","reason":"browser_restarted","references":"stale_ref(browser_restarted)"})).await?;
        Ok(())
    }
    pub async fn site_state(&self, run: &str, sid: &str, state: &str) -> Result<Value> {
        if !matches!(state, "unknown" | "logged_in" | "needs_login") {
            return Err(Fault::new("invalid_params", "login state"));
        }
        let m = self.model.lock().await;
        let s = Self::owned(&m, run, sid)?;
        let profile = s.profile.clone();
        let site = s
            .site
            .clone()
            .ok_or_else(|| Fault::new("invalid_params", "persistent site required"))?;
        let state = state.to_owned();
        self.db
            .call(move |c| {
                c.execute(
                    "INSERT OR REPLACE INTO web_site_state VALUES(?1,?2,?3,?4)",
                    params![profile, site, state, now()],
                )?;
                Ok(json!({"site":site,"login_state":state}))
            })
            .await
    }
    pub async fn site_states(&self, profile: &str) -> Result<Value> {
        let profile = profile.to_owned();
        self.db.call(move|c|{let mut stmt=c.prepare("SELECT site,login_state,changed_at FROM web_site_state WHERE profile=?1 ORDER BY site")?;let rows=stmt.query_map([profile],|r|Ok(json!({"name":r.get::<_,String>(0)?,"login_state":r.get::<_,String>(1)?,"changed_at":r.get::<_,i64>(2)?})))?;Ok(json!(rows.collect::<std::result::Result<Vec<_>,_>>()?))}).await
    }
    pub async fn status(&self) -> Value {
        let m = self.model.lock().await;
        json!({"instances":m.instances.iter().map(|(k,i)|json!({"id":k,"pid":i.pid(),"state":*i.status().borrow(),"version":i.version,"warnings":i.warnings})).collect::<Vec<_>>(),"failed_instances":m.failed,"limits":{"tabs_per_run":4,"tabs_total":12,"ephemeral_contexts":3},"password_store":if self.secret_service{"gnome-libsecret"}else{"basic"},"warning":if self.secret_service{None}else{Some("profile cookies can be read by anyone with filesystem access")},"network":"deny_all_until_broker_integration"})
    }
    pub async fn shutdown(&self) -> Result<()> {
        let mut m = self.model.lock().await;
        self.db.save(&mut m).await?;
        for (_, i) in m.instances.drain() {
            i.close().await?;
        }
        Ok(())
    }
    pub async fn diagnostic_client(
        &self,
        run: &str,
        sid: &str,
        tid: &str,
    ) -> Result<(excalibur_cdp::Client, String)> {
        self.accessible(run, sid, tid).await?;
        let m = self.model.lock().await;
        let t = &m.tabs[tid];
        Ok((
            m.instances[&m.sessions[&t.session_id].instance_id]
                .client()
                .clone(),
            t.cdp_session
                .clone()
                .ok_or_else(|| Fault::new("session_recovering", sid))?,
        ))
    }
    pub async fn tick(&self) -> Result<()> {
        self.observation.cleanup_uploads(None);
        let instances = {
            let m = self.model.lock().await;
            m.instances
                .iter()
                .map(|(key, instance)| (key.clone(), instance.clone()))
                .collect::<Vec<_>>()
        };
        let mut discoveries = tokio::task::JoinSet::new();
        for (key, instance) in instances {
            discoveries.spawn(async move {
                let targets = instance
                    .client()
                    .send_with_timeout(Method::TargetGetTargets, json!({}), None, PROBE_TIMEOUT)
                    .await;
                (key, instance, targets)
            });
        }
        let mut snapshots = HashMap::new();
        while let Some(result) = discoveries.join_next().await {
            if let Ok((key, instance, Ok(targets))) = result {
                snapshots.insert(key, (instance, targets));
            }
        }
        let mut m = self.model.lock().await;
        let mut changed = self.collect_probes(&mut m).await?;
        let keys = m.instances.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            let i = m.instances[&key].clone();
            let status = i.status().borrow().clone();
            if matches!(status, Status::Crashed | Status::Blocked | Status::Stopped) {
                m.instances.remove(&key);
                m.streams.remove(&key);
                m.lifecycle.remove(&key);
                let persistent = key != "ephemeral";
                let ids = m
                    .sessions
                    .values()
                    .filter(|s| {
                        s.instance_id == key
                            && !matches!(
                                s.state,
                                SessionState::Closed | SessionState::Lost | SessionState::Suspended
                            )
                    })
                    .map(|s| s.id.clone())
                    .collect::<Vec<_>>();
                for sid in ids {
                    let s = m.sessions.get_mut(&sid).unwrap();
                    s.set_state(
                        if persistent && s.profile != "ephemeral" {
                            SessionState::Recovering
                        } else {
                            SessionState::Lost
                        },
                        None,
                    )?;
                    self.db
                        .event("session_state", json!({"session_id":sid,"state":s.state}))
                        .await?;
                }
                self.db
                    .event(
                        if status == Status::Blocked {
                            "blocked"
                        } else {
                            "instance_crashed"
                        },
                        json!({"instance_id":key}),
                    )
                    .await?;
                if status == Status::Blocked {
                    for s in m
                        .sessions
                        .values_mut()
                        .filter(|s| s.instance_id == key && s.state == SessionState::Recovering)
                    {
                        s.set_state(
                            SessionState::Blocked,
                            Some("instance_security_or_version_blocked"),
                        )?;
                    }
                    self.db.event("blocked",json!({"instance_id":key,"reason":"instance_security_or_version_blocked"})).await?;
                } else {
                    let history = m.retries.entry(key.clone()).or_default();
                    while history
                        .front()
                        .is_some_and(|at| at.elapsed() > Duration::from_secs(600))
                    {
                        history.pop_front();
                    }
                    let delay = retry_delay(history.len());
                    m.pending_retry
                        .insert(key.clone(), tokio::time::Instant::now() + delay);
                }
                changed = true;
                continue;
            }
            let mut events = vec![];
            if let Some(stream) = m.streams.get_mut(&key) {
                for _ in 0..1024 {
                    match stream.try_recv() {
                        Ok(e) => events.push(e),
                        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            }
            if let Some(stream) = m.lifecycle.get_mut(&key) {
                for _ in 0..256 {
                    match stream.try_recv() {
                        Ok(event) => events.push(event),
                        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            }
            for event in events {
                let tid = m
                    .tabs
                    .values()
                    .find(|t| {
                        t.target_id.as_deref() == event.params["targetId"].as_str()
                            && event.params["targetId"].is_string()
                            || t.cdp_session.as_deref() == event.session_id.as_deref()
                                && event.session_id.is_some()
                    })
                    .map(|t| t.id.clone());
                let Some(tid) = tid else { continue };
                let t = m.tabs[&tid].clone();
                if matches!(
                    event.method.as_str(),
                    "Target.targetCrashed" | "Inspector.targetCrashed"
                ) {
                    m.tabs.get_mut(&tid).unwrap().state = "crashed".into();
                    self.db
                        .event("tab_state", json!({"tab_id":tid,"state":"crashed"}))
                        .await?;
                    if t.listen {
                        self.revive_listener(&mut m, &tid).await?;
                    }
                    changed = true;
                } else if event.method == "Page.frameNavigated"
                    && event.params["frame"]["parentId"].is_null()
                {
                    let tab = m.tabs.get_mut(&tid).unwrap();
                    tab.document_epoch += 1;
                    if let Some(url) = event.params["frame"]["url"].as_str() {
                        tab.url = url.into();
                    }
                    tab.state = "ready".into();
                    if let Some(key) = ProbeKey::of(tab) {
                        m.frame_sources.insert(tid.clone(), key);
                    }
                    changed = true;
                } else if t.listen
                    && event.method == "Runtime.bindingCalled"
                    && event.params["name"] == "__excalibur_unread"
                    && let Some(count) = event.params["payload"]
                        .as_str()
                        .and_then(|s| s.parse::<i64>().ok())
                        .filter(|n| *n >= 0)
                {
                    let s = m.sessions[&t.session_id].clone();
                    if m.counts.insert(tid.clone(), count) != Some(count) {
                        let site_key = format!("{}/{}", s.profile, s.site.as_deref().unwrap_or(""));
                        self.activity(&mut m, &site_key, &s, "unread_count", Some(count))
                            .await?;
                    }
                }
            }
            let Some((source, targets)) = snapshots.remove(&key) else {
                continue;
            };
            if !Arc::ptr_eq(&source, &i) {
                continue;
            }
            let infos = targets["targetInfos"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let tids = m
                .tabs
                .values()
                .filter(|t| {
                    t.state != "closed"
                        && m.sessions[&t.session_id].instance_id == key
                        && t.target_id.is_some()
                })
                .map(|t| t.id.clone())
                .collect::<Vec<_>>();
            for tid in tids {
                let t = m.tabs[&tid].clone();
                let info = infos
                    .iter()
                    .find(|v| v["targetId"].as_str() == t.target_id.as_deref());
                if let Some(info) = info {
                    let url = if m.frame_sources.get(&tid).is_some_and(|key| key.matches(&m)) {
                        t.url.clone()
                    } else {
                        info["url"].as_str().unwrap_or(&t.url).to_owned()
                    };
                    let title = info["title"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(2048)
                        .collect::<String>();
                    let s = m.sessions[&t.session_id].clone();
                    if t.url != url {
                        let live = m.tabs.get_mut(&tid).unwrap();
                        live.url = url.clone();
                        live.document_epoch += 1;
                        changed = true;
                    }
                    if title != t.title {
                        m.tabs.get_mut(&tid).unwrap().title = title.clone();
                        changed = true;
                    }
                    if t.listen {
                        let site_key = format!("{}/{}", s.profile, s.site.as_deref().unwrap_or(""));
                        let first = !m.titles.contains_key(&tid);
                        let old = m.titles.insert(tid.clone(), title.clone());
                        if !first && old.as_ref() != Some(&title) {
                            self.activity(&mut m, &site_key, &s, "title_changed", None)
                                .await?;
                        }
                    }
                } else if t.listen {
                    self.revive_listener(&mut m, &tid).await?;
                    changed = true;
                }
            }
            // A filtered event stream is consumed by a separate task below; polling target metadata also reconciles lag.
            let active = m.sessions.values().any(|s| {
                s.instance_id == key
                    && !matches!(
                        s.state,
                        SessionState::Closed | SessionState::Lost | SessionState::Suspended
                    )
            });
            if !active && key == "ephemeral" {
                let since = m
                    .idle_since
                    .entry(key.clone())
                    .or_insert_with(tokio::time::Instant::now);
                if since.elapsed() >= Duration::from_secs(600) {
                    i.close().await?;
                    m.instances.remove(&key);
                    let path = self.options.data_dir.join("profiles").join(&key);
                    tokio::task::spawn_blocking(move || fs::remove_dir_all(path))
                        .await
                        .map_err(|e| Fault::new("io_error", e))??;
                }
            } else {
                m.idle_since.remove(&key);
            }
        }
        let retries = m
            .pending_retry
            .iter()
            .filter(|(_, at)| **at <= tokio::time::Instant::now())
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();
        for key in retries {
            let ids = m
                .sessions
                .values()
                .filter(|s| s.instance_id == key && s.state == SessionState::Recovering)
                .map(|s| s.id.clone())
                .collect::<Vec<_>>();
            if ids.is_empty() {
                m.pending_retry.remove(&key);
                continue;
            }
            let history = m.retries.entry(key.clone()).or_default();
            while history
                .front()
                .is_some_and(|at| at.elapsed() > Duration::from_secs(600))
            {
                history.pop_front();
            }
            if history.len() >= 5 {
                m.failed.insert(key.clone());
                for sid in ids {
                    m.sessions
                        .get_mut(&sid)
                        .unwrap()
                        .set_state(SessionState::Lost, None)?;
                }
                m.pending_retry.remove(&key);
                self.db
                    .event(
                        "instance_failed",
                        json!({"instance_id":key,"reason":"5 relaunches in 10 minutes"}),
                    )
                    .await?;
                changed = true;
                continue;
            }
            history.push_back(tokio::time::Instant::now());
            let attempt = history.len();
            let mut success = true;
            for sid in ids {
                if self.restore(&mut m, &sid).await.is_err() {
                    success = false;
                }
            }
            if success {
                m.pending_retry.remove(&key);
            } else {
                m.pending_retry
                    .insert(key, tokio::time::Instant::now() + retry_delay(attempt));
            }
            changed = true;
        }
        changed |= self.reconcile_documents(&mut m).await?;
        if changed {
            self.db.save(&mut m).await?;
        }
        self.flush_activity(&mut m).await?;
        self.schedule_probes(&mut m)?;
        Ok(())
    }
    fn schedule_probes(&self, m: &mut Model) -> Result<()> {
        let tabs = m
            .tabs
            .values()
            .filter(|t| t.state != "closed")
            .cloned()
            .collect::<Vec<_>>();
        for tab in tabs {
            if m.probes.len() >= 12 {
                break;
            }
            let Some(key) = ProbeKey::of(&tab) else {
                continue;
            };
            if m.pending_probes.contains_key(&tab.id)
                || m.health
                    .get(&tab.id)
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
            {
                continue;
            }
            let session = &m.sessions[&tab.session_id];
            let Some(instance) = m.instances.get(&session.instance_id) else {
                continue;
            };
            let selector = if tab.listen
                && !m
                    .unread_health
                    .get(&tab.id)
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
            {
                m.unread_health
                    .insert(tab.id.clone(), tokio::time::Instant::now());
                self.site(&session.profile, session.site.as_deref())?
                    .map(|site| site.listen_probe.unread_selector)
                    .filter(|s| !s.is_empty())
            } else {
                None
            };
            let client = instance.client().clone();
            let request = key.clone();
            let job = m
                .probes
                .spawn(async move { probe_tab(client, request, selector).await });
            m.pending_probes.insert(tab.id.clone(), (key, job.id()));
            m.health.insert(tab.id, tokio::time::Instant::now());
        }
        Ok(())
    }
    async fn collect_probes(&self, m: &mut Model) -> Result<bool> {
        let mut changed = false;
        while let Some(result) = m.probes.try_join_next() {
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    m.pending_probes.retain(|_, (_, id)| *id != error.id());
                    continue;
                }
            };
            if m.pending_probes
                .get(&result.key.tab)
                .is_some_and(|(key, _)| key == &result.key)
            {
                m.pending_probes.remove(&result.key.tab);
            }
            if !result.key.matches(m) {
                continue;
            }
            let tid = result.key.tab.clone();
            if result.crashed {
                m.tabs.get_mut(&tid).unwrap().state = "crashed".into();
                self.db
                    .event("tab_state", json!({"tab_id":tid,"state":"crashed"}))
                    .await?;
                if m.tabs[&tid].listen {
                    self.revive_listener(m, &tid).await?;
                }
                changed = true;
                continue;
            }
            if let Some(url) = result.url {
                let tab = m.tabs.get_mut(&tid).unwrap();
                if tab.url != url {
                    tab.url = url;
                    tab.document_epoch += 1;
                    changed = true;
                }
                if let Some(key) = ProbeKey::of(tab) {
                    m.frame_sources.insert(tid.clone(), key);
                }
            }
            if let Some(count) = result.count
                && m.counts.insert(tid.clone(), count) != Some(count)
            {
                let session = m.sessions[&m.tabs[&tid].session_id].clone();
                let key = format!(
                    "{}/{}",
                    session.profile,
                    session.site.as_deref().unwrap_or("")
                );
                self.activity(m, &key, &session, "unread_count", Some(count))
                    .await?;
            }
        }
        Ok(changed)
    }
    async fn fresh_frame(&self, run: &str, sid: &str, tid: &str) -> Result<(ProbeKey, Value)> {
        let (key, client) = {
            let m = self.model.lock().await;
            let session = Self::owned(&m, run, sid)?;
            let tab = m
                .tabs
                .get(tid)
                .ok_or_else(|| Fault::new("unknown_tab", tid))?;
            if !Self::visible(&m, session, tab) {
                return Err(Fault::new("denied", "tab_owner"));
            }
            let key = ProbeKey::of(tab).ok_or_else(|| Fault::new("session_recovering", sid))?;
            let client = m.instances[&session.instance_id].client().clone();
            (key, client)
        };
        let frame = client
            .send_with_timeout(
                Method::PageGetFrameTree,
                json!({}),
                Some(&key.session),
                PROBE_TIMEOUT,
            )
            .await?;
        Ok((key, frame))
    }
    async fn reconcile_documents(&self, m: &mut Model) -> Result<bool> {
        let ids = m.sessions.keys().cloned().collect::<Vec<_>>();
        let mut changed = false;
        for sid in ids {
            let session = m.sessions[&sid].clone();
            if !matches!(
                session.state,
                SessionState::Ready | SessionState::Busy | SessionState::Blocked
            ) {
                continue;
            }
            if session.state == SessionState::Blocked
                && !matches!(
                    session.blocked_reason.as_deref(),
                    Some("needs_login" | "document_origin")
                )
            {
                continue;
            }
            let tabs = m
                .tabs
                .values()
                .filter(|t| {
                    Self::visible(m, &session, t) && t.target_id.is_some() && t.state != "closed"
                })
                .collect::<Vec<_>>();
            if tabs.is_empty() {
                continue;
            }
            let invalid = tabs
                .iter()
                .find(|t| self.document_allowed(&session, &t.url, false).is_err());
            if let Some(tab) = invalid {
                let login = self
                    .site(&session.profile, session.site.as_deref())?
                    .is_some_and(|site| origin(&tab.url).is_some_and(|o| site.login.contains(&o)));
                let reason = if login {
                    "needs_login"
                } else {
                    "document_origin"
                };
                if session.state != SessionState::Blocked
                    || session.blocked_reason.as_deref() != Some(reason)
                {
                    m.sessions
                        .get_mut(&sid)
                        .unwrap()
                        .set_state(SessionState::Blocked, Some(reason))?;
                    if login {
                        let profile = session.profile.clone();
                        let site = session.site.clone().unwrap();
                        self.db.call(move|c|{c.execute("INSERT OR REPLACE INTO web_site_state VALUES(?1,?2,'needs_login',?3)",params![profile,site,now()])?;Ok(())}).await?;
                    }
                    self.db
                        .event(
                            "session_state",
                            json!({"session_id":sid,"state":"blocked","reason":reason}),
                        )
                        .await?;
                    changed = true;
                }
            } else if session.state == SessionState::Blocked
                && tabs.iter().all(|t| t.state != "crashed")
            {
                let session = m.sessions.get_mut(&sid).unwrap();
                session.set_state(SessionState::Ready, None)?;
                if m.busy.get(&sid).copied().unwrap_or(0) > 0 {
                    session.set_state(SessionState::Busy, None)?;
                }
                self.db.event("session_state",json!({"session_id":sid,"state":session.state,"reason":"document_condition_cleared"})).await?;
                changed = true;
            }
        }
        Ok(changed)
    }
    async fn revive_listener(&self, m: &mut Model, tid: &str) -> Result<()> {
        if m.tab_retry
            .get(tid)
            .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
        {
            return Ok(());
        }
        if let Err(error) = self.revive_tab(m, tid).await {
            m.tab_retry.insert(tid.into(), tokio::time::Instant::now());
            self.db.event("tab_state",json!({"tab_id":tid,"state":"crashed","reason":"recovery_failed","error":error.kind})).await?;
        } else {
            m.tab_retry.remove(tid);
        }
        Ok(())
    }
    async fn activity(
        &self,
        m: &mut Model,
        key: &str,
        s: &Session,
        kind: &str,
        count: Option<i64>,
    ) -> Result<()> {
        let pending = m.pending_activity.entry(key.into()).or_default();
        pending.session_id = s.id.clone();
        pending.profile = s.profile.clone();
        pending.site = s.site.clone();
        if kind == "title_changed" {
            pending.title_changed = true;
        }
        if kind == "unread_count" {
            pending.unread_changed = true;
            pending.count = count;
        }
        Ok(())
    }
    async fn flush_activity(&self, m: &mut Model) -> Result<()> {
        let keys = m.pending_activity.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            if m.last_activity
                .get(&key)
                .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
            {
                continue;
            }
            let p = m.pending_activity[&key].clone();
            let session = m
                .sessions
                .values()
                .find(|s| {
                    s.id == p.session_id
                        && !matches!(s.state, SessionState::Closed | SessionState::Lost)
                })
                .or_else(|| {
                    m.sessions.values().find(|s| {
                        s.profile == p.profile
                            && s.site == p.site
                            && s.listen
                            && !matches!(s.state, SessionState::Closed | SessionState::Lost)
                    })
                });
            let Some(session) = session else {
                m.pending_activity.remove(&key);
                continue;
            };
            let mut kinds = vec![];
            if p.title_changed {
                kinds.push("title_changed")
            }
            if p.unread_changed {
                kinds.push("unread_count")
            }
            self.db.event("listener_activity",json!({"session_id":session.id,"site":p.site,"kind":if p.unread_changed{"unread_count"}else{"title_changed"},"kinds":kinds,"count":p.count})).await?;
            m.last_activity
                .insert(key.clone(), tokio::time::Instant::now());
            m.pending_activity.remove(&key);
        }
        Ok(())
    }
}
fn retry_delay(attempts: usize) -> Duration {
    Duration::from_secs(match attempts {
        0 => 1,
        1 => 5,
        _ => 30,
    })
}
async fn probe_tab(
    client: excalibur_cdp::Client,
    key: ProbeKey,
    selector: Option<String>,
) -> ProbeResult {
    let mut result = ProbeResult {
        key: key.clone(),
        url: None,
        count: None,
        crashed: false,
    };
    let _ = tokio::time::timeout(PROBE_TIMEOUT, async {
        match client
            .send(Method::PageGetFrameTree, json!({}), Some(&key.session))
            .await
        {
            Ok(tree) => {
                result.url = tree["frameTree"]["frame"]["url"]
                    .as_str()
                    .map(str::to_owned);
                if let Some(selector) = selector {
                    result.count = read_unread(&client, &key.session, &tree, &selector).await;
                }
            }
            Err(excalibur_cdp::Error::Protocol { message, .. }) if message.contains("crashed") => {
                result.crashed = true
            }
            _ => {}
        }
    })
    .await;
    result
}
async fn read_unread(
    client: &excalibur_cdp::Client,
    cdp: &str,
    tree: &Value,
    selector: &str,
) -> Option<i64> {
    let world = client
        .send(
            Method::PageCreateIsolatedWorld,
            json!({"frameId":tree["frameTree"]["frame"]["id"],"worldName":"excalibur-listener"}),
            Some(cdp),
        )
        .await
        .ok()?;
    let function = format!(
        "function(){{try{{return document.querySelectorAll({}).length}}catch{{return -1}}}}",
        json!(selector)
    );
    let value=client.send(Method::RuntimeCallFunctionOn,json!({"executionContextId":world["executionContextId"],"functionDeclaration":function,"returnByValue":true}),Some(cdp)).await.ok()?;
    value["result"]["value"].as_i64().filter(|n| *n >= 0)
}
fn remove_stale_lock(dir: &Path) -> Result<()> {
    let lock = dir.join("SingletonLock");
    let Ok(value) = fs::read_link(&lock) else {
        return Ok(());
    };
    let Some(pid) = value
        .to_str()
        .and_then(|s| s.rsplit('-').next())
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|p| *p > 0)
    else {
        return Err(Fault::new("profile_busy", "unrecognized SingletonLock"));
    };
    if unsafe { libc::kill(pid, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        return Err(Fault::new(
            "profile_busy",
            "SingletonLock process is alive or inaccessible",
        ));
    }
    fs::remove_file(lock)?;
    Ok(())
}

impl Manager {
    pub(crate) async fn observation_context(
        &self,
        run: &str,
        sid: &str,
        tid: &str,
    ) -> Result<crate::observation::Context> {
        self.accessible(run, sid, tid).await?;
        let model = self.model.lock().await;
        let session = Self::owned(&model, run, sid)?.clone();
        let tab = model.tabs[tid].clone();
        let client = model
            .instances
            .get(&session.instance_id)
            .ok_or_else(|| Fault::new("session_recovering", sid))?
            .client()
            .clone();
        Ok(crate::observation::Context {
            session,
            tab,
            client,
            logged_in: false,
        })
    }
    /// Executes observation with a tab lease, never a global model guard across renderer calls.
    pub async fn observe(
        &self,
        run: &str,
        sid: &str,
        tid: &str,
        method: &str,
        mut p: Value,
    ) -> Result<Value> {
        let _lease = self.lease(run, sid, tid, false).await?;
        let mut ctx = self.observation_context(run, sid, tid).await?;
        if let Some(site) = &ctx.session.site {
            ctx.logged_in = self
                .site_states(&ctx.session.profile)
                .await?
                .as_array()
                .is_some_and(|states| {
                    states
                        .iter()
                        .any(|v| v["name"] == *site && v["login_state"] == "logged_in")
                });
        }
        // Blob authorization belongs to the requesting run, including shared listener observers.
        ctx.session.owner_run = run.to_owned();
        if let Some(site) = ctx.session.site.as_ref().and_then(|name| {
            self.options
                .profiles
                .get(&ctx.session.profile)
                .and_then(|profile| profile.sites.iter().find(|s| &s.name == name))
        }) && let Some(criteria) = &site.wait
        {
            if p.get("criteria").is_none() {
                p["criteria"] = json!(criteria);
            }
            if method == "observe.snapshot" && p["options"]["wait"].is_null() {
                if p["options"].is_null() {
                    p["options"] = json!({});
                }
                p["options"]["wait"] = json!(criteria);
            }
        }
        match method {
            "observe.wait_ready" => {
                let criteria =
                    serde_json::from_value(p.get("criteria").cloned().unwrap_or(json!({})))
                        .map_err(|e| Fault::new("invalid_params", e))?;
                Ok(json!(self.observation.wait(&ctx, criteria).await?))
            }
            "observe.snapshot" => {
                let options =
                    serde_json::from_value(p.get("options").cloned().unwrap_or(json!({})))
                        .map_err(|e| Fault::new("invalid_params", e))?;
                let result = self.observation.snapshot(&ctx, options).await?;
                let blobs = std::iter::once(result.dom_tree_ref.sha256.clone())
                    .chain(result.screenshot_ref.as_ref().map(|r| r.sha256.clone()))
                    .collect::<Vec<_>>();
                let run = run.to_owned();
                let id = result.snapshot_id.clone();
                self.db
                    .call(move |c| {
                        let tx = c.transaction()?;
                        for hash in blobs {
                            tx.execute(
                                "INSERT OR IGNORE INTO web_observation_blobs VALUES(?1,?2,?3,?4)",
                                params![hash, run, id, now()],
                            )?;
                        }
                        tx.commit()?;
                        Ok(())
                    })
                    .await?;
                Ok(json!(result))
            }
            "observe.subtree" => {
                self.observation
                    .subtree(&ctx, required(&p, "snapshot_id")?, p["ref"].as_str())
                    .await
            }
            "observe.validate_ref" => {
                self.observation
                    .validate(&ctx, required(&p, "snapshot_id")?, required(&p, "ref")?)
                    .await
            }
            "devtools.console" | "devtools.network" | "devtools.performance" => {
                self.observation
                    .devtools(&ctx, method.trim_start_matches("devtools."), p)
                    .await
            }
            "emulate.viewport" => self.observation.emulate(&ctx, &p).await,
            "nav.go" | "nav.reload" | "nav.back" | "nav.forward" => {
                let _dialog_scope = self.observation.action_scope(&ctx, false, true);
                let frames = self.observation.frames(&ctx).await?;
                let _network = self.observation.initialize(&ctx, &frames).await?;
                if method != "nav.go" {
                    self.document_allowed(&ctx.session, &frames[0].url, true)?;
                }
                let client = &ctx.client;
                let cdp = ctx.tab.cdp_session.as_deref();
                if method == "nav.go" {
                    let url = required(&p, "url")?;
                    self.document_allowed(&ctx.session, url, true)?;
                    client
                        .send_with_timeout(
                            Method::PageNavigate,
                            json!({"url":url}),
                            cdp,
                            Duration::from_secs(2),
                        )
                        .await?;
                } else if method == "nav.reload" {
                    client
                        .send_with_timeout(
                            Method::PageReload,
                            json!({}),
                            cdp,
                            Duration::from_secs(2),
                        )
                        .await?;
                } else {
                    let history = client
                        .send(Method::PageGetNavigationHistory, json!({}), cdp)
                        .await?;
                    let index = history["currentIndex"].as_i64().unwrap_or(0)
                        + if method == "nav.back" { -1 } else { 1 };
                    let entry = history["entries"]
                        .as_array()
                        .and_then(|a| usize::try_from(index).ok().and_then(|i| a.get(i)))
                        .ok_or_else(|| Fault::new("invalid_params", "history boundary"))?;
                    self.document_allowed(&ctx.session, entry["url"].as_str().unwrap_or(""), true)?;
                    client
                        .send_with_timeout(
                            Method::PageNavigateToHistoryEntry,
                            json!({"entryId":entry["id"]}),
                            cdp,
                            Duration::from_secs(2),
                        )
                        .await?;
                }
                let criteria =
                    serde_json::from_value(p.get("criteria").cloned().unwrap_or(json!({})))
                        .map_err(|e| Fault::new("invalid_params", e))?;
                Ok(json!(self.observation.wait(&ctx, criteria).await?))
            }
            _ => Err(Fault::new("method_not_found", method)),
        }
    }
    /// Internal post-dispatch readiness boundary; it sends no input and never fails an action.
    pub async fn settle_observation(&self, run: &str, sid: &str, tid: &str) -> Value {
        tokio::time::sleep(Duration::from_millis(250)).await;
        match self
            .observe(
                run,
                sid,
                tid,
                "observe.wait_ready",
                json!({"criteria":{"timeout_ms":5000}}),
            )
            .await
        {
            Ok(wait) => json!({"settled":wait["status"]=="ready","wait":wait}),
            Err(error) => json!({"settled":false,"error":error.kind}),
        }
    }
    pub async fn observation_blob(
        &self,
        run: &str,
        hash: &str,
        offset: u64,
        length: usize,
    ) -> Result<Value> {
        self.observation
            .read_blob(&self.db, run, hash, offset, length)
            .await
    }
}
fn required<'a>(p: &'a Value, key: &str) -> Result<&'a str> {
    p[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Fault::new("invalid_params", key))
}

impl Manager {
    pub(crate) async fn credential_frame_allowed(&self, s: &Session, origin: &str) -> bool {
        self.site(&s.profile, s.site.as_deref())
            .ok()
            .flatten()
            .is_some_and(|site| {
                site.credential_frames.iter().any(|o| o == origin)
                    && !site.login.iter().any(|o| o == origin)
            })
    }
    pub(crate) async fn adopt_action_popups(
        &self,
        ctx: &crate::observation::Context,
    ) -> Result<Value> {
        let targets = ctx
            .client
            .send_with_timeout(
                Method::TargetGetTargets,
                json!({}),
                None,
                Duration::from_millis(500),
            )
            .await?;
        let mut tabs = vec![];
        let mut blocked = vec![];
        for info in targets["targetInfos"].as_array().into_iter().flatten() {
            if info["type"] != "page"
                || info["openerId"] != ctx.tab.target_id.as_deref().unwrap_or("")
            {
                continue;
            }
            let Some(target) = info["targetId"].as_str() else {
                continue;
            };
            {
                let m = self.model.lock().await;
                if m.tabs
                    .values()
                    .any(|t| t.target_id.as_deref() == Some(target))
                {
                    continue;
                }
            }
            let url = info["url"].as_str().unwrap_or("");
            if self.document_allowed(&ctx.session, url, false).is_err() {
                ctx.client
                    .send(Method::TargetCloseTarget, json!({"targetId":target}), None)
                    .await?;
                blocked.push(json!({"origin":origin(url),"reason":"origin_not_writable"}));
                continue;
            }
            let instance = {
                let m = self.model.lock().await;
                m.instances
                    .get(&ctx.session.instance_id)
                    .cloned()
                    .ok_or_else(|| Fault::new("session_recovering", "popup instance"))?
            };
            let cdp = match tokio::time::timeout(
                Duration::from_millis(750),
                instance.attach_page(target),
            )
            .await
            {
                Ok(Ok(sid)) => sid,
                _ => {
                    let _ = ctx
                        .client
                        .send(Method::TargetCloseTarget, json!({"targetId":target}), None)
                        .await;
                    blocked.push(json!({"reason":"popup_unresponsive"}));
                    continue;
                }
            };
            let mut m = self.model.lock().await;
            let count = m
                .tabs
                .values()
                .filter(|t| t.state != "closed" && t.target_id.is_some())
                .count();
            let own = m
                .tabs
                .values()
                .filter(|t| {
                    t.owner_run == ctx.session.owner_run
                        && t.state != "closed"
                        && t.target_id.is_some()
                })
                .count();
            if count >= 12 || own >= 4 {
                drop(m);
                let _ = ctx
                    .client
                    .send(Method::TargetCloseTarget, json!({"targetId":target}), None)
                    .await;
                blocked.push(json!({"reason":"tab_limit"}));
                continue;
            }
            let tab = Tab {
                id: id(),
                session_id: ctx.session.id.clone(),
                target_id: Some(target.into()),
                cdp_session: Some(cdp),
                owner_run: ctx.session.owner_run.clone(),
                label: Some("popup".into()),
                url: url.into(),
                title: info["title"].as_str().unwrap_or("").into(),
                state: "ready".into(),
                document_epoch: 1,
                listen: false,
            };
            tabs.push(json!(tab));
            m.tabs.insert(tab.id.clone(), tab);
            self.db.save(&mut m).await?;
        }
        Ok(json!({"tabs":tabs,"blocked":blocked}))
    }
}
#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn restart_backoff_is_bounded() {
        assert_eq!(retry_delay(0), Duration::from_secs(1));
        assert_eq!(retry_delay(1), Duration::from_secs(5));
        for attempts in 2..10 {
            assert_eq!(retry_delay(attempts), Duration::from_secs(30));
        }
    }
    #[tokio::test]
    async fn only_document_blocks_clear_when_the_origin_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "personal".into(),
            Profile {
                sites: vec![Site {
                    name: "chat".into(),
                    rw: vec!["http://chat.test".into()],
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        let manager = Manager::open(Options {
            data_dir: dir.path().into(),
            binary: "/not-used".into(),
            proxy: "127.0.0.1:12345".parse().unwrap(),
            profiles,
            compact: false,
            version: VersionPolicy::default(),
            chrome_args: vec![],
        })
        .await
        .unwrap();
        let mut model = manager.model.lock().await;
        let mut session = Session {
            id: "s".into(),
            owner_run: "r".into(),
            profile: "personal".into(),
            context_id: None,
            instance_id: "personal".into(),
            mode: "headless".into(),
            site: Some("chat".into()),
            listen: false,
            state: SessionState::Ready,
            blocked_reason: None,
            origins: vec![],
            credentials: vec![],
            created_at: now(),
            updated_at: now(),
        };
        session
            .set_state(SessionState::Blocked, Some("version_blocked"))
            .unwrap();
        model.sessions.insert("s".into(), session);
        model.tabs.insert(
            "t".into(),
            Tab {
                id: "t".into(),
                session_id: "s".into(),
                target_id: Some("target".into()),
                cdp_session: None,
                owner_run: "r".into(),
                label: None,
                url: "http://chat.test/".into(),
                title: String::new(),
                state: "ready".into(),
                document_epoch: 1,
                listen: false,
            },
        );
        assert!(!manager.reconcile_documents(&mut model).await.unwrap());
        assert_eq!(model.sessions["s"].state, SessionState::Blocked);
        model
            .sessions
            .get_mut("s")
            .unwrap()
            .set_state(SessionState::Blocked, Some("document_origin"))
            .unwrap();
        assert!(manager.reconcile_documents(&mut model).await.unwrap());
        assert_eq!(model.sessions["s"].state, SessionState::Ready);
        assert_eq!(model.sessions["s"].blocked_reason, None);
        model
            .sessions
            .get_mut("s")
            .unwrap()
            .set_state(SessionState::Blocked, Some("needs_login"))
            .unwrap();
        model
            .sessions
            .get_mut("s")
            .unwrap()
            .set_state(SessionState::HumanControl, None)
            .unwrap();
        assert!(!manager.reconcile_documents(&mut model).await.unwrap());
        assert_eq!(model.sessions["s"].state, SessionState::HumanControl);
    }
    #[tokio::test]
    async fn late_probe_results_cannot_modify_a_replaced_navigated_or_closed_tab() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let manager = Manager::open(Options {
            data_dir: directory.path().into(),
            binary: "/not-used".into(),
            proxy: "127.0.0.1:12345".parse().unwrap(),
            profiles: BTreeMap::new(),
            compact: false,
            version: VersionPolicy::default(),
            chrome_args: vec![],
        })
        .await
        .unwrap();
        let mut model = manager.model.lock().await;
        model.tabs.insert(
            "t".into(),
            Tab {
                id: "t".into(),
                session_id: "s".into(),
                target_id: Some("old-target".into()),
                cdp_session: Some("old-session".into()),
                owner_run: "r".into(),
                label: None,
                url: "http://login.test/".into(),
                title: String::new(),
                state: "ready".into(),
                document_epoch: 1,
                listen: true,
            },
        );
        for change in ["replacement", "navigation", "close"] {
            let old = ProbeKey::of(&model.tabs["t"]).unwrap();
            let tab = model.tabs.get_mut("t").unwrap();
            match change {
                "replacement" => {
                    tab.target_id = Some("new-target".into());
                    tab.cdp_session = Some("new-session".into());
                }
                "navigation" => tab.document_epoch += 1,
                _ => {
                    tab.target_id = None;
                    tab.cdp_session = None;
                    tab.state = "closed".into();
                }
            }
            model.probes.spawn(async move {
                ProbeResult {
                    key: old,
                    url: Some("http://rw.test/".into()),
                    count: Some(999),
                    crashed: false,
                }
            });
            while !model.probes.is_empty() {
                assert!(!manager.collect_probes(&mut model).await.unwrap());
                tokio::task::yield_now().await;
            }
            assert_eq!(model.tabs["t"].url, "http://login.test/");
            assert!(!model.counts.contains_key("t"));
            assert!(!model.frame_sources.contains_key("t"));
        }
    }
}

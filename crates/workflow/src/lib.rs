//! Durable workflows and sagas (architecture 21.4/21.5 with the 17.7 retry policy).
//!
//! Versioned definitions are immutable; each instance is pinned to the version and
//! definition hash it started with. Every node is a durable step: a tool node writes
//! its intent (with a per-instance-and-node idempotency key) before calling the host
//! [`Effector`] and its result afterwards, so a killed process resumes per node.
//! Retries of the same node reuse the same key. Abort runs declared compensations
//! in reverse order. Dry runs never receive an effector at all.
//!
//! Single-writer ownership comes from the store (one process owns the database) plus
//! a per-instance async lock inside the process. Event triggers (21.3), the agent,
//! script, parallel, loop and subflow node kinds, workflow test suites and daemon or
//! Agenda dispatch are not implemented here.
mod clock;
mod definition;
mod retry;

pub use clock::{Clock, ManualClock, SystemClock};
pub use definition::{Compensation, MAX_NODES, Node, NodeKind, WorkflowDefinition};
pub use excalibur_types::runtime::Effect;
pub use retry::{FailureClass, RetryPolicy};

use excalibur_store::{
    Database, DatabaseKind, Priority, ReadLane,
    rusqlite::{self, Connection, OptionalExtension, params},
};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, Weak},
};

const CONFLICT: &str = "workflow instance revision conflict";

#[derive(Debug)]
pub enum Error {
    Store(excalibur_store::Error),
    Invalid(String),
    NotFound,
    /// Another transition changed the instance first.
    Conflict,
    /// A live instance was given to the simulator or a dry run to a live effector.
    ModeMismatch,
    /// The pinned definition no longer matches its recorded hash.
    DefinitionChanged,
    /// Logical corrections are exhausted; the plan must change (8.5).
    ReplanRequired,
    NotAllowed(&'static str),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "workflow storage: {e}"),
            Self::Invalid(e) => write!(f, "invalid workflow request: {e}"),
            Self::NotFound => f.write_str("workflow or instance not found"),
            Self::Conflict => f.write_str(CONFLICT),
            Self::ModeMismatch => f.write_str("instance mode does not match the driver"),
            Self::DefinitionChanged => f.write_str("pinned workflow definition hash mismatch"),
            Self::ReplanRequired => f.write_str("logical corrections exhausted; replan required"),
            Self::NotAllowed(e) => write!(f, "transition not allowed: {e}"),
        }
    }
}
impl std::error::Error for Error {}
impl From<excalibur_store::Error> for Error {
    fn from(e: excalibur_store::Error) -> Self {
        match e {
            excalibur_store::Error::Invalid(m) if m == CONFLICT => Self::Conflict,
            e => Self::Store(e),
        }
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e.into())
    }
}
pub type Result<T> = std::result::Result<T, Error>;
type SResult<T> = excalibur_store::Result<T>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Live,
    DryRun,
}
impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::DryRun => "dry_run",
        }
    }
}

/// Observable instance status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Status {
    Running,
    /// Retry backoff or a wait node; resumes when the clock reaches `until`.
    Waiting {
        until: i64,
    },
    WaitingApproval {
        node: String,
    },
    /// Permanent failure, exhausted transient retries or failed compensation.
    Paused {
        class: Option<FailureClass>,
        reason: String,
    },
    /// An interrupted non-idempotent effect needs reconciliation; never repeated.
    Blocked {
        node: String,
        reason: String,
    },
    /// Input or logical failure returned as the result.
    Failed {
        class: FailureClass,
        reason: String,
    },
    Compensating,
    Completed,
    Aborted {
        reason: String,
    },
}

/// One effect request. `idempotency_key` is stable for the instance and node across
/// every attempt and every replay after a crash; compensations use `<key>:compensate`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EffectRequest {
    pub instance_id: String,
    pub node_id: String,
    pub tool: String,
    pub input: Value,
    /// `{"input": instance input, "outputs": {node: output}}`.
    pub context: Value,
    pub idempotency_key: String,
    pub attempt: u32,
    pub compensation: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepFailure {
    pub class: FailureClass,
    pub message: String,
    /// Provider `Retry-After` hint in seconds, honoured for transient failures.
    pub retry_after: Option<u64>,
}
impl StepFailure {
    pub fn new(class: FailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            retry_after: None,
        }
    }
}

/// Host tool execution for live instances. Keyed effects must deduplicate by key.
pub trait Effector: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: &'a EffectRequest,
    ) -> BoxFuture<'a, std::result::Result<Value, StepFailure>>;
}

/// Simulated responses for a dry run. Per-node lists are consumed by attempt (the last
/// entry repeats). Without an entry the node's declared `dry_run` output is used; a
/// node with neither fails closed as an input failure, it is never executed.
#[derive(Clone, Debug, Default)]
pub struct Scenario {
    pub outputs: BTreeMap<String, Vec<std::result::Result<Value, StepFailure>>>,
    pub approvals: BTreeMap<String, bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InstanceSnapshot {
    pub id: String,
    pub workflow_id: String,
    pub version: u32,
    pub definition_hash: String,
    pub mode: Mode,
    pub input: Value,
    pub status: Status,
    pub cursor: Option<String>,
    pub revision: i64,
    pub outputs: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StepRecord {
    pub node_id: String,
    pub seq: i64,
    pub phase: String,
    pub idempotency_key: String,
    pub attempts: u32,
    pub logical_failures: u32,
    pub output: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LogEntry {
    pub seq: i64,
    pub at: i64,
    pub node_id: Option<String>,
    pub kind: String,
    pub idempotency_key: Option<String>,
    pub attempt: Option<u32>,
    pub detail: Value,
}

enum Driver<'a> {
    Live(&'a dyn Effector),
    Simulated(&'a Scenario),
}
impl Driver<'_> {
    fn mode(&self) -> Mode {
        match self {
            Self::Live(_) => Mode::Live,
            Self::Simulated(_) => Mode::DryRun,
        }
    }
    async fn call(
        &self,
        request: &EffectRequest,
        declared: Option<&Value>,
    ) -> std::result::Result<Value, StepFailure> {
        match self {
            Self::Live(effector) => effector.execute(request).await,
            Self::Simulated(scenario) => {
                if request.compensation {
                    return Ok(declared.cloned().unwrap_or(Value::Null));
                }
                if let Some(list) = scenario.outputs.get(&request.node_id)
                    && let Some(last) = list.last()
                {
                    let index = (request.attempt.saturating_sub(1) as usize).min(list.len() - 1);
                    return list.get(index).unwrap_or(last).clone();
                }
                declared.cloned().ok_or_else(|| {
                    StepFailure::new(
                        FailureClass::Input,
                        format!(
                            "tool {} has no declared dry_run behaviour; not executed",
                            request.tool
                        ),
                    )
                })
            }
        }
    }
}

#[derive(Clone, Debug)]
struct Row {
    workflow_id: String,
    version: u32,
    hash: String,
    mode: Mode,
    input: Value,
    status: String,
    cursor: Option<String>,
    wake_at: Option<i64>,
    class: Option<String>,
    reason: Option<String>,
    revision: i64,
}
impl Row {
    fn status(&self) -> Status {
        let reason = self.reason.clone().unwrap_or_default();
        let class = self.class.as_deref().and_then(FailureClass::parse);
        match self.status.as_str() {
            "running" => Status::Running,
            "waiting" => Status::Waiting {
                until: self.wake_at.unwrap_or(0),
            },
            "waiting_approval" => Status::WaitingApproval {
                node: self.cursor.clone().unwrap_or_default(),
            },
            "paused" => Status::Paused { class, reason },
            "blocked" => Status::Blocked {
                node: self.cursor.clone().unwrap_or_default(),
                reason,
            },
            "failed" => Status::Failed {
                class: class.unwrap_or(FailureClass::Input),
                reason,
            },
            "compensating" => Status::Compensating,
            "completed" => Status::Completed,
            _ => Status::Aborted { reason },
        }
    }
}

/// The next instance state written atomically with a step change.
struct Next<'a> {
    status: &'a str,
    cursor: Option<&'a str>,
    wake_at: Option<i64>,
    class: Option<&'a str>,
    reason: Option<&'a str>,
}
impl<'a> Next<'a> {
    fn running(cursor: Option<&'a str>) -> Self {
        Self {
            status: if cursor.is_some() {
                "running"
            } else {
                "completed"
            },
            cursor,
            wake_at: None,
            class: None,
            reason: None,
        }
    }
}

fn transition(
    c: &Connection,
    scope: &str,
    id: &str,
    revision: i64,
    now: i64,
    next: &Next<'_>,
) -> SResult<()> {
    let changed = c.execute(
        "UPDATE workflow_instances SET revision=revision+1,updated_at=?4,status=?5,cursor=?6,\
         wake_at=?7,failure_class=?8,reason=?9 WHERE scope=?1 AND id=?2 AND revision=?3",
        params![
            scope,
            id,
            revision,
            now,
            next.status,
            next.cursor,
            next.wake_at,
            next.class,
            next.reason
        ],
    )?;
    if changed != 1 {
        return Err(excalibur_store::Error::Invalid(CONFLICT));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn log(
    c: &Connection,
    scope: &str,
    id: &str,
    now: i64,
    node: Option<&str>,
    kind: &str,
    key: Option<&str>,
    attempt: Option<u32>,
    detail: Value,
) -> SResult<()> {
    c.execute(
        "INSERT INTO workflow_log(scope,instance_id,seq,at,node_id,kind,idempotency_key,attempt,detail) \
         VALUES(?1,?2,(SELECT COALESCE(MAX(seq),0)+1 FROM workflow_log WHERE scope=?1 AND instance_id=?2),\
         ?3,?4,?5,?6,?7,?8)",
        params![scope, id, now, node, kind, key, attempt, detail.to_string()],
    )?;
    Ok(())
}

fn json_err(_: serde_json::Error) -> excalibur_store::Error {
    excalibur_store::Error::Invalid("invalid persisted workflow data")
}

fn load_row(c: &Connection, scope: &str, id: &str) -> SResult<Option<Row>> {
    let raw = c
        .query_row(
            "SELECT workflow_id,version,definition_hash,mode,input,status,cursor,wake_at,\
             failure_class,reason,revision FROM workflow_instances WHERE scope=?1 AND id=?2",
            params![scope, id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, u32>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, i64>(10)?,
                ))
            },
        )
        .optional()?;
    let Some((
        workflow_id,
        version,
        hash,
        mode,
        input,
        status,
        cursor,
        wake_at,
        class,
        reason,
        revision,
    )) = raw
    else {
        return Ok(None);
    };
    Ok(Some(Row {
        workflow_id,
        version,
        hash,
        mode: if mode == "dry_run" {
            Mode::DryRun
        } else {
            Mode::Live
        },
        input: serde_json::from_str(&input).map_err(json_err)?,
        status,
        cursor,
        wake_at,
        class,
        reason,
        revision,
    }))
}

fn load_steps(c: &Connection, scope: &str, id: &str) -> SResult<Vec<StepRecord>> {
    let mut q = c.prepare(
        "SELECT node_id,seq,phase,idempotency_key,attempts,logical_failures,output \
         FROM workflow_steps WHERE scope=?1 AND instance_id=?2 ORDER BY seq",
    )?;
    let rows = q
        .query_map(params![scope, id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, u32>(4)?,
                r.get::<_, u32>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(node_id, seq, phase, key, attempts, logical, output)| {
            Ok(StepRecord {
                node_id,
                seq,
                phase,
                idempotency_key: key,
                attempts,
                logical_failures: logical,
                output: output
                    .map(|o| serde_json::from_str(&o))
                    .transpose()
                    .map_err(json_err)?,
            })
        })
        .collect()
}

fn load_definition(
    c: &Connection,
    scope: &str,
    workflow_id: &str,
    version: u32,
) -> SResult<Option<(String, String)>> {
    Ok(c.query_row(
        "SELECT definition,hash FROM workflow_definitions WHERE scope=?1 AND workflow_id=?2 AND version=?3",
        params![scope, workflow_id, version],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?)
}

fn outputs_of(steps: &[StepRecord]) -> BTreeMap<String, Value> {
    steps
        .iter()
        .filter(|s| matches!(s.phase.as_str(), "done" | "compensating" | "compensated"))
        .filter_map(|s| s.output.clone().map(|o| (s.node_id.clone(), o)))
        .collect()
}

fn instance_lock(path: PathBuf, scope: &str, id: &str) -> Arc<tokio::sync::Mutex<()>> {
    type Locks = Mutex<HashMap<(PathBuf, String, String), Weak<tokio::sync::Mutex<()>>>>;
    static LOCKS: OnceLock<Locks> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    locks.retain(|_, l| l.strong_count() > 0);
    let key = (path, scope.to_string(), id.to_string());
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

fn bounded_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "instance ids must be 1..256 printable bytes".into(),
        ));
    }
    Ok(())
}

/// Durable workflow runtime for one authenticated scope.
#[derive(Clone)]
pub struct Workflows {
    db: Database,
    scope: String,
    clock: Arc<dyn Clock>,
}

impl Workflows {
    /// The scope must come from authenticated host authority, never workflow content.
    pub async fn open(db: Database, scope: String, clock: Arc<dyn Clock>) -> Result<Self> {
        if scope.is_empty() || scope.len() > 512 {
            return Err(Error::Invalid("scope must be 1..512 bytes".into()));
        }
        if db.kind() != DatabaseKind::Main {
            return Err(Error::Store(excalibur_store::Error::PrincipalOnly));
        }
        db.write(Priority::Journal, |c| {
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS workflow_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL)",
            )?;
            {
                let (version, schema) = (1, include_str!("schema.sql"));
                let checksum = format!("{:x}", Sha256::digest(schema.as_bytes()));
                let known: Option<String> = c
                    .query_row(
                        "SELECT checksum FROM workflow_schema WHERE version=?1",
                        [version],
                        |r| r.get(0),
                    )
                    .optional()?;
                if known.as_ref().is_some_and(|stored| stored != &checksum) {
                    return Err(excalibur_store::Error::Invalid(
                        "workflow schema checksum mismatch",
                    ));
                }
                c.execute_batch(schema)?;
                c.execute(
                    "INSERT OR IGNORE INTO workflow_schema(version,checksum) VALUES(?1,?2)",
                    params![version, checksum],
                )?;
            }
            Ok(())
        })
        .await?;
        Ok(Self { db, scope, clock })
    }

    /// The per-instance, per-node key shared by every attempt of that node.
    pub fn idempotency_key(&self, instance_id: &str, node_id: &str) -> String {
        let digest = Sha256::digest(format!("{}\0{instance_id}\0{node_id}", self.scope).as_bytes());
        format!("wf-{:x}", digest)[..35].to_string()
    }

    /// Publishes an immutable version. Republishing identical content is idempotent;
    /// different content under an existing version is rejected.
    pub async fn publish(&self, definition: WorkflowDefinition) -> Result<String> {
        definition.validate().map_err(Error::Invalid)?;
        let (text, hash) = definition.canonical().map_err(Error::Invalid)?;
        let (scope, now) = (self.scope.clone(), self.clock.now());
        let stored_hash = hash.clone();
        let existing = self
            .db
            .write(Priority::Journal, move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO workflow_definitions(scope,workflow_id,version,definition,hash,published_at) \
                     VALUES(?1,?2,?3,?4,?5,?6)",
                    params![scope, definition.id, definition.version, text, stored_hash, now],
                )?;
                Ok(c.query_row(
                    "SELECT hash FROM workflow_definitions WHERE scope=?1 AND workflow_id=?2 AND version=?3",
                    params![scope, definition.id, definition.version],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .await?;
        if existing != hash {
            return Err(Error::NotAllowed(
                "version already published with another definition",
            ));
        }
        Ok(hash)
    }

    pub async fn latest_version(&self, workflow_id: &str) -> Result<Option<u32>> {
        let (scope, id) = (self.scope.clone(), workflow_id.to_string());
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT MAX(version) FROM workflow_definitions WHERE scope=?1 AND workflow_id=?2",
                    params![scope, id],
                    |r| r.get::<_, Option<u32>>(0),
                )?)
            })
            .await?)
    }

    pub async fn definition(&self, workflow_id: &str, version: u32) -> Result<WorkflowDefinition> {
        let (scope, id) = (self.scope.clone(), workflow_id.to_string());
        let (text, hash) = self
            .db
            .read(ReadLane::Interactive, move |c| {
                load_definition(c, &scope, &id, version)
            })
            .await?
            .ok_or(Error::NotFound)?;
        let definition: WorkflowDefinition =
            serde_json::from_str(&text).map_err(|e| Error::Invalid(e.to_string()))?;
        if definition.canonical().map_err(Error::Invalid)?.1 != hash {
            return Err(Error::DefinitionChanged);
        }
        Ok(definition)
    }

    /// Starts an instance pinned to `version` (latest when `None`). Starting the same
    /// instance ID again with identical arguments returns the existing instance.
    pub async fn start(
        &self,
        instance_id: &str,
        workflow_id: &str,
        version: Option<u32>,
        input: Value,
        mode: Mode,
    ) -> Result<InstanceSnapshot> {
        bounded_id(instance_id)?;
        let version = match version {
            Some(v) => v,
            None => self
                .latest_version(workflow_id)
                .await?
                .ok_or(Error::NotFound)?,
        };
        let definition = self.definition(workflow_id, version).await?;
        let hash = definition.canonical().map_err(Error::Invalid)?.1;
        let input_text = input.to_string();
        if input_text.len() > 256 * 1024 {
            return Err(Error::Invalid("input exceeds 256 KiB".into()));
        }
        let (scope, id, wf, now) = (
            self.scope.clone(),
            instance_id.to_string(),
            workflow_id.to_string(),
            self.clock.now(),
        );
        let start = definition.start.clone();
        let same = self
            .db
            .write(Priority::Journal, move |c| {
                let inserted = c.execute(
                    "INSERT OR IGNORE INTO workflow_instances(scope,id,workflow_id,version,definition_hash,mode,\
                     input,status,cursor,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,'running',?8,?9,?9)",
                    params![scope, id, wf, version, hash, mode.as_str(), input_text, start, now],
                )?;
                if inserted == 1 {
                    log(c, &scope, &id, now, None, "started", None, None,
                        json!({"workflow": wf, "version": version, "hash": hash, "mode": mode.as_str()}))?;
                    return Ok(true);
                }
                let row = load_row(c, &scope, &id)?
                    .ok_or(excalibur_store::Error::Invalid("instance vanished"))?;
                Ok(row.workflow_id == wf
                    && row.version == version
                    && row.mode == mode
                    && serde_json::to_string(&row.input).is_ok_and(|s| s == input_text))
            })
            .await?;
        if !same {
            return Err(Error::NotAllowed(
                "instance id already started with other arguments",
            ));
        }
        self.snapshot(instance_id).await
    }

    pub async fn snapshot(&self, instance_id: &str) -> Result<InstanceSnapshot> {
        let (row, steps) = self.state(instance_id).await?;
        let _definition = self.pinned(&row).await?;
        Ok(InstanceSnapshot {
            id: instance_id.to_string(),
            workflow_id: row.workflow_id.clone(),
            version: row.version,
            definition_hash: row.hash.clone(),
            mode: row.mode,
            input: row.input.clone(),
            status: row.status(),
            cursor: row.cursor.clone(),
            revision: row.revision,
            outputs: outputs_of(&steps),
        })
    }

    pub async fn steps(&self, instance_id: &str) -> Result<Vec<StepRecord>> {
        Ok(self.state(instance_id).await?.1)
    }

    pub async fn log(&self, instance_id: &str) -> Result<Vec<LogEntry>> {
        let (scope, id) = (self.scope.clone(), instance_id.to_string());
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut q = c.prepare(
                    "SELECT seq,at,node_id,kind,idempotency_key,attempt,detail FROM workflow_log \
                     WHERE scope=?1 AND instance_id=?2 ORDER BY seq",
                )?;
                let rows = q
                    .query_map(params![scope, id], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, Option<u32>>(5)?,
                            r.get::<_, String>(6)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows.into_iter()
                    .map(|(seq, at, node_id, kind, key, attempt, detail)| {
                        Ok(LogEntry {
                            seq,
                            at,
                            node_id,
                            kind,
                            idempotency_key: key,
                            attempt,
                            detail: serde_json::from_str(&detail).map_err(json_err)?,
                        })
                    })
                    .collect()
            })
            .await?)
    }

    /// Advances a live instance as far as possible with the host effector.
    pub async fn advance(&self, instance_id: &str, effector: &dyn Effector) -> Result<Status> {
        let lock = self.lock(instance_id);
        let _guard = lock.lock().await;
        self.drive(instance_id, Driver::Live(effector)).await
    }

    /// Advances a dry-run instance. No effector exists on this path, so even a tool
    /// that misdeclares its effect class cannot run; outputs come from the scenario
    /// or the node's declared `dry_run` behaviour.
    pub async fn simulate(&self, instance_id: &str, scenario: &Scenario) -> Result<Status> {
        let lock = self.lock(instance_id);
        let _guard = lock.lock().await;
        self.drive(instance_id, Driver::Simulated(scenario)).await
    }

    /// Records the user's decision for the approval node the instance waits on.
    /// No effect runs here; approval lets the next `advance` continue, rejection
    /// moves the instance to compensation.
    pub async fn decide_approval(
        &self,
        instance_id: &str,
        node_id: &str,
        approved: bool,
        decided_by: &str,
    ) -> Result<Status> {
        if decided_by.is_empty() || decided_by.len() > 256 {
            return Err(Error::Invalid("decided_by must be 1..256 bytes".into()));
        }
        let lock = self.lock(instance_id);
        let _guard = lock.lock().await;
        let (row, _) = self.state(instance_id).await?;
        let (scope, id, node, by, now) = (
            self.scope.clone(),
            instance_id.to_string(),
            node_id.to_string(),
            decided_by.to_string(),
            self.clock.now(),
        );
        let waiting = row.status == "waiting_approval" && row.cursor.as_deref() == Some(node_id);
        let outcome = self
            .db
            .write(Priority::Interactive, move |c| {
                let existing: Option<Option<bool>> = c
                    .query_row(
                        "SELECT approved FROM workflow_approvals WHERE scope=?1 AND instance_id=?2 AND node_id=?3",
                        params![scope, id, node],
                        |r| r.get(0),
                    )
                    .optional()?;
                match existing {
                    Some(Some(previous)) => Ok(Some(previous == approved)),
                    Some(None) if waiting => {
                        c.execute(
                            "UPDATE workflow_approvals SET approved=?4,decided_by=?5,decided_at=?6 \
                             WHERE scope=?1 AND instance_id=?2 AND node_id=?3",
                            params![scope, id, node, approved, by, now],
                        )?;
                        transition(c, &scope, &id, row.revision, now, &Next::running(Some(&node)))?;
                        log(c, &scope, &id, now, Some(&node), "approval_decided", None, None,
                            json!({"approved": approved, "decided_by": by}))?;
                        Ok(Some(true))
                    }
                    _ => Ok(None),
                }
            })
            .await?;
        match outcome {
            Some(true) => Ok(self.snapshot(instance_id).await?.status),
            Some(false) => Err(Error::Conflict),
            None => Err(Error::NotAllowed(
                "instance is not waiting on that approval",
            )),
        }
    }

    /// Aborts a live instance and runs compensations in reverse order. Calling it again
    /// after a failed compensation continues with the same compensation keys.
    pub async fn abort(
        &self,
        instance_id: &str,
        reason: &str,
        effector: &dyn Effector,
    ) -> Result<Status> {
        self.abort_with(instance_id, reason, Driver::Live(effector))
            .await
    }

    /// Dry-run abort: compensations are simulated and never executed.
    pub async fn abort_simulation(
        &self,
        instance_id: &str,
        reason: &str,
        scenario: &Scenario,
    ) -> Result<Status> {
        self.abort_with(instance_id, reason, Driver::Simulated(scenario))
            .await
    }

    async fn abort_with(
        &self,
        instance_id: &str,
        reason: &str,
        driver: Driver<'_>,
    ) -> Result<Status> {
        let lock = self.lock(instance_id);
        let _guard = lock.lock().await;
        let (row, _) = self.state(instance_id).await?;
        if row.mode != driver.mode() {
            return Err(Error::ModeMismatch);
        }
        match row.status.as_str() {
            "completed" => return Err(Error::NotAllowed("completed instances cannot be aborted")),
            "aborted" => return Ok(row.status()),
            "compensating" => {}
            _ => {
                let (scope, id, now) = (
                    self.scope.clone(),
                    instance_id.to_string(),
                    self.clock.now(),
                );
                let reason = reason.chars().take(1024).collect::<String>();
                self.db
                    .write(Priority::Interactive, move |c| {
                        transition(
                            c,
                            &scope,
                            &id,
                            row.revision,
                            now,
                            &Next {
                                status: "compensating",
                                cursor: row.cursor.as_deref(),
                                wake_at: None,
                                class: None,
                                reason: Some(&reason),
                            },
                        )?;
                        log(
                            c,
                            &scope,
                            &id,
                            now,
                            None,
                            "abort_requested",
                            None,
                            None,
                            json!({"reason": reason}),
                        )
                    })
                    .await?;
            }
        }
        self.drive(instance_id, driver).await
    }

    /// Resumes after a permanent failure, exhausted transient retries or a logical
    /// failure (bounded corrections). The next attempt reuses the node's key.
    pub async fn resume(&self, instance_id: &str) -> Result<Status> {
        let lock = self.lock(instance_id);
        let _guard = lock.lock().await;
        let (row, steps) = self.state(instance_id).await?;
        let definition = self.pinned(&row).await?;
        if steps
            .iter()
            .any(|s| matches!(s.phase.as_str(), "compensating" | "compensated"))
        {
            return Err(Error::NotAllowed(
                "an aborting instance continues with abort",
            ));
        }
        let node = row
            .cursor
            .clone()
            .ok_or(Error::NotAllowed("nothing to resume"))?;
        let step = steps
            .iter()
            .find(|s| s.node_id == node)
            .ok_or(Error::NotAllowed("nothing to resume"))?;
        match (row.status.as_str(), row.class.as_deref()) {
            ("paused", _) => {}
            ("failed", Some("logical")) => {
                let limit = match definition.node(&node).map(|n| &n.kind) {
                    Some(NodeKind::Tool { retry, .. }) => retry.logical_attempts,
                    _ => 0,
                };
                if step.logical_failures >= limit {
                    return Err(Error::ReplanRequired);
                }
            }
            ("failed", _) => {
                return Err(Error::NotAllowed(
                    "input failures return to the agent; no blind retry",
                ));
            }
            _ => return Err(Error::NotAllowed("instance is not paused or failed")),
        }
        let (scope, id, now) = (
            self.scope.clone(),
            instance_id.to_string(),
            self.clock.now(),
        );
        self.db
            .write(Priority::Interactive, move |c| {
                c.execute(
                    "UPDATE workflow_steps SET phase='retry' WHERE scope=?1 AND instance_id=?2 AND node_id=?3",
                    params![scope, id, node],
                )?;
                transition(c, &scope, &id, row.revision, now, &Next::running(Some(&node)))?;
                log(c, &scope, &id, now, Some(&node), "resumed", None, None, json!({}))
            })
            .await?;
        Ok(Status::Running)
    }

    /// Reconciles a blocked non-idempotent step after the host inspected the world:
    /// `Some(output)` records that the effect happened, `None` that it did not, in
    /// which case the next advance executes it again with the same key.
    pub async fn reconcile(&self, instance_id: &str, applied: Option<Value>) -> Result<Status> {
        let lock = self.lock(instance_id);
        let _guard = lock.lock().await;
        let (row, _) = self.state(instance_id).await?;
        let definition = self.pinned(&row).await?;
        if row.status != "blocked" {
            return Err(Error::NotAllowed("instance is not blocked"));
        }
        let node = row
            .cursor
            .clone()
            .ok_or(Error::NotAllowed("instance is not blocked"))?;
        let next = definition
            .node(&node)
            .and_then(|n| n.kind.successors().first().map(|s| s.to_string()));
        let (scope, id, now) = (
            self.scope.clone(),
            instance_id.to_string(),
            self.clock.now(),
        );
        self.db
            .write(Priority::Interactive, move |c| {
                match &applied {
                    Some(output) => {
                        c.execute(
                            "UPDATE workflow_steps SET phase='done',output=?4,completed_at=?5 \
                             WHERE scope=?1 AND instance_id=?2 AND node_id=?3",
                            params![scope, id, node, output.to_string(), now],
                        )?;
                        transition(c, &scope, &id, row.revision, now, &Next::running(next.as_deref()))?;
                    }
                    None => {
                        c.execute(
                            "UPDATE workflow_steps SET phase='retry' WHERE scope=?1 AND instance_id=?2 AND node_id=?3",
                            params![scope, id, node],
                        )?;
                        transition(c, &scope, &id, row.revision, now, &Next::running(Some(&node)))?;
                    }
                }
                log(c, &scope, &id, now, Some(&node), "reconciled", None, None,
                    json!({"applied": applied.is_some()}))
            })
            .await?;
        Ok(self.snapshot(instance_id).await?.status)
    }

    /// Explicit migration of a running instance to another published version. Only
    /// allowed when the current node and every recorded step exist with the same kind.
    pub async fn migrate(&self, instance_id: &str, to_version: u32) -> Result<()> {
        let lock = self.lock(instance_id);
        let _guard = lock.lock().await;
        let (row, steps) = self.state(instance_id).await?;
        let current = self.pinned(&row).await?;
        if matches!(
            row.status.as_str(),
            "completed" | "aborted" | "compensating"
        ) || steps
            .iter()
            .any(|s| matches!(s.phase.as_str(), "compensating" | "compensated"))
        {
            return Err(Error::NotAllowed(
                "terminal or aborting instances cannot migrate",
            ));
        }
        let target = self.definition(&row.workflow_id, to_version).await?;
        let compatible = |node: &str| match (current.node(node), target.node(node)) {
            (Some(a), Some(b)) => a.kind.type_name() == b.kind.type_name(),
            _ => false,
        };
        if row.cursor.as_deref().is_some_and(|c| !compatible(c))
            || steps.iter().any(|s| !compatible(&s.node_id))
        {
            return Err(Error::NotAllowed(
                "target version lacks the current node or a recorded step",
            ));
        }
        let hash = target.canonical().map_err(Error::Invalid)?.1;
        let (scope, id, now, from) = (
            self.scope.clone(),
            instance_id.to_string(),
            self.clock.now(),
            row.version,
        );
        self.db
            .write(Priority::Journal, move |c| {
                let changed = c.execute(
                    "UPDATE workflow_instances SET version=?4,definition_hash=?5,revision=revision+1,updated_at=?6 \
                     WHERE scope=?1 AND id=?2 AND revision=?3",
                    params![scope, id, row.revision, to_version, hash, now],
                )?;
                if changed != 1 {
                    return Err(excalibur_store::Error::Invalid(CONFLICT));
                }
                log(c, &scope, &id, now, None, "migrated", None, None,
                    json!({"from": from, "to": to_version, "hash": hash}))
            })
            .await?;
        Ok(())
    }

    fn lock(&self, instance_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        instance_lock(self.db.path().to_path_buf(), &self.scope, instance_id)
    }

    async fn state(&self, instance_id: &str) -> Result<(Row, Vec<StepRecord>)> {
        let (scope, id) = (self.scope.clone(), instance_id.to_string());
        self.db
            .read(ReadLane::Interactive, move |c| {
                match load_row(c, &scope, &id)? {
                    Some(row) => Ok(Some((row, load_steps(c, &scope, &id)?))),
                    None => Ok(None),
                }
            })
            .await?
            .ok_or(Error::NotFound)
    }

    async fn pinned(&self, row: &Row) -> Result<WorkflowDefinition> {
        let definition = self.definition(&row.workflow_id, row.version).await?;
        if definition.canonical().map_err(Error::Invalid)?.1 != row.hash {
            return Err(Error::DefinitionChanged);
        }
        Ok(definition)
    }

    async fn write<T: Send + 'static>(
        &self,
        job: impl FnOnce(&Connection, &str) -> SResult<T> + Send + 'static,
    ) -> Result<T> {
        let scope = self.scope.clone();
        Ok(self
            .db
            .write(Priority::Journal, move |c| job(c, &scope))
            .await?)
    }

    async fn drive(&self, instance_id: &str, driver: Driver<'_>) -> Result<Status> {
        let (row, _) = self.state(instance_id).await?;
        if row.mode != driver.mode() {
            return Err(Error::ModeMismatch);
        }
        let definition = self.pinned(&row).await?;
        let id = instance_id.to_string();
        // Each node is visited at most once per instance, plus waits and retries.
        for _ in 0..(definition.nodes.len() * 64 + 64) {
            let (row, steps) = self.state(instance_id).await?;
            let now = self.clock.now();
            match row.status.as_str() {
                "running" => {}
                "waiting" => {
                    let until = row.wake_at.unwrap_or(0);
                    if now < until {
                        return Ok(Status::Waiting { until });
                    }
                    let id = id.clone();
                    self.write(move |c, scope| {
                        transition(
                            c,
                            scope,
                            &id,
                            row.revision,
                            now,
                            &Next::running(row.cursor.as_deref()),
                        )?;
                        log(
                            c,
                            scope,
                            &id,
                            now,
                            row.cursor.as_deref(),
                            "woke",
                            None,
                            None,
                            json!({"until": until}),
                        )
                    })
                    .await?;
                    continue;
                }
                "compensating" => {
                    return self
                        .compensate(&id, &row, &steps, &definition, &driver)
                        .await;
                }
                _ => return Ok(row.status()),
            }
            let Some(cursor) = row.cursor.clone() else {
                return Ok(Status::Completed);
            };
            let node = definition
                .node(&cursor)
                .ok_or_else(|| Error::Invalid(format!("cursor {cursor} not in pinned definition")))?
                .clone();
            let step = steps.iter().find(|s| s.node_id == cursor).cloned();
            let outputs = outputs_of(&steps);
            let context = json!({"input": row.input, "outputs": outputs});
            match &node.kind {
                NodeKind::Condition {
                    pointer,
                    equals,
                    then,
                    otherwise,
                } => {
                    let matched = context.pointer(pointer) == Some(equals);
                    let target = if matched {
                        then.clone()
                    } else {
                        otherwise.clone()
                    };
                    let (id, key) = (id.clone(), self.idempotency_key(&id, &cursor));
                    self.write(move |c, scope| {
                        insert_step(
                            c,
                            scope,
                            &id,
                            &cursor,
                            &key,
                            "done",
                            0,
                            Some(&json!({"matched": matched, "next": target})),
                            None,
                            now,
                        )?;
                        transition(
                            c,
                            scope,
                            &id,
                            row.revision,
                            now,
                            &Next::running(Some(&target)),
                        )?;
                        log(
                            c,
                            scope,
                            &id,
                            now,
                            Some(&cursor),
                            "condition",
                            None,
                            None,
                            json!({"matched": matched, "next": target}),
                        )
                    })
                    .await?;
                }
                NodeKind::Wait { seconds, next } => {
                    let (id, key, next) =
                        (id.clone(), self.idempotency_key(&id, &cursor), next.clone());
                    let simulated = driver.mode() == Mode::DryRun;
                    let seconds = i64::try_from(*seconds).unwrap_or(i64::MAX);
                    self.write(move |c, scope| match step {
                        None if !simulated => {
                            let until = now.saturating_add(seconds);
                            insert_step(
                                c,
                                scope,
                                &id,
                                &cursor,
                                &key,
                                "waiting",
                                0,
                                None,
                                Some(until),
                                now,
                            )?;
                            transition(
                                c,
                                scope,
                                &id,
                                row.revision,
                                now,
                                &Next {
                                    status: "waiting",
                                    cursor: Some(&cursor),
                                    wake_at: Some(until),
                                    class: None,
                                    reason: None,
                                },
                            )?;
                            log(
                                c,
                                scope,
                                &id,
                                now,
                                Some(&cursor),
                                "wait",
                                None,
                                None,
                                json!({"until": until}),
                            )
                        }
                        _ => {
                            if step.is_none() {
                                insert_step(
                                    c,
                                    scope,
                                    &id,
                                    &cursor,
                                    &key,
                                    "waiting",
                                    0,
                                    None,
                                    Some(now),
                                    now,
                                )?;
                            }
                            finish_step(
                                c,
                                scope,
                                &id,
                                &cursor,
                                &json!({"waited": !simulated}),
                                now,
                            )?;
                            transition(
                                c,
                                scope,
                                &id,
                                row.revision,
                                now,
                                &Next::running(next.as_deref()),
                            )?;
                            log(
                                c,
                                scope,
                                &id,
                                now,
                                Some(&cursor),
                                "wait_done",
                                None,
                                None,
                                json!({"simulated": simulated}),
                            )
                        }
                    })
                    .await?;
                }
                NodeKind::Approval { prompt, next } => {
                    let preset = match &driver {
                        Driver::Simulated(s) => s.approvals.get(&cursor).copied(),
                        Driver::Live(_) => None,
                    };
                    let (id, key, next, prompt) = (
                        id.clone(),
                        self.idempotency_key(&id, &cursor),
                        next.clone(),
                        prompt.clone(),
                    );
                    let waiting = self
                        .write(move |c, scope| {
                            let decision: Option<(Option<bool>, Option<String>)> = c
                                .query_row(
                                    "SELECT approved,decided_by FROM workflow_approvals \
                                     WHERE scope=?1 AND instance_id=?2 AND node_id=?3",
                                    params![scope, id, cursor],
                                    |r| Ok((r.get(0)?, r.get(1)?)),
                                )
                                .optional()?;
                            let decision = match (decision, preset) {
                                (None, preset) => {
                                    c.execute(
                                        "INSERT INTO workflow_approvals(scope,instance_id,node_id,prompt,requested_at,\
                                         approved,decided_by,decided_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                                        params![scope, id, cursor, prompt, now, preset,
                                            preset.map(|_| "dry_run_scenario"), preset.map(|_| now)],
                                    )?;
                                    log(c, scope, &id, now, Some(&cursor), "approval_requested", None, None,
                                        json!({"prompt": prompt, "simulated_decision": preset}))?;
                                    preset.map(|p| (p, "dry_run_scenario".to_string()))
                                }
                                (Some((Some(p), by)), _) => Some((p, by.unwrap_or_default())),
                                (Some((None, _)), _) => None,
                            };
                            let Some((approved, by)) = decision else {
                                transition(c, scope, &id, row.revision, now, &Next {
                                    status: "waiting_approval",
                                    cursor: Some(&cursor),
                                    wake_at: None,
                                    class: None,
                                    reason: None,
                                })?;
                                return Ok(true);
                            };
                            let output = json!({"approved": approved, "decided_by": by});
                            insert_step(c, scope, &id, &cursor, &key, "done", 0, Some(&output), None, now)?;
                            if approved {
                                transition(c, scope, &id, row.revision, now, &Next::running(next.as_deref()))?;
                            } else {
                                let reason = format!("approval {cursor} rejected by {by}");
                                transition(c, scope, &id, row.revision, now, &Next {
                                    status: "compensating",
                                    cursor: Some(&cursor),
                                    wake_at: None,
                                    class: None,
                                    reason: Some(&reason),
                                })?;
                            }
                            log(c, scope, &id, now, Some(&cursor), "approval_applied", None, None, output)?;
                            Ok(false)
                        })
                        .await?;
                    if waiting {
                        return Ok(Status::WaitingApproval { node: node.id });
                    }
                }
                NodeKind::Tool {
                    tool,
                    input,
                    effect,
                    dry_run,
                    retry,
                    next,
                    ..
                } => {
                    let key = self.idempotency_key(&id, &cursor);
                    let attempt = match step.as_ref().map(|s| s.phase.as_str()) {
                        None => {
                            let (id, cursor, key) = (id.clone(), cursor.clone(), key.clone());
                            self.write(move |c, scope| {
                                insert_step(
                                    c, scope, &id, &cursor, &key, "intent", 1, None, None, now,
                                )?;
                                transition(
                                    c,
                                    scope,
                                    &id,
                                    row.revision,
                                    now,
                                    &Next::running(Some(&cursor)),
                                )?;
                                log(
                                    c,
                                    scope,
                                    &id,
                                    now,
                                    Some(&cursor),
                                    "attempt",
                                    Some(&key),
                                    Some(1),
                                    json!({}),
                                )
                            })
                            .await?;
                            1
                        }
                        Some("retry") => {
                            let attempt = step.as_ref().map_or(1, |s| s.attempts + 1);
                            let (id, cursor, key) = (id.clone(), cursor.clone(), key.clone());
                            self.write(move |c, scope| {
                                c.execute(
                                    "UPDATE workflow_steps SET phase='intent',attempts=?4 \
                                     WHERE scope=?1 AND instance_id=?2 AND node_id=?3",
                                    params![scope, id, cursor, attempt],
                                )?;
                                transition(
                                    c,
                                    scope,
                                    &id,
                                    row.revision,
                                    now,
                                    &Next::running(Some(&cursor)),
                                )?;
                                log(
                                    c,
                                    scope,
                                    &id,
                                    now,
                                    Some(&cursor),
                                    "attempt",
                                    Some(&key),
                                    Some(attempt),
                                    json!({}),
                                )
                            })
                            .await?;
                            attempt
                        }
                        Some("intent") => {
                            // The previous process stopped between intent and result.
                            let attempt = step.as_ref().map_or(1, |s| s.attempts);
                            let effect = *effect;
                            let uncertain =
                                driver.mode() == Mode::Live && effect == Effect::NonIdempotent;
                            let (id2, cursor2, key2) = (id.clone(), cursor.clone(), key.clone());
                            self.write(move |c, scope| {
                                if uncertain {
                                    let reason =
                                        "interrupted non-idempotent effect requires reconciliation";
                                    transition(
                                        c,
                                        scope,
                                        &id2,
                                        row.revision,
                                        now,
                                        &Next {
                                            status: "blocked",
                                            cursor: Some(&cursor2),
                                            wake_at: None,
                                            class: None,
                                            reason: Some(reason),
                                        },
                                    )?;
                                    log(
                                        c,
                                        scope,
                                        &id2,
                                        now,
                                        Some(&cursor2),
                                        "uncertain_effect",
                                        Some(&key2),
                                        Some(attempt),
                                        json!({}),
                                    )
                                } else {
                                    transition(
                                        c,
                                        scope,
                                        &id2,
                                        row.revision,
                                        now,
                                        &Next::running(Some(&cursor2)),
                                    )?;
                                    log(
                                        c,
                                        scope,
                                        &id2,
                                        now,
                                        Some(&cursor2),
                                        "replay_after_interruption",
                                        Some(&key2),
                                        Some(attempt),
                                        json!({"effect": effect}),
                                    )
                                }
                            })
                            .await?;
                            if uncertain {
                                continue;
                            }
                            attempt
                        }
                        Some(other) => {
                            return Err(Error::Invalid(format!(
                                "tool step {cursor} in unexpected phase {other} while running"
                            )));
                        }
                    };
                    let request = EffectRequest {
                        instance_id: id.clone(),
                        node_id: cursor.clone(),
                        tool: tool.clone(),
                        input: input.clone(),
                        context,
                        idempotency_key: key.clone(),
                        attempt,
                        compensation: false,
                    };
                    let result = driver.call(&request, dry_run.as_ref()).await;
                    let (row, _) = self.state(instance_id).await?;
                    let now = self.clock.now();
                    let (id, next, retry, simulated) = (
                        id.clone(),
                        next.clone(),
                        retry.clone(),
                        driver.mode() == Mode::DryRun,
                    );
                    self.write(move |c, scope| match result {
                        Ok(output) => {
                            finish_step(c, scope, &id, &cursor, &output, now)?;
                            transition(
                                c,
                                scope,
                                &id,
                                row.revision,
                                now,
                                &Next::running(next.as_deref()),
                            )?;
                            log(
                                c,
                                scope,
                                &id,
                                now,
                                Some(&cursor),
                                "result",
                                Some(&key),
                                Some(attempt),
                                json!({"simulated": simulated}),
                            )
                        }
                        Err(failure) => record_failure(
                            c,
                            scope,
                            &id,
                            &cursor,
                            &key,
                            attempt,
                            &retry,
                            failure,
                            row.revision,
                            now,
                            simulated,
                        ),
                    })
                    .await?;
                }
            }
        }
        Ok(self.snapshot(instance_id).await?.status)
    }

    async fn compensate(
        &self,
        id: &str,
        row: &Row,
        steps: &[StepRecord],
        definition: &WorkflowDefinition,
        driver: &Driver<'_>,
    ) -> Result<Status> {
        let outputs = outputs_of(steps);
        // Reverse start order. Started-but-unfinished steps are compensated too, because
        // their effect may have happened; compensations must therefore be idempotent.
        let mut pending: Vec<&StepRecord> = steps
            .iter()
            .filter(|s| s.attempts > 0 && s.phase != "compensated")
            .collect();
        pending.sort_by_key(|s| std::cmp::Reverse(s.seq));
        for step in pending {
            let Some(NodeKind::Tool {
                compensation: Some(comp),
                ..
            }) = definition.node(&step.node_id).map(|n| &n.kind)
            else {
                continue;
            };
            let key = format!("{}:compensate", step.idempotency_key);
            let now = self.clock.now();
            let (id2, node, key2) = (id.to_string(), step.node_id.clone(), key.clone());
            let attempt = self
                .write(move |c, scope| {
                    c.execute(
                        "UPDATE workflow_steps SET phase='compensating' WHERE scope=?1 AND instance_id=?2 AND node_id=?3",
                        params![scope, id2, node],
                    )?;
                    let attempt = 1 + c.query_row(
                        "SELECT count(*) FROM workflow_log WHERE scope=?1 AND instance_id=?2 AND node_id=?3 \
                         AND kind='compensation_started'",
                        params![scope, id2, node],
                        |r| r.get::<_, u32>(0),
                    )?;
                    log(c, scope, &id2, now, Some(&node), "compensation_started", Some(&key2), Some(attempt), json!({}))?;
                    Ok(attempt)
                })
                .await?;
            let request = EffectRequest {
                instance_id: id.to_string(),
                node_id: step.node_id.clone(),
                tool: comp.tool.clone(),
                input: comp.input.clone(),
                context: json!({"input": row.input, "outputs": outputs}),
                idempotency_key: key.clone(),
                attempt,
                compensation: true,
            };
            let result = driver.call(&request, comp.dry_run.as_ref()).await;
            let now = self.clock.now();
            let (current, _) = self.state(id).await?;
            let (id2, node, simulated) = (
                id.to_string(),
                step.node_id.clone(),
                driver.mode() == Mode::DryRun,
            );
            let failed = self
                .write(move |c, scope| match result {
                    Ok(output) => {
                        c.execute(
                            "UPDATE workflow_steps SET phase='compensated' WHERE scope=?1 AND instance_id=?2 AND node_id=?3",
                            params![scope, id2, node],
                        )?;
                        log(c, scope, &id2, now, Some(&node), "compensated", Some(&key), Some(attempt),
                            json!({"simulated": simulated, "output": output}))?;
                        Ok(None)
                    }
                    Err(f) => {
                        let reason = format!("compensation of {node} failed: {}", f.message);
                        transition(c, scope, &id2, current.revision, now, &Next {
                            status: "paused",
                            cursor: current.cursor.as_deref(),
                            wake_at: None,
                            class: Some(f.class.as_str()),
                            reason: Some(&reason),
                        })?;
                        log(c, scope, &id2, now, Some(&node), "compensation_failed", Some(&key), Some(attempt),
                            json!({"class": f.class, "message": f.message}))?;
                        Ok(Some(Status::Paused { class: Some(f.class), reason }))
                    }
                })
                .await?;
            if let Some(status) = failed {
                return Ok(status);
            }
        }
        let (current, _) = self.state(id).await?;
        let reason = current.reason.clone().unwrap_or_else(|| "aborted".into());
        let (id2, now, stored) = (id.to_string(), self.clock.now(), reason.clone());
        self.write(move |c, scope| {
            transition(
                c,
                scope,
                &id2,
                current.revision,
                now,
                &Next {
                    status: "aborted",
                    cursor: current.cursor.as_deref(),
                    wake_at: None,
                    class: None,
                    reason: Some(&stored),
                },
            )?;
            log(
                c,
                scope,
                &id2,
                now,
                None,
                "aborted",
                None,
                None,
                json!({"reason": stored}),
            )
        })
        .await?;
        Ok(Status::Aborted { reason })
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_step(
    c: &Connection,
    scope: &str,
    id: &str,
    node: &str,
    key: &str,
    phase: &str,
    attempts: u32,
    output: Option<&Value>,
    wake_at: Option<i64>,
    now: i64,
) -> SResult<()> {
    let seq: i64 = c.query_row(
        "UPDATE workflow_instances SET next_seq=next_seq+1 WHERE scope=?1 AND id=?2 RETURNING next_seq-1",
        params![scope, id],
        |r| r.get(0),
    )?;
    let done = phase == "done";
    c.execute(
        "INSERT INTO workflow_steps(scope,instance_id,node_id,seq,phase,idempotency_key,attempts,output,\
         wake_at,started_at,completed_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            scope,
            id,
            node,
            seq,
            phase,
            key,
            attempts,
            output.map(Value::to_string),
            wake_at,
            now,
            done.then_some(now)
        ],
    )?;
    Ok(())
}

fn finish_step(
    c: &Connection,
    scope: &str,
    id: &str,
    node: &str,
    output: &Value,
    now: i64,
) -> SResult<()> {
    c.execute(
        "UPDATE workflow_steps SET phase='done',output=?4,completed_at=?5 \
         WHERE scope=?1 AND instance_id=?2 AND node_id=?3",
        params![scope, id, node, output.to_string(), now],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_failure(
    c: &Connection,
    scope: &str,
    id: &str,
    node: &str,
    key: &str,
    attempt: u32,
    policy: &RetryPolicy,
    failure: StepFailure,
    revision: i64,
    now: i64,
    simulated: bool,
) -> SResult<()> {
    let message: String = failure.message.chars().take(2048).collect();
    let detail = json!({"class": failure.class, "message": message,
        "retry_after": failure.retry_after, "simulated": simulated});
    log(
        c,
        scope,
        id,
        now,
        Some(node),
        "failure",
        Some(key),
        Some(attempt),
        detail,
    )?;
    let set_phase = |phase: &str, logical: u32| {
        c.execute(
            "UPDATE workflow_steps SET phase=?4,logical_failures=logical_failures+?5 \
             WHERE scope=?1 AND instance_id=?2 AND node_id=?3",
            params![scope, id, node, phase, logical],
        )
    };
    let class = failure.class.as_str();
    match failure.class {
        FailureClass::Transient => {
            set_phase("retry", 0)?;
            match policy.transient_delay(key, attempt, failure.retry_after) {
                Some(delay) => {
                    let until = now.saturating_add(i64::try_from(delay).unwrap_or(i64::MAX));
                    transition(
                        c,
                        scope,
                        id,
                        revision,
                        now,
                        &Next {
                            status: "waiting",
                            cursor: Some(node),
                            wake_at: Some(until),
                            class: Some(class),
                            reason: Some(&message),
                        },
                    )?;
                    log(
                        c,
                        scope,
                        id,
                        now,
                        Some(node),
                        "retry_scheduled",
                        Some(key),
                        Some(attempt + 1),
                        json!({"delay": delay, "until": until}),
                    )
                }
                None => {
                    let reason = format!("transient retries exhausted: {message}");
                    transition(
                        c,
                        scope,
                        id,
                        revision,
                        now,
                        &Next {
                            status: "paused",
                            cursor: Some(node),
                            wake_at: None,
                            class: Some(class),
                            reason: Some(&reason),
                        },
                    )?;
                    log(
                        c,
                        scope,
                        id,
                        now,
                        Some(node),
                        "notice",
                        Some(key),
                        Some(attempt),
                        json!({"notify_user": true, "reason": reason}),
                    )
                }
            }
        }
        FailureClass::Input | FailureClass::Logical => {
            set_phase("failed", u32::from(failure.class == FailureClass::Logical))?;
            transition(
                c,
                scope,
                id,
                revision,
                now,
                &Next {
                    status: "failed",
                    cursor: Some(node),
                    wake_at: None,
                    class: Some(class),
                    reason: Some(&message),
                },
            )
        }
        FailureClass::Permanent => {
            set_phase("failed", 0)?;
            transition(
                c,
                scope,
                id,
                revision,
                now,
                &Next {
                    status: "paused",
                    cursor: Some(node),
                    wake_at: None,
                    class: Some(class),
                    reason: Some(&message),
                },
            )?;
            log(
                c,
                scope,
                id,
                now,
                Some(node),
                "notice",
                Some(key),
                Some(attempt),
                json!({"notify_user": true, "reason": message}),
            )
        }
    }
}

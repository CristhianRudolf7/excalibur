//! Durable capability tasks (§14.1 long-operation lifecycle).
//!
//! The durable task identifier and every intermediate decision are persisted
//! in the principal database before and after each remote call:
//!
//! 1. a *claim* transaction picks the next action (start, deliver input,
//!    cancel or poll), counts start attempts and leases the task;
//! 2. the provider is called outside any transaction;
//! 3. an *apply* transaction records the outcome, guarded by the claim's row
//!    version so a stale driver cannot overwrite newer state.
//!
//! A crash between 1 and 3 leaves the claim visible; [`CapabilityTasks::recover_interrupted`]
//! releases it at host startup and the next drive resends the same keyed
//! action. Effects are therefore delivered at least once to the remote and
//! counted once by a provider that honours the idempotency key.
use crate::{
    Error, Result,
    manifest::CapabilityManifest,
    provider::{
        CapabilityProvider, FailureClass, ProviderError, RemoteState, RemoteStatus, StartRequest,
    },
};
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, rusqlite};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Working,
    InputRequired,
    Completed,
    Failed,
    Cancelled,
}
impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::InputRequired => "input_required",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    fn parse(value: &str) -> excalibur_store::Result<Self> {
        Ok(match value {
            "working" => Self::Working,
            "input_required" => Self::InputRequired,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return Err(corrupt()),
        })
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CancelState {
    None,
    /// Recorded durably, not yet acknowledged by the remote.
    Requested,
    /// Delivered; the remote may still not honour it, so polling continues.
    Sent,
}
impl CancelState {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Requested => "requested",
            Self::Sent => "sent",
        }
    }
    fn parse(value: &str) -> excalibur_store::Result<Self> {
        Ok(match value {
            "none" => Self::None,
            "requested" => Self::Requested,
            "sent" => Self::Sent,
            _ => return Err(corrupt()),
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskFailure {
    pub class: FailureClass,
    pub message: String,
    /// The start may have reached the remote without the harness learning its
    /// task identifier; a host must not silently resubmit under a new key.
    pub effect_uncertain: bool,
}
/// Content produced outside the harness. It is always untrusted (§20.8).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalOutput {
    pub capability: String,
    pub value: Value,
    pub untrusted: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityTask {
    pub id: String,
    pub operation_key: String,
    pub capability: String,
    pub capability_version: String,
    pub operation: String,
    pub state: TaskState,
    pub remote_id: Option<String>,
    pub start_attempts: u32,
    /// What the capability needs from the user while `input_required`.
    pub input_request: Option<ExternalOutput>,
    /// Generation of the current input request; answers must name it.
    pub input_seq: u64,
    pub input_pending: bool,
    pub cancel: CancelState,
    /// A cancellation was requested but the remote finished the work anyway.
    pub cancel_unhonored: bool,
    pub result: Option<ExternalOutput>,
    pub failure: Option<TaskFailure>,
    pub consecutive_failures: u32,
    pub in_flight: bool,
    pub next_poll_at: i64,
    pub deadline_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub version: i64,
    /// Identifies the latest claimed remote call; only its outcome applies.
    pub claim_seq: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Submission {
    /// Host-supplied operation key; the durable task ID is derived from it and
    /// the authenticated scope, so a retried submission maps to the same task.
    pub operation_key: String,
    pub capability: String,
    pub capability_version: String,
    pub operation: String,
    pub input: Value,
    pub timeout_ms: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskEvent {
    pub seq: i64,
    pub task_id: String,
    pub at: i64,
    pub kind: String,
    pub detail: Value,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollPolicy {
    pub default_interval_ms: u64,
    pub min_interval_ms: u64,
    pub max_interval_ms: u64,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    /// Consecutive transient errors tolerated before the task fails.
    pub max_transient_failures: u32,
    /// How long a claimed remote call excludes other drivers.
    pub in_flight_lease_ms: u64,
}
impl Default for PollPolicy {
    fn default() -> Self {
        Self {
            default_interval_ms: 5_000,
            min_interval_ms: 500,
            max_interval_ms: 300_000,
            backoff_base_ms: 1_000,
            backoff_max_ms: 300_000,
            max_transient_failures: 8,
            in_flight_lease_ms: 60_000,
        }
    }
}
impl PollPolicy {
    fn validate(&self) -> Result<()> {
        let bounded = |v: u64| v > 0 && v <= 86_400_000;
        if !(bounded(self.default_interval_ms)
            && bounded(self.min_interval_ms)
            && bounded(self.max_interval_ms)
            && bounded(self.backoff_base_ms)
            && bounded(self.backoff_max_ms)
            && bounded(self.in_flight_lease_ms))
            || self.min_interval_ms > self.max_interval_ms
            || self.backoff_base_ms > self.backoff_max_ms
            || self.max_transient_failures > 64
        {
            return Err(Error::Invalid("poll policy bounds"));
        }
        Ok(())
    }
    fn interval(&self, suggested: Option<u64>) -> i64 {
        suggested
            .unwrap_or(self.default_interval_ms)
            .clamp(self.min_interval_ms, self.max_interval_ms) as i64
    }
    /// Exponential backoff for the n-th consecutive transient error (n >= 1).
    pub fn backoff(&self, failures: u32) -> i64 {
        let factor = 1u64 << failures.saturating_sub(1).min(32);
        self.backoff_base_ms
            .saturating_mul(factor)
            .min(self.backoff_max_ms) as i64
    }
}
/// Milliseconds since the Unix epoch. Tests inject [`ManualClock`].
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default()
    }
}
#[derive(Default)]
pub struct ManualClock(AtomicI64);
impl ManualClock {
    pub fn new(now_ms: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(now_ms)))
    }
    pub fn set(&self, now_ms: i64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}
impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

const MAX_KEY: usize = 256;
const MAX_JSON: usize = 1 << 20;
const MAX_MESSAGE: usize = 512;
const COLUMNS: &str = "id,operation_key,capability,capability_version,operation,input,state,remote_id,start_attempts,input_request,input_seq,pending_input,cancel_state,cancel_unhonored,result,failure,failures,in_flight,next_poll_at,deadline_at,created_at,updated_at,version,claim_seq";

fn corrupt() -> excalibur_store::Error {
    excalibur_store::Error::Invalid("invalid persisted capability task")
}
fn encode<T: Serialize>(value: &T) -> excalibur_store::Result<String> {
    serde_json::to_string(value).map_err(|_| corrupt())
}
fn decode<T: for<'de> Deserialize<'de>>(value: &str) -> excalibur_store::Result<T> {
    serde_json::from_str(value).map_err(|_| corrupt())
}
fn bounded_message(message: &str) -> String {
    let mut end = message.len().min(MAX_MESSAGE);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_string()
}
fn json_size(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}
/// Durable task ID: stable across restarts and resubmissions of the same key.
pub fn task_id(scope: &str, operation_key: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(scope.as_bytes());
    hash.update([0]);
    hash.update(operation_key.as_bytes());
    let digest = format!("{:x}", hash.finalize());
    format!("cap_{}", &digest[..32])
}

#[derive(Clone)]
struct Row {
    task: CapabilityTask,
    input: Value,
    pending_input: Option<Value>,
}
fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        id: r.get(0)?,
        operation_key: r.get(1)?,
        capability: r.get(2)?,
        capability_version: r.get(3)?,
        operation: r.get(4)?,
        input: r.get(5)?,
        state: r.get(6)?,
        remote_id: r.get(7)?,
        start_attempts: r.get(8)?,
        input_request: r.get(9)?,
        input_seq: r.get(10)?,
        pending_input: r.get(11)?,
        cancel_state: r.get(12)?,
        cancel_unhonored: r.get(13)?,
        result: r.get(14)?,
        failure: r.get(15)?,
        failures: r.get(16)?,
        in_flight: r.get(17)?,
        next_poll_at: r.get(18)?,
        deadline_at: r.get(19)?,
        created_at: r.get(20)?,
        updated_at: r.get(21)?,
        version: r.get(22)?,
        claim_seq: r.get(23)?,
    })
}
struct RawRow {
    id: String,
    operation_key: String,
    capability: String,
    capability_version: String,
    operation: String,
    input: String,
    state: String,
    remote_id: Option<String>,
    start_attempts: i64,
    input_request: Option<String>,
    input_seq: i64,
    pending_input: Option<String>,
    cancel_state: String,
    cancel_unhonored: i64,
    result: Option<String>,
    failure: Option<String>,
    failures: i64,
    in_flight: i64,
    next_poll_at: i64,
    deadline_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
    version: i64,
    claim_seq: i64,
}
impl RawRow {
    fn into_row(self) -> excalibur_store::Result<Row> {
        let external = |value: Option<String>| -> excalibur_store::Result<Option<ExternalOutput>> {
            value
                .map(|v| {
                    Ok(ExternalOutput {
                        capability: self.capability.clone(),
                        value: decode(&v)?,
                        untrusted: true,
                    })
                })
                .transpose()
        };
        let input_request = external(self.input_request.clone())?;
        let result = external(self.result.clone())?;
        Ok(Row {
            input: decode(&self.input)?,
            pending_input: self.pending_input.as_deref().map(decode).transpose()?,
            task: CapabilityTask {
                state: TaskState::parse(&self.state)?,
                cancel: CancelState::parse(&self.cancel_state)?,
                failure: self.failure.as_deref().map(decode).transpose()?,
                input_pending: self.pending_input.is_some(),
                input_request,
                result,
                id: self.id,
                operation_key: self.operation_key,
                capability: self.capability,
                capability_version: self.capability_version,
                operation: self.operation,
                remote_id: self.remote_id,
                start_attempts: u32::try_from(self.start_attempts).map_err(|_| corrupt())?,
                input_seq: u64::try_from(self.input_seq).map_err(|_| corrupt())?,
                cancel_unhonored: self.cancel_unhonored != 0,
                consecutive_failures: u32::try_from(self.failures).map_err(|_| corrupt())?,
                in_flight: self.in_flight != 0,
                next_poll_at: self.next_poll_at,
                deadline_at: self.deadline_at,
                created_at: self.created_at,
                updated_at: self.updated_at,
                version: self.version,
                claim_seq: u64::try_from(self.claim_seq).map_err(|_| corrupt())?,
            },
        })
    }
}
fn load(c: &Connection, scope: &str, id: &str) -> excalibur_store::Result<Option<Row>> {
    c.query_row(
        &format!("SELECT {COLUMNS} FROM capability_tasks WHERE scope=?1 AND id=?2"),
        params![scope, id],
        read_row,
    )
    .optional()?
    .map(RawRow::into_row)
    .transpose()
}
/// Writes every mutable column if the row still has `row.task.version`.
fn save(c: &Connection, scope: &str, row: &mut Row, now: i64) -> excalibur_store::Result<bool> {
    let t = &row.task;
    let changed = c.execute(
        "UPDATE capability_tasks SET state=?3,remote_id=?4,start_attempts=?5,input_request=?6,input_seq=?7,pending_input=?8,cancel_state=?9,cancel_unhonored=?10,result=?11,failure=?12,failures=?13,in_flight=?14,next_poll_at=?15,deadline_at=?16,updated_at=?17,claim_seq=?19,version=version+1 WHERE scope=?1 AND id=?2 AND version=?18",
        params![
            scope,
            t.id,
            t.state.as_str(),
            t.remote_id,
            t.start_attempts as i64,
            t.input_request.as_ref().map(|o| encode(&o.value)).transpose()?,
            t.input_seq as i64,
            row.pending_input.as_ref().map(encode).transpose()?,
            t.cancel.as_str(),
            t.cancel_unhonored as i64,
            t.result.as_ref().map(|o| encode(&o.value)).transpose()?,
            t.failure.as_ref().map(encode).transpose()?,
            t.consecutive_failures as i64,
            t.in_flight as i64,
            t.next_poll_at,
            t.deadline_at,
            now,
            t.version,
            t.claim_seq as i64,
        ],
    )?;
    if changed == 1 {
        row.task.version += 1;
        row.task.updated_at = now;
        row.task.input_pending = row.pending_input.is_some();
    }
    Ok(changed == 1)
}
fn event(
    c: &Connection,
    scope: &str,
    id: &str,
    at: i64,
    kind: &str,
    detail: Value,
) -> excalibur_store::Result<()> {
    c.execute(
        "INSERT INTO capability_task_events(scope,task_id,at,kind,detail) VALUES(?1,?2,?3,?4,?5)",
        params![scope, id, at, kind, encode(&detail)?],
    )?;
    Ok(())
}
fn load_manifest(
    c: &Connection,
    capability: &str,
    version: &str,
) -> excalibur_store::Result<Option<CapabilityManifest>> {
    c.query_row(
        "SELECT definition FROM capability_manifests WHERE capability=?1 AND version=?2",
        params![capability, version],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|d| decode(&d))
    .transpose()
}

#[derive(Clone, Debug)]
enum Action {
    Start,
    Input(Value),
    Cancel,
    Poll,
}
impl Action {
    fn name(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Input(_) => "input",
            Self::Cancel => "cancel",
            Self::Poll => "poll",
        }
    }
}
enum Claim {
    Idle(Box<Row>),
    Act(Box<Row>, Action, Box<CapabilityManifest>),
}
type Inner<T> = std::result::Result<T, &'static str>;

/// Scoped handle over durable capability tasks. The host supplies `scope`
/// from authenticated authority, never from model or capability content.
#[derive(Clone)]
pub struct CapabilityTasks {
    db: Database,
    scope: String,
    policy: PollPolicy,
    clock: Arc<dyn Clock>,
}
impl CapabilityTasks {
    pub async fn open(
        db: Database,
        scope: String,
        policy: PollPolicy,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        if scope.is_empty() || scope.len() > 512 {
            return Err(Error::Invalid("nonempty bounded scope"));
        }
        policy.validate()?;
        if db.kind() != DatabaseKind::Main {
            return Err(excalibur_store::Error::PrincipalOnly.into());
        }
        db.write(Priority::Journal, |c| {
            c.execute_batch("CREATE TABLE IF NOT EXISTS capability_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL)")?;
            {
                let (version, schema) = (1, include_str!("schema.sql"));
                let checksum = format!("{:x}", Sha256::digest(schema.as_bytes()));
                let known: Option<String> = c
                    .query_row("SELECT checksum FROM capability_schema WHERE version=?1", [version], |r| r.get(0))
                    .optional()?;
                if known.as_ref().is_some_and(|stored| stored != &checksum) {
                    return Err(excalibur_store::Error::Invalid("capability schema checksum mismatch"));
                }
                c.execute_batch(schema)?;
                c.execute("INSERT OR IGNORE INTO capability_schema(version,checksum) VALUES(?1,?2)", params![version, checksum])?;
            }
            Ok(())
        })
        .await?;
        Ok(Self {
            db,
            scope,
            policy,
            clock,
        })
    }
    pub fn policy(&self) -> PollPolicy {
        self.policy
    }
    /// Registers an installed capability manifest. Re-registering the same
    /// identity and version is idempotent only for an identical definition.
    pub async fn register(&self, manifest: CapabilityManifest) -> Result<()> {
        manifest.validate()?;
        let definition = encode(&manifest)?;
        let outcome = self
            .db
            .write(Priority::Journal, move |c| -> excalibur_store::Result<Inner<()>> {
                c.execute(
                    "INSERT OR IGNORE INTO capability_manifests(capability,version,definition) VALUES(?1,?2,?3)",
                    params![manifest.id, manifest.version, definition],
                )?;
                let stored: String = c.query_row(
                    "SELECT definition FROM capability_manifests WHERE capability=?1 AND version=?2",
                    params![manifest.id, manifest.version],
                    |r| r.get(0),
                )?;
                Ok(if stored == definition { Ok(()) } else { Err("manifest version already has another definition") })
            })
            .await?;
        outcome.map_err(Error::Conflict)
    }
    pub async fn manifest(
        &self,
        capability: &str,
        version: &str,
    ) -> Result<Option<CapabilityManifest>> {
        let (capability, version) = (capability.to_string(), version.to_string());
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                load_manifest(c, &capability, &version)
            })
            .await?)
    }
    /// Records a task durably without contacting the capability. The first
    /// [`drive`](Self::drive) sends the keyed start. Resubmitting the same key
    /// with the same definition returns the existing task.
    pub async fn submit(&self, submission: Submission) -> Result<CapabilityTask> {
        if submission.operation_key.is_empty()
            || submission.operation_key.len() > MAX_KEY
            || json_size(&submission.input) > MAX_JSON
            || submission
                .timeout_ms
                .is_some_and(|t| t == 0 || t > 30 * 86_400_000)
        {
            return Err(Error::Invalid(
                "bounded operation key, input and positive timeout",
            ));
        }
        let now = self.clock.now_ms();
        let scope = self.scope.clone();
        let id = task_id(&scope, &submission.operation_key);
        let definition = encode(&submission)?;
        let outcome = self
            .db
            .write(Priority::Journal, move |c| -> excalibur_store::Result<Inner<Row>> {
                let Some(manifest) = load_manifest(c, &submission.capability, &submission.capability_version)? else {
                    return Ok(Err("capability manifest is not registered"));
                };
                if manifest.operation(&submission.operation).is_none() {
                    return Ok(Err("operation is not declared by the manifest"));
                }
                let stored: Option<String> = c
                    .query_row("SELECT definition FROM capability_tasks WHERE scope=?1 AND id=?2", params![scope, id], |r| r.get(0))
                    .optional()?;
                if let Some(stored) = stored {
                    if stored != definition {
                        return Ok(Err("operation key already names another submission"));
                    }
                } else {
                    let deadline = submission.timeout_ms.map(|t| now.saturating_add(t as i64));
                    c.execute(
                        "INSERT INTO capability_tasks(scope,id,operation_key,capability,capability_version,operation,definition,input,state,next_poll_at,deadline_at,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'working',?9,?10,?9,?9)",
                        params![scope, id, submission.operation_key, submission.capability, submission.capability_version, submission.operation, definition, encode(&submission.input)?, now, deadline],
                    )?;
                    event(c, &scope, &id, now, "submitted", json!({"capability": submission.capability, "version": submission.capability_version, "operation": submission.operation}))?;
                }
                load(c, &scope, &id)?.ok_or_else(corrupt).map(Ok)
            })
            .await?;
        Ok(outcome.map_err(Error::Conflict)?.task)
    }
    pub async fn get(&self, id: &str) -> Result<Option<CapabilityTask>> {
        let (scope, id) = (self.scope.clone(), id.to_string());
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| load(c, &scope, &id))
            .await?
            .map(|row| row.task))
    }
    /// Answers the current `input_required` request (§14.1: the host turns it
    /// into `ask_user`/`request_permission`). The answer is persisted first and
    /// delivered by the next drive with a per-request idempotency key. The
    /// answer itself is never written to the audit log.
    pub async fn respond(
        &self,
        id: &str,
        request_seq: u64,
        response: Value,
    ) -> Result<CapabilityTask> {
        if json_size(&response) > MAX_JSON {
            return Err(Error::Invalid("response exceeds 1 MiB"));
        }
        let now = self.clock.now_ms();
        let (scope, id) = (self.scope.clone(), id.to_string());
        let outcome = self
            .db
            .write(
                Priority::Interactive,
                move |c| -> excalibur_store::Result<Inner<Row>> {
                    let Some(mut row) = load(c, &scope, &id)? else {
                        return Ok(Err("not found"));
                    };
                    if row.task.state != TaskState::InputRequired
                        || row.task.input_seq != request_seq
                    {
                        return Ok(Err("no matching input request is outstanding"));
                    }
                    if let Some(pending) = &row.pending_input {
                        return Ok(if pending == &response {
                            Ok(row)
                        } else {
                            Err("another answer is already pending")
                        });
                    }
                    row.pending_input = Some(response);
                    if !row.task.in_flight {
                        row.task.next_poll_at = row.task.next_poll_at.min(now);
                    }
                    if !save(c, &scope, &mut row, now)? {
                        return Err(corrupt());
                    }
                    event(
                        c,
                        &scope,
                        &id,
                        now,
                        "input_recorded",
                        json!({"seq": request_seq}),
                    )?;
                    Ok(Ok(row))
                },
            )
            .await?;
        match outcome {
            Ok(row) => Ok(row.task),
            Err("not found") => Err(Error::NotFound),
            Err(reason) => Err(Error::Conflict(reason)),
        }
    }
    /// Requests cooperative cancellation. It is idempotent; a terminal task is
    /// returned unchanged. The remote may ignore it, so polling continues.
    pub async fn request_cancel(&self, id: &str) -> Result<CapabilityTask> {
        let now = self.clock.now_ms();
        let (scope, id) = (self.scope.clone(), id.to_string());
        let outcome = self
            .db
            .write(
                Priority::Interactive,
                move |c| -> excalibur_store::Result<Option<Row>> {
                    let Some(mut row) = load(c, &scope, &id)? else {
                        return Ok(None);
                    };
                    if row.task.state.is_terminal() || row.task.cancel != CancelState::None {
                        return Ok(Some(row));
                    }
                    row.task.cancel = CancelState::Requested;
                    if !row.task.in_flight {
                        row.task.next_poll_at = row.task.next_poll_at.min(now);
                    }
                    if !save(c, &scope, &mut row, now)? {
                        return Err(corrupt());
                    }
                    event(c, &scope, &id, now, "cancel_requested", json!({}))?;
                    Ok(Some(row))
                },
            )
            .await?;
        outcome.map(|row| row.task).ok_or(Error::NotFound)
    }
    /// Host startup only, before any driver runs: releases claims left by a
    /// process that died during a remote call so the keyed action is resent
    /// immediately instead of after the lease. The principal store is
    /// exclusively owned by one process, so a surviving claim is orphaned.
    pub async fn recover_interrupted(&self) -> Result<usize> {
        let now = self.clock.now_ms();
        let scope = self.scope.clone();
        Ok(self
            .db
            .write(Priority::Journal, move |c| {
                let ids = {
                    let mut q = c.prepare("SELECT id FROM capability_tasks WHERE scope=?1 AND in_flight=1 AND state IN ('working','input_required') ORDER BY id")?;
                    q.query_map([&scope], |r| r.get::<_, String>(0))?
                        .collect::<std::result::Result<Vec<_>, _>>()?
                };
                for id in &ids {
                    let mut row = load(c, &scope, id)?.ok_or_else(corrupt)?;
                    row.task.in_flight = false;
                    row.task.next_poll_at = row.task.next_poll_at.min(now);
                    if !save(c, &scope, &mut row, now)? {
                        return Err(corrupt());
                    }
                    event(c, &scope, id, now, "resumed_after_restart", json!({"remote_id": row.task.remote_id, "start_attempts": row.task.start_attempts}))?;
                }
                Ok(ids.len())
            })
            .await?)
    }
    /// Drives one step of a task: deadline check, then start, deliver a
    /// pending answer, deliver a cancellation, or poll, whichever is due.
    pub async fn drive<P: CapabilityProvider>(
        &self,
        provider: &P,
        id: &str,
    ) -> Result<CapabilityTask> {
        let now = self.clock.now_ms();
        let (scope, task) = (self.scope.clone(), id.to_string());
        let policy = self.policy;
        let claim = self
            .db
            .write(Priority::Journal, move |c| -> excalibur_store::Result<Option<Claim>> {
                let Some(mut row) = load(c, &scope, &task)? else { return Ok(None) };
                let t = &mut row.task;
                if t.state.is_terminal() || (t.in_flight && now < t.next_poll_at) {
                    return Ok(Some(Claim::Idle(Box::new(row))));
                }
                let manifest = load_manifest(c, &t.capability, &t.capability_version)?.ok_or_else(corrupt)?;
                let mut events: Vec<(&str, Value)> = Vec::new();
                if t.in_flight {
                    t.in_flight = false;
                    events.push(("lease_expired", json!({})));
                }
                if t.deadline_at.is_some_and(|d| now >= d) && t.cancel == CancelState::None {
                    t.cancel = CancelState::Requested;
                    t.next_poll_at = now;
                    events.push(("deadline_exceeded", json!({"deadline_at": t.deadline_at})));
                }
                let mut action = None;
                if t.remote_id.is_none() && t.start_attempts == 0 && t.cancel != CancelState::None {
                    t.state = TaskState::Cancelled;
                    t.cancel = CancelState::Sent;
                    events.push(("cancelled_before_start", json!({})));
                } else if now >= t.next_poll_at {
                    let next = if t.remote_id.is_none() {
                        Action::Start
                    } else if let Some(answer) = row.pending_input.clone() {
                        Action::Input(answer)
                    } else if t.cancel == CancelState::Requested {
                        Action::Cancel
                    } else {
                        Action::Poll
                    };
                    let t = &mut row.task;
                    if matches!(next, Action::Start) {
                        let active: i64 = c.query_row(
                            "SELECT COUNT(*) FROM capability_tasks WHERE capability=?1 AND state IN ('working','input_required') AND (remote_id IS NOT NULL OR start_attempts>0) AND NOT (scope=?2 AND id=?3)",
                            params![t.capability, scope, t.id],
                            |r| r.get(0),
                        )?;
                        if active >= manifest.max_concurrency as i64 {
                            t.next_poll_at = now + policy.default_interval_ms as i64;
                            events.push(("deferred_concurrency", json!({"active": active, "limit": manifest.max_concurrency})));
                        } else {
                            t.start_attempts += 1;
                            action = Some(next);
                        }
                    } else {
                        action = Some(next);
                    }
                    if let Some(action) = &action {
                        t.in_flight = true;
                        t.claim_seq += 1;
                        t.next_poll_at = now + policy.in_flight_lease_ms as i64;
                        events.push(("dispatch", json!({"action": action.name(), "start_attempt": t.start_attempts, "input_seq": t.input_seq})));
                    }
                }
                if !events.is_empty() && !save(c, &scope, &mut row, now)? {
                    return Err(corrupt());
                }
                for (kind, detail) in events {
                    event(c, &scope, &task, now, kind, detail)?;
                }
                Ok(Some(match action {
                    Some(action) => Claim::Act(Box::new(row), action, Box::new(manifest)),
                    None => Claim::Idle(Box::new(row)),
                }))
            })
            .await?
            .ok_or(Error::NotFound)?;
        let (row, action, manifest) = match claim {
            Claim::Idle(row) => return Ok(row.task),
            Claim::Act(row, action, manifest) => (*row, action, *manifest),
        };
        let t = &row.task;
        let outcome = match &action {
            Action::Start => {
                provider
                    .start(StartRequest {
                        capability: t.capability.clone(),
                        operation: t.operation.clone(),
                        input: row.input.clone(),
                        idempotency_key: format!("{}:start", t.id),
                    })
                    .await
            }
            Action::Input(answer) => {
                provider
                    .provide_input(
                        t.remote_id.as_deref().unwrap_or_default(),
                        &format!("{}:input:{}", t.id, t.input_seq),
                        answer.clone(),
                    )
                    .await
            }
            Action::Cancel => {
                provider
                    .cancel(
                        t.remote_id.as_deref().unwrap_or_default(),
                        &format!("{}:cancel", t.id),
                    )
                    .await
            }
            Action::Poll => {
                provider
                    .poll(t.remote_id.as_deref().unwrap_or_default())
                    .await
            }
        };
        self.apply(row, action, manifest, outcome).await
    }
    async fn apply(
        &self,
        claimed: Row,
        action: Action,
        manifest: CapabilityManifest,
        outcome: std::result::Result<RemoteStatus, ProviderError>,
    ) -> Result<CapabilityTask> {
        let now = self.clock.now_ms();
        let scope = self.scope.clone();
        let policy = self.policy;
        let applied = self
            .db
            .write(Priority::Journal, move |c| -> excalibur_store::Result<Row> {
                let id = claimed.task.id.clone();
                let mut row = load(c, &scope, &id)?.ok_or_else(corrupt)?;
                if !row.task.in_flight || row.task.claim_seq != claimed.task.claim_seq {
                    // Startup recovery or a driver after lease expiry owns the
                    // task now; the remote call was keyed, so dropping this
                    // outcome is safe.
                    event(c, &scope, &id, now, "stale_outcome_discarded", json!({"action": action.name(), "claim": claimed.task.claim_seq}))?;
                    return Ok(row);
                }
                // The current row may carry a cancellation or an answer that
                // the user recorded while the call was in flight; keep them.
                let before = row.task.state;
                let mut events: Vec<(&str, Value)> = Vec::new();
                let t = &mut row.task;
                t.in_flight = false;
                let fail = |t: &mut CapabilityTask, class, message: &str, uncertain| {
                    t.state = TaskState::Failed;
                    t.failure = Some(TaskFailure { class, message: bounded_message(message), effect_uncertain: uncertain });
                };
                match outcome {
                    Ok(status) => {
                        t.consecutive_failures = 0;
                        events.push(("acknowledged", json!({"action": action.name(), "remote_id": status.remote_id})));
                        let reported = match &status.state {
                            RemoteState::Working => TaskState::Working,
                            RemoteState::InputRequired { .. } => TaskState::InputRequired,
                            RemoteState::Completed { .. } => TaskState::Completed,
                            RemoteState::Failed { .. } => TaskState::Failed,
                            RemoteState::Cancelled => TaskState::Cancelled,
                        };
                        let known = t.remote_id.get_or_insert_with(|| status.remote_id.clone()).clone();
                        match action {
                            Action::Input(_) => row.pending_input = None,
                            Action::Cancel => t.cancel = CancelState::Sent,
                            _ => {}
                        }
                        if known != status.remote_id || status.remote_id.is_empty() {
                            fail(t, FailureClass::Permanent, "capability changed the task identifier", false);
                        } else if !manifest.reports(reported) {
                            fail(t, FailureClass::Permanent, "capability reported an undeclared state", false);
                        } else {
                            match status.state {
                                RemoteState::Working => t.state = TaskState::Working,
                                RemoteState::InputRequired { request } => {
                                    let fresh = t.state != TaskState::InputRequired
                                        || matches!(action, Action::Input(_))
                                        || t.input_request.as_ref().is_none_or(|r| r.value != request);
                                    if json_size(&request) > MAX_JSON {
                                        fail(t, FailureClass::Permanent, "input request exceeds 1 MiB", false);
                                    } else {
                                        if fresh {
                                            // An answer recorded for an older
                                            // request must not be delivered.
                                            if !matches!(action, Action::Input(_)) {
                                                row.pending_input = None;
                                            }
                                            t.input_seq += 1;
                                            events.push(("input_required", json!({"seq": t.input_seq})));
                                        }
                                        t.state = TaskState::InputRequired;
                                        t.input_request = Some(ExternalOutput { capability: t.capability.clone(), value: request, untrusted: true });
                                    }
                                }
                                RemoteState::Completed { result } => {
                                    if json_size(&result) > MAX_JSON {
                                        fail(t, FailureClass::Permanent, "result exceeds 1 MiB", false);
                                    } else {
                                        t.state = TaskState::Completed;
                                        t.result = Some(ExternalOutput { capability: t.capability.clone(), value: result, untrusted: true });
                                        if t.cancel != CancelState::None {
                                            t.cancel_unhonored = true;
                                            events.push(("cancel_not_honored", json!({})));
                                        }
                                    }
                                }
                                RemoteState::Failed { class, message } => fail(t, class, &message, false),
                                RemoteState::Cancelled => t.state = TaskState::Cancelled,
                            }
                        }
                        if t.state != TaskState::InputRequired {
                            t.input_request = None;
                            row.pending_input = None;
                        }
                        t.next_poll_at = now + policy.interval(status.poll_interval_ms);
                    }
                    Err(ProviderError::Transient(message)) => {
                        t.consecutive_failures += 1;
                        if t.consecutive_failures > policy.max_transient_failures {
                            let uncertain = t.remote_id.is_none();
                            fail(t, FailureClass::Transient, "transient failure budget exhausted", uncertain);
                            events.push(("transient_budget_exhausted", json!({"action": action.name(), "message": bounded_message(&message)})));
                        } else {
                            let delay = policy.backoff(t.consecutive_failures);
                            t.next_poll_at = now + delay;
                            events.push(("transient_error", json!({"action": action.name(), "failures": t.consecutive_failures, "retry_in_ms": delay, "message": bounded_message(&message)})));
                        }
                    }
                    Err(ProviderError::Rejected(message)) => {
                        events.push(("rejected", json!({"action": action.name(), "message": bounded_message(&message)})));
                        t.consecutive_failures = 0;
                        t.next_poll_at = now + policy.interval(None);
                        match action {
                            Action::Start | Action::Poll => fail(t, FailureClass::Permanent, &message, false),
                            Action::Input(_) => {
                                // The remote refused this answer: ask again.
                                row.pending_input = None;
                                t.input_seq += 1;
                                events.push(("input_required", json!({"seq": t.input_seq})));
                            }
                            Action::Cancel => t.cancel = CancelState::Sent,
                        }
                    }
                    Err(ProviderError::UnknownTask) => {
                        fail(t, FailureClass::Permanent, "capability no longer knows the task", true);
                    }
                }
                if row.task.state.is_terminal() {
                    row.pending_input = None;
                }
                if row.task.state != before {
                    events.push(("state", json!({"from": before.as_str(), "to": row.task.state.as_str()})));
                }
                if !save(c, &scope, &mut row, now)? {
                    return Err(corrupt());
                }
                for (kind, detail) in events {
                    event(c, &scope, &id, now, kind, detail)?;
                }
                Ok(row)
            })
            .await?;
        Ok(applied.task)
    }
    /// Drives every task due at the current clock, up to `limit`.
    pub async fn drive_due<P: CapabilityProvider>(
        &self,
        provider: &P,
        limit: u32,
    ) -> Result<Vec<CapabilityTask>> {
        if limit == 0 || limit > 1000 {
            return Err(Error::Invalid("limit 1..1000"));
        }
        let now = self.clock.now_ms();
        let scope = self.scope.clone();
        let ids = self
            .db
            .read(ReadLane::Background, move |c| {
                let mut q = c.prepare("SELECT id FROM capability_tasks WHERE scope=?1 AND state IN ('working','input_required') AND (next_poll_at<=?2 OR (deadline_at<=?2 AND cancel_state='none')) ORDER BY next_poll_at,id LIMIT ?3")?;
                Ok(q.query_map(params![scope, now, limit], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        let mut driven = Vec::with_capacity(ids.len());
        for id in ids {
            driven.push(self.drive(provider, &id).await?);
        }
        Ok(driven)
    }
    /// Earliest time at which some nonterminal task needs a drive.
    pub async fn next_wake(&self) -> Result<Option<i64>> {
        let scope = self.scope.clone();
        Ok(self
            .db
            .read(ReadLane::Background, move |c| {
                Ok(c.query_row(
                    "SELECT MIN(CASE WHEN deadline_at IS NOT NULL AND cancel_state='none' AND deadline_at<next_poll_at THEN deadline_at ELSE next_poll_at END) FROM capability_tasks WHERE scope=?1 AND state IN ('working','input_required')",
                    [scope],
                    |r| r.get::<_, Option<i64>>(0),
                )?)
            })
            .await?)
    }
    /// Tasks waiting for the user, for conversion into `ask_user`.
    pub async fn awaiting_input(&self) -> Result<Vec<CapabilityTask>> {
        let scope = self.scope.clone();
        let rows = self
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut q = c.prepare(&format!("SELECT {COLUMNS} FROM capability_tasks WHERE scope=?1 AND state='input_required' AND pending_input IS NULL ORDER BY created_at,id LIMIT 1000"))?;
                let raw = q
                    .query_map([scope], read_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                raw.into_iter().map(RawRow::into_row).collect::<excalibur_store::Result<Vec<_>>>()
            })
            .await?;
        Ok(rows.into_iter().map(|r| r.task).collect())
    }
    /// Audit log of one task in order.
    pub async fn events(&self, id: &str) -> Result<Vec<TaskEvent>> {
        let (scope, id) = (self.scope.clone(), id.to_string());
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut q = c.prepare("SELECT seq,task_id,at,kind,detail FROM capability_task_events WHERE scope=?1 AND task_id=?2 ORDER BY seq LIMIT 10000")?;
                let raw = q
                    .query_map(params![scope, id], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                raw.into_iter()
                    .map(|(seq, task_id, at, kind, detail)| Ok(TaskEvent { seq, task_id, at, kind, detail: decode(&detail)? }))
                    .collect()
            })
            .await?)
    }
}

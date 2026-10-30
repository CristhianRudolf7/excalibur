//! Confirmed append-only execution events on the principal SQLite writer.
pub mod context_memory;
pub mod instructions;
use excalibur_store::{
    Database, Error as StoreError, Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use excalibur_types::{JobId, RunId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock, Weak},
};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("journal storage: {0}")]
    Storage(#[from] StoreError),
    #[error("journal serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("execution does not exist")]
    Missing,
    #[error("invalid journal argument: {0}")]
    Invalid(&'static str),
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub sequence: u64,
    pub kind: String,
    pub key: String,
    pub payload: Value,
}
#[derive(Clone)]
pub struct Journal {
    db: Database,
}
type RunLocks = Mutex<BTreeMap<String, Weak<tokio::sync::Mutex<()>>>>;
static LOCKS: OnceLock<RunLocks> = OnceLock::new();
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
impl Journal {
    pub async fn new(db: Database) -> Result<Self> {
        // Own transactional, checksummed extension of the foundational storage schema.
        let sql = include_str!("schema.sql");
        let checksum = digest(sql.as_bytes());
        db.write(Priority::Journal, move |c| {
            c.execute_batch(sql)?;
            let known: Option<String> = c
                .query_row(
                    "SELECT checksum FROM durable_schema WHERE version=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            match known {
                Some(value) if value != checksum => {
                    return Err(StoreError::Configuration(
                        "durable schema checksum mismatch".into(),
                    ));
                }
                Some(_) => {}
                None => {
                    c.execute("INSERT INTO durable_schema VALUES(1,?1)", [checksum])?;
                }
            }
            Ok(())
        })
        .await?;
        Ok(Self { db })
    }
    pub fn database(&self) -> &Database {
        &self.db
    }
    /// Shared across engine instances; the Database already enforces per-process file ownership.
    pub async fn lock_run(&self, run: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let key = format!("{}:{run}", self.db.path().display());
        let lock = {
            let mut locks = LOCKS.get_or_init(Default::default).lock().unwrap();
            locks.retain(|_, v| v.strong_count() > 0);
            if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
    /// An operation key creates at most one run; a different seed with the same key is rejected.
    pub async fn create_run(&self, operation: &str, seed: Value) -> Result<String> {
        if operation.trim().is_empty() || operation.len() > 512 {
            return Err(Error::Invalid("operation key required"));
        }
        let text = serde_json::to_string(&seed)?;
        if text.len() > 8 * 1024 * 1024 {
            return Err(Error::Invalid("run seed exceeds 8 MiB"));
        }
        let seed_hash = digest(text.as_bytes());
        let operation = operation.to_owned();
        let run = RunId::new().to_string();
        Ok(self.db.write(Priority::Journal,move|c|{
            let known:Option<(String,String)>=c.query_row("SELECT run_id,seed_hash FROM durable_operations WHERE operation_id=?1",[&operation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((run,hash))=known { if hash!=seed_hash{return Err(StoreError::Invalid("operation key reused with different run seed"));}return Ok(run); }
            let initial=if serde_json::from_str::<Value>(&text).is_ok_and(|v|v["initial_state"]=="created"){"created"}else{"running"};
            c.execute("INSERT INTO runs(id,state) VALUES(?1,?2)",rusqlite::params![run,initial])?;
            c.execute("INSERT INTO durable_operations VALUES(?1,?2,?3)",rusqlite::params![operation,run,seed_hash])?;
            c.execute("INSERT INTO journal(id,run_id,sequence,kind,payload,state,idempotency_key) VALUES(?1,?2,1,'run_started',?3,'confirmed','run_started')",rusqlite::params![JobId::new().to_string(),run,text])?;
            Ok(run)
        }).await?)
    }
    /// A confirmed event key is immutable. Retransmission must carry exactly the same payload.
    pub async fn append(&self, run: &str, kind: &str, key: &str, payload: Value) -> Result<u64> {
        self.append_guarded(run, kind, key, payload, false)
            .await?
            .ok_or(Error::Invalid("unconditional event was not appended"))
    }
    /// Admit an effect only if its execution is still running in the same writer transaction.
    pub async fn append_when_running(
        &self,
        run: &str,
        kind: &str,
        key: &str,
        payload: Value,
    ) -> Result<Option<u64>> {
        self.append_guarded(run, kind, key, payload, true).await
    }
    /// Optimistic atomic fact insertion against a reconstructed journal revision.
    /// Used for shared-budget reservations without holding a driver lock.
    pub async fn append_if_sequence(
        &self,
        run: &str,
        expected: u64,
        kind: &str,
        key: &str,
        payload: Value,
    ) -> Result<Option<u64>> {
        self.append_conditionally(run, expected, kind, key, payload, None)
            .await
    }
    /// Atomically claim one ready child for one parent and reject tree cycles.
    pub async fn append_child_budget_reservation(
        &self,
        run: &str,
        expected: u64,
        key: &str,
        payload: Value,
    ) -> Result<Option<u64>> {
        let child = payload
            .pointer("/allocation/child_run")
            .and_then(Value::as_str)
            .ok_or(Error::Invalid("reservation child missing"))?
            .to_owned();
        self.append_conditionally(
            run,
            expected,
            "child_budget_reserved",
            key,
            payload,
            Some(child),
        )
        .await
    }
    async fn append_conditionally(
        &self,
        run: &str,
        expected: u64,
        kind: &str,
        key: &str,
        payload: Value,
        reserve_child: Option<String>,
    ) -> Result<Option<u64>> {
        if kind.is_empty() || kind.len() > 80 || key.is_empty() || key.len() > 1024 {
            return Err(Error::Invalid("event kind/key invalid"));
        }
        let text = serde_json::to_string(&payload)?;
        if text.len() > 64 * 1024 {
            return Err(Error::Invalid("conditional fact exceeds inline limit"));
        }
        let (run, kind, key) = (run.to_owned(), kind.to_owned(), key.to_owned());
        Ok(self
            .db
            .write(Priority::Journal, move |c| {
                let current = authoritative_state(c, &run)?.0;
                let sequence: u64 = c.query_row(
                    "SELECT COALESCE(MAX(sequence),0) FROM journal WHERE run_id=?1",
                    [&run],
                    |r| r.get(0),
                )?;
                if sequence != expected {
                    return Ok(None);
                }
                if let Some(child)=reserve_child{
                    if !matches!(current.as_str(),"ready"|"running"|"verifying")||authoritative_state(c,&child)?.0!="ready"{return Ok(None);}
                    let expected_child=serde_json::from_str::<Value>(&text).ok().and_then(|v|v["child_sequence"].as_u64()).ok_or(StoreError::Invalid("child revision missing"))?;
                    let child_sequence:u64=c.query_row("SELECT COALESCE(MAX(sequence),0) FROM journal WHERE run_id=?1",[&child],|r|r.get(0))?;
                    if child_sequence!=expected_child{return Ok(None);}
                    let claimed:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM journal WHERE kind='child_budget_reserved' AND json_extract(payload,'$.allocation.child_run')=?1)",[&child],|r|r.get(0))?;
                    if claimed{return Err(StoreError::Invalid("child budget already belongs to an allocation"));}
                    let cyclic:bool=c.query_row("WITH RECURSIVE descendants(id) AS (SELECT ?1 UNION SELECT json_extract(j.payload,'$.allocation.child_run') FROM journal j JOIN descendants d ON j.run_id=d.id WHERE j.kind='child_budget_reserved') SELECT EXISTS(SELECT 1 FROM descendants WHERE id=?2)",rusqlite::params![child,run],|r|r.get(0))?;
                    if cyclic{return Err(StoreError::Invalid("child budget allocation would create a cycle"));}
                }
                append_row(c, &run, &kind, &key, Some(&text), None).map(Some)
            })
            .await?)
    }
    async fn append_guarded(
        &self,
        run: &str,
        kind: &str,
        key: &str,
        payload: Value,
        only_running: bool,
    ) -> Result<Option<u64>> {
        if kind.is_empty() || key.is_empty() || kind.len() > 80 || key.len() > 1024 {
            return Err(Error::Invalid("event kind/key invalid"));
        }
        let text = serde_json::to_string(&payload)?;
        if text.len() > 16 * 1024 * 1024 {
            return Err(Error::Invalid("event exceeds 16 MiB"));
        }
        let (run, kind, key) = (run.to_owned(), kind.to_owned(), key.to_owned());
        if text.len() > 64 * 1024 {
            let blobs = self.db.blobs();
            let bytes = text.clone().into_bytes();
            let prepared = tokio::task::spawn_blocking(move || blobs.prepare(&bytes))
                .await
                .map_err(|_| Error::Invalid("blob preparation panicked"))??;
            Ok(self
                .db
                .write_with_blob(prepared, Priority::Journal, move |c, hash| {
                    if only_running && authoritative_state(c, &run)?.0 != "running" {
                        return Ok(None);
                    }
                    append_row(c, &run, &kind, &key, None, Some(hash.as_str())).map(Some)
                })
                .await?)
        } else {
            Ok(self
                .db
                .write(Priority::Journal, move |c| {
                    if only_running && authoritative_state(c, &run)?.0 != "running" {
                        return Ok(None);
                    }
                    append_row(c, &run, &kind, &key, Some(&text), None).map(Some)
                })
                .await?)
        }
    }
    /// Renew the run's liveness heartbeat in the `runs` projection. Only an
    /// execution that is authoritatively `running` is renewed, so a stale driver
    /// cannot refresh a paused, blocked or terminal run. This is not a journal
    /// event and does not change the replayed revision.
    pub async fn heartbeat(&self, run: &str) -> Result<bool> {
        let run = run.to_owned();
        Ok(self
            .db
            .write(Priority::Journal, move |c| {
                if authoritative_state(c, &run)?.0 != "running" {
                    return Ok(false);
                }
                Ok(c.execute(
                    "UPDATE runs SET heartbeat_at=max(COALESCE(heartbeat_at,0),unixepoch()) WHERE id=?1",
                    [&run],
                )? > 0)
            })
            .await?)
    }
    /// Cheap advisory read; effect admission must still use the writer-side guard.
    pub async fn is_running(&self, run: &str) -> Result<bool> {
        let run = run.to_owned();
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(authoritative_state(c, &run)?.0 == "running")
            })
            .await?)
    }
    pub async fn state(&self, run: &str) -> Result<(String, Option<String>)> {
        let run = run.to_owned();
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| authoritative_state(c, &run))
            .await?)
    }
    pub async fn set_state(
        &self,
        run: &str,
        state: &str,
        reason: Option<String>,
        key: &str,
    ) -> Result<()> {
        match self
            .change_state(
                run,
                &[
                    "created",
                    "ready",
                    "running",
                    "waiting",
                    "verifying",
                    "recovering",
                    "paused",
                    "blocked",
                ],
                state,
                reason,
                key,
                None,
            )
            .await?
        {
            StateChange::Rejected => Err(Error::Invalid("terminal execution cannot change state")),
            StateChange::Applied | StateChange::Unchanged => Ok(()),
        }
    }

    /// Atomically compare the authoritative journal state before appending a transition.
    /// Replayed keys are immutable no-ops; terminal states cannot be reopened.
    pub async fn set_state_if(
        &self,
        run: &str,
        expected: &[&str],
        state: &str,
        reason: Option<String>,
        key: &str,
    ) -> Result<bool> {
        Ok(matches!(
            self.change_state(run, expected, state, reason, key, None)
                .await?,
            StateChange::Applied
        ))
    }
    /// Also reject any intervening journal fact, including a new directive.
    pub async fn set_state_if_revision(
        &self,
        run: &str,
        expected: &[&str],
        sequence: u64,
        state: &str,
        reason: Option<String>,
        key: &str,
    ) -> Result<bool> {
        Ok(matches!(
            self.change_state(run, expected, state, reason, key, Some(sequence))
                .await?,
            StateChange::Applied
        ))
    }
    async fn change_state(
        &self,
        run: &str,
        expected: &[&str],
        state: &str,
        reason: Option<String>,
        key: &str,
        expected_sequence: Option<u64>,
    ) -> Result<StateChange> {
        if !matches!(
            state,
            "running"
                | "created"
                | "ready"
                | "waiting"
                | "verifying"
                | "recovering"
                | "paused"
                | "blocked"
                | "completed"
                | "completed_with_pending"
                | "cancelled"
                | "failed"
        ) || reason.as_ref().is_some_and(|s| s.len() > 65536)
            || key.is_empty()
            || key.len() > 1024
        {
            return Err(Error::Invalid("invalid state transition argument"));
        }
        let (run, state, key) = (run.to_owned(), state.to_owned(), key.to_owned());
        let expected = expected.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let text = serde_json::to_string(&serde_json::json!({"state":state,"reason":reason}))?;
        Ok(self
            .db
            .write(Priority::Journal, move |c| {
                let (current, current_reason) = authoritative_state(c, &run)?;
                // Rebuild the projection from the journal, including on a rejected CAS.
                c.execute(
                    "UPDATE runs SET state=?2,reason=?3 WHERE id=?1",
                    rusqlite::params![run, current, current_reason],
                )?;
                let duplicate: bool = c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM journal WHERE run_id=?1 AND idempotency_key=?2)",
                    rusqlite::params![run, key],
                    |r| r.get(0),
                )?;
                if duplicate {
                    append_row(c, &run, "state_change", &key, Some(&text), None)?;
                    return Ok(StateChange::Unchanged);
                }
                if matches!(
                    current.as_str(),
                    "cancelled" | "completed" | "completed_with_pending" | "failed"
                ) {
                    return Ok(if current == state {
                        StateChange::Unchanged
                    } else {
                        StateChange::Rejected
                    });
                }
                if !expected.contains(&current) {
                    return Ok(StateChange::Rejected);
                }
                if let Some(expected) = expected_sequence {
                    let actual: u64 = c.query_row(
                        "SELECT COALESCE(MAX(sequence),0) FROM journal WHERE run_id=?1",
                        [&run],
                        |r| r.get(0),
                    )?;
                    if actual != expected {
                        return Ok(StateChange::Rejected);
                    }
                }
                if !valid_transition(&current, &state) {
                    return Ok(StateChange::Rejected);
                }
                append_row(c, &run, "state_change", &key, Some(&text), None)?;
                c.execute(
                    "UPDATE runs SET state=?2,reason=?3,updated_at=unixepoch() WHERE id=?1",
                    rusqlite::params![run, state, reason],
                )?;
                Ok(StateChange::Applied)
            })
            .await?)
    }

    pub async fn entries(&self, run: &str, after: u64, limit: u32) -> Result<Vec<Event>> {
        if !(1..=1000).contains(&limit) {
            return Err(Error::Invalid("journal page limit"));
        }
        let run = run.to_string();
        let rows=self.db.read(ReadLane::Interactive,move|c|{
            let mut statement=c.prepare("SELECT sequence,kind,idempotency_key,payload,payload_blob FROM journal WHERE run_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")?;
            Ok(statement.query_map(rusqlite::params![run,after,limit],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)
        }).await?;
        let mut out = Vec::new();
        for (sequence, kind, key, inline, blob) in rows {
            let bytes = if let Some(text) = inline {
                text.into_bytes()
            } else if let Some(hash) = blob {
                self.db.read_blob(hash.parse()?, 16 * 1024 * 1024).await?
            } else {
                return Err(Error::Invalid("event payload missing"));
            };
            out.push(Event {
                sequence,
                kind,
                key,
                payload: serde_json::from_slice(&bytes)?,
            });
        }
        Ok(out)
    }
    pub async fn latest_checkpoint(&self, run: &str) -> Result<Option<u64>> {
        let run = run.to_string();
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT max(sequence) FROM journal WHERE run_id=?1 AND kind='checkpoint'",
                    [run],
                    |r| r.get(0),
                )?)
            })
            .await?)
    }
    pub async fn replay(&self, run: &str, after: u64) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        let mut cursor = after;
        loop {
            let page = self.entries(run, cursor, 1000).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().unwrap().sequence;
            events.extend(page);
        }
        Ok(events)
    }
}
fn valid_transition(from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    match from {
        "created" => matches!(to, "ready" | "cancelled"),
        "ready" => matches!(to, "running" | "paused" | "cancelled"),
        "running" => matches!(
            to,
            "waiting"
                | "verifying"
                | "paused"
                | "blocked"
                | "failed"
                | "cancelled"
                | "recovering"
                | "completed"
                | "completed_with_pending"
        ),
        "waiting" => matches!(
            to,
            "running" | "paused" | "blocked" | "cancelled" | "recovering"
        ),
        "verifying" => matches!(
            to,
            "running"
                | "completed"
                | "completed_with_pending"
                | "blocked"
                | "paused"
                | "cancelled"
                | "recovering"
        ),
        // Direct running resumes remain accepted for existing host adapters.
        "paused" | "blocked" => matches!(to, "ready" | "running" | "cancelled" | "paused"),
        "recovering" => matches!(to, "ready" | "paused" | "blocked" | "cancelled"),
        _ => false,
    }
}
fn authoritative_state(
    c: &rusqlite::Connection,
    run: &str,
) -> excalibur_store::Result<(String, Option<String>)> {
    let exists: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM durable_operations WHERE run_id=?1)",
        [run],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(StoreError::Invalid("durable run does not exist"));
    }
    let latest:Option<Option<String>>=c.query_row("SELECT payload FROM journal WHERE run_id=?1 AND kind='state_change' ORDER BY sequence DESC LIMIT 1",[run],|r|r.get(0)).optional()?;
    match latest {
        None => {
            let seed:String=c.query_row("SELECT payload FROM journal WHERE run_id=?1 AND kind='run_started' ORDER BY sequence LIMIT 1",[run],|r|r.get(0))?;
            let seed: Value =
                serde_json::from_str(&seed).map_err(|_| StoreError::Invalid("invalid run seed"))?;
            Ok((
                if seed["initial_state"] == "created" {
                    "created"
                } else {
                    "running"
                }
                .into(),
                None,
            ))
        }
        Some(Some(payload)) => {
            let value: Value = serde_json::from_str(&payload)
                .map_err(|_| StoreError::Invalid("invalid durable state payload"))?;
            Ok((
                value["state"]
                    .as_str()
                    .ok_or(StoreError::Invalid("durable state missing"))?
                    .to_string(),
                value["reason"].as_str().map(str::to_owned),
            ))
        }
        Some(None) => Err(StoreError::Invalid("durable state payload must be inline")),
    }
}
#[derive(Clone, Copy)]
enum StateChange {
    Applied,
    Unchanged,
    Rejected,
}
fn append_row(
    c: &rusqlite::Connection,
    run: &str,
    kind: &str,
    key: &str,
    text: Option<&str>,
    blob: Option<&str>,
) -> excalibur_store::Result<u64> {
    let known:Option<(u64,String,Option<String>,Option<String>)>=c.query_row("SELECT sequence,kind,payload,payload_blob FROM journal WHERE run_id=?1 AND idempotency_key=?2",rusqlite::params![run,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    if let Some((seq, k, t, b)) = known {
        if k != kind || t.as_deref() != text || b.as_deref() != blob {
            return Err(StoreError::Invalid("journal event key conflict"));
        }
        return Ok(seq);
    }
    if kind == "budget_updated" {
        let bound:Option<String>=c.query_row("SELECT json_extract(payload,'$.allocation.budget') FROM journal WHERE kind='child_budget_reserved' AND json_extract(payload,'$.allocation.child_run')=?1 LIMIT 1",[run],|r|r.get(0)).optional()?;
        if let Some(bound) = bound {
            let bound: Value = serde_json::from_str(&bound)
                .map_err(|_| StoreError::Invalid("invalid allocation budget"))?;
            let update: Value = serde_json::from_str(
                text.ok_or(StoreError::Invalid("budget update must be inline"))?,
            )
            .map_err(|_| StoreError::Invalid("invalid budget update"))?;
            for field in [
                "max_steps",
                "max_tokens",
                "max_cost_microusd",
                "max_wall_seconds",
                "max_tool_calls",
                "tool_timeout_seconds",
                "max_processes",
                "max_terminal_sessions",
            ] {
                if let Some(max) = bound[field].as_u64()
                    && update["budget"][field]
                        .as_u64()
                        .is_none_or(|value| value > max)
                {
                    return Err(StoreError::Invalid(
                        "child budget exceeds its parent allocation",
                    ));
                }
            }
        }
    }
    let seq: u64 = c.query_row(
        "SELECT coalesce(max(sequence),0)+1 FROM journal WHERE run_id=?1",
        [run],
        |r| r.get(0),
    )?;
    c.execute("INSERT INTO journal(id,run_id,sequence,kind,payload,payload_blob,state,idempotency_key) VALUES(?1,?2,?3,?4,?5,?6,'confirmed',?7)",rusqlite::params![JobId::new().to_string(),run,seq,kind,text,blob,key])?;
    Ok(seq)
}

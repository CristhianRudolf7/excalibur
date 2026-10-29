//! Atomic ordering of new approvals and terminal engine transitions. The lock is
//! distinct from the long-lived journal run lock, and shared across decorators.
use crate::{Result, Security};
use excalibur_store::{
    ReadLane,
    rusqlite::{Connection, OptionalExtension},
};
use excalibur_types::runtime::{FinalizationGuard, FinalizationState};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, Weak},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
type Key = (PathBuf, String);
type Registry = Mutex<BTreeMap<Key, Weak<AsyncMutex<()>>>>;
static GATES: OnceLock<Registry> = OnceLock::new();

/// Use the journal as authority whenever the run is journal-owned; the runs row
/// is only a projection. Legacy host fixtures may have only a runs row.
pub(crate) fn authoritative_run_state(
    connection: &Connection,
    run: &str,
) -> excalibur_store::Result<String> {
    let durable_table:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='durable_operations')",[],|row|row.get(0))?;
    let durable = durable_table
        && connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM durable_operations WHERE run_id=?1)",
            [run],
            |row| row.get::<_, bool>(0),
        )?;
    if durable {
        let latest:Option<Option<String>>=connection.query_row("SELECT payload FROM journal WHERE run_id=?1 AND kind='state_change' ORDER BY sequence DESC LIMIT 1",[run],|row|row.get(0)).optional()?;
        match latest {
            None => Ok("running".into()),
            Some(Some(payload)) => {
                let value: serde_json::Value = serde_json::from_str(&payload)
                    .map_err(|_| excalibur_store::Error::Invalid("invalid durable run state"))?;
                value["state"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or(excalibur_store::Error::Invalid("durable state is missing"))
            }
            Some(None) => Err(excalibur_store::Error::Invalid("run state must be inline")),
        }
    } else {
        Ok(
            connection.query_row("SELECT state FROM runs WHERE id=?1", [run], |row| {
                row.get(0)
            })?,
        )
    }
}
pub(crate) fn terminal(state: &str) -> bool {
    matches!(
        state,
        "completed" | "completed_with_pending" | "cancelled" | "failed"
    )
}
struct HeldGuard {
    _lock: OwnedMutexGuard<()>,
    _database: excalibur_store::Database,
    state: FinalizationState,
}
impl FinalizationGuard for HeldGuard {
    fn state(&self) -> FinalizationState {
        self.state.clone()
    }
}
impl Security {
    pub(crate) async fn approval_ordering_lock(&self, run: &str) -> Result<OwnedMutexGuard<()>> {
        let key = (
            self.database().path().canonicalize().map_err(|_| {
                crate::Error::Denied("canonical security database path unavailable".into())
            })?,
            run.to_owned(),
        );
        let lock = {
            let mut registry = GATES
                .get_or_init(|| Mutex::new(BTreeMap::new()))
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            registry.retain(|_, value| value.strong_count() > 0);
            if let Some(lock) = registry.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(AsyncMutex::new(()));
                registry.insert(key, Arc::downgrade(&lock));
                lock
            }
        };
        Ok(lock.lock_owned().await)
    }
    /// Keep the returned guard alive THROUGH the engine's terminal state CAS.
    /// New approval creation uses the same lock and rejects terminal journal runs.
    pub async fn finalization_guard(
        &self,
        run: &str,
        now: i64,
    ) -> Result<Box<dyn FinalizationGuard>> {
        let lock = self.approval_ordering_lock(run).await?;
        self.approval_maintenance(now).await?;
        let run = run.to_owned();
        let state=self.database().read(ReadLane::Interactive,move|connection| {
            // One SQL statement provides a consistent snapshot if a host answers
            // an approval concurrently; answers never create a pending request.
            let blocked:Option<String>=connection.query_row("SELECT CASE WHEN EXISTS(SELECT 1 FROM security_approvals WHERE run_id=?1 AND status IN('rejected','expired')) THEN 'blocked:'||(SELECT status FROM security_approvals WHERE run_id=?1 AND status IN('rejected','expired') ORDER BY rowid DESC LIMIT 1) WHEN EXISTS(SELECT 1 FROM security_approvals WHERE run_id=?1 AND status='pending' AND expires_at>?2) THEN 'pending' ELSE NULL END",excalibur_store::rusqlite::params![run,now],|row|row.get(0))?;
            Ok(match blocked {Some(state)if state=="pending"=>FinalizationState::Pending,Some(state)=>FinalizationState::Blocked(format!("approval {}",state.trim_start_matches("blocked:"))),None=>FinalizationState::Ready})
        }).await?;
        Ok(Box::new(HeldGuard {
            _lock: lock,
            _database: self.database().clone(),
            state,
        }))
    }
}

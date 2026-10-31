//! Durable run-stall watchdog (17.6/22.5).
//!
//! A run whose driver is live in this process, is `running`, and has neither
//! renewed its heartbeat nor confirmed driver progress for ten minutes (outside
//! the declared timeout of a registered pending tool) is interrupted with an
//! exact-revision CAS. Uncertain non-pure intentions block the run; otherwise it
//! is paused. Once the old driver task is gone, the shared 17.3 assessment
//! decides whether the watchdog-owned pause may resume automatically. Every
//! incident produces one scoped alert through the ordinary notification
//! pipeline. Effects are never executed or retried by the watchdog itself.
use crate::*;
use excalibur_engine::{WATCHDOG_BLOCK_REASON, WATCHDOG_PAUSE_REASON};
use excalibur_security::Autonomy;
use serde::{Deserialize, Serialize};

/// Ten minutes without a heartbeat or driver progress (17.6, appendix default).
pub const STALL_SECONDS: i64 = 600;
/// Cooperative interruption window before the hung driver task is aborted.
const ABORT_GRACE_SECONDS: i64 = 5;
/// After an abort, a driver that still has not stopped is reported without resume.
const ABORT_GIVE_UP_SECONDS: i64 = 60;
const BATCH: u32 = 128;
/// Journal facts that only the engine driver writes while it makes progress.
/// External controls, messages and host notes are deliberately excluded so that
/// they cannot mask a hung driver.
pub const PROGRESS_KINDS: &str = "'run_started','model_request','model_dispatch','model_response','model_failure','model_interrupted','tool_intent','tool_result','step_completed','checkpoint','compaction','directive_applied','wake_event'";

/// Durable stall-watchdog record for one interrupted (or attempted) revision.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatchdogIncident {
    pub run_id: String,
    pub sequence: u64,
    pub agent_id: String,
    pub observed_at: i64,
    pub action: Option<String>,
    pub decision: Option<String>,
    pub reason: Option<String>,
    pub aborted: bool,
    pub notified: bool,
}

pub(crate) async fn initialize(db: &Database) -> Result<()> {
    let sql = include_str!("run_watchdog.sql");
    let checksum = digest(sql.as_bytes());
    db.write(Priority::Journal, move |c| {
        c.execute_batch(sql)?;
        let old: Option<String> = c
            .query_row(
                "SELECT checksum FROM orchestration_run_watchdog_schema WHERE version=1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if old.is_some_and(|old| old != checksum) {
            return Err(excalibur_store::Error::Configuration(
                "run watchdog schema checksum mismatch".into(),
            ));
        }
        c.execute(
            "INSERT OR IGNORE INTO orchestration_run_watchdog_schema VALUES(1,?1)",
            [checksum],
        )?;
        // Additive v2 and v3: each is applied exactly once in this transaction.
        for (version, next) in [
            (2, include_str!("run_watchdog2.sql")),
            (3, include_str!("run_watchdog3.sql")),
        ] {
            let next_checksum = digest(next.as_bytes());
            let old: Option<String> = c
                .query_row(
                    "SELECT checksum FROM orchestration_run_watchdog_schema WHERE version=?1",
                    [version],
                    |r| r.get(0),
                )
                .optional()?;
            match old {
                Some(old) if old != next_checksum => {
                    return Err(excalibur_store::Error::Configuration(format!(
                        "run watchdog schema v{version} checksum mismatch"
                    )));
                }
                Some(_) => {}
                None => {
                    c.execute_batch(next)?;
                    c.execute(
                        "INSERT INTO orchestration_run_watchdog_schema VALUES(?1,?2)",
                        rusqlite::params![version, next_checksum],
                    )?;
                }
            }
        }
        Ok(())
    })
    .await?;
    Ok(())
}

impl Orchestrator {
    /// One watchdog pass with an explicit host clock (UTC seconds). Returns the
    /// agents whose stalled revision this pass interrupted. Detection only
    /// considers driver tasks live in this orchestrator; durable incidents from
    /// an earlier process are still reconciled and reported. A failure while
    /// handling one run is isolated and reported; other runs still progress.
    pub async fn watchdog_runs(&self, at: i64) -> Result<Vec<String>> {
        let _pass = self.inner.watchdog.lock().await;
        self.reap().await?;
        let interrupted = self.detect_stalls(at).await?;
        self.settle_watchdog_incidents(at).await?;
        self.publish_watchdog_notices().await?;
        Ok(interrupted)
    }
    /// Durable watchdog records, newest first, optionally for one run.
    pub async fn watchdog_incidents(&self, run: Option<&str>) -> Result<Vec<WatchdogIncident>> {
        let run = run.map(str::to_owned);
        Ok(self.inner.db.read(ReadLane::Interactive, move |c| {
            let mut q = c.prepare("SELECT run_id,sequence,agent_id,observed_at,action,decision,reason,aborted,notified FROM orchestration_run_watchdog WHERE ?1 IS NULL OR run_id=?1 ORDER BY observed_at DESC,run_id,sequence DESC LIMIT 1000")?;
            Ok(q.query_map([run], |r| {
                Ok(WatchdogIncident {
                    run_id: r.get(0)?,
                    sequence: r.get(1)?,
                    agent_id: r.get(2)?,
                    observed_at: r.get(3)?,
                    action: r.get(4)?,
                    decision: r.get(5)?,
                    reason: r.get(6)?,
                    aborted: r.get(7)?,
                    notified: r.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
        }).await?)
    }
    async fn live_driver(&self, agent: &str) -> bool {
        self.inner
            .running
            .lock()
            .await
            .get(agent)
            .is_some_and(|h| !h.is_finished())
    }
    async fn detect_stalls(&self, at: i64) -> Result<Vec<String>> {
        let live = self
            .inner
            .running
            .lock()
            .await
            .iter()
            .filter(|(_, h)| !h.is_finished())
            .map(|(id, h)| (id.clone(), h.id()))
            .collect::<Vec<_>>();
        let mut interrupted = Vec::new();
        for (agent, task) in live {
            match self.detect_stall(&agent, task, at).await {
                Ok(true) => interrupted.push(agent),
                Ok(false) => {}
                Err(error) => {
                    eprintln!("run watchdog: detection for agent {agent} failed: {error}")
                }
            }
        }
        Ok(interrupted)
    }
    async fn detect_stall(&self, agent: &str, task: tokio::task::Id, at: i64) -> Result<bool> {
        let Some(run) = self.agent(agent).await?.run_id else {
            return Ok(false);
        };
        let state = self.inner.engine.load(&run).await?;
        if state.status != Status::Running {
            return Ok(false);
        }
        let sequence = state.last_sequence;
        let r = run.clone();
        let (actual, reference, last_intent): (u64, i64, Option<i64>) = self
            .inner
            .db
            .read(ReadLane::Background, move |c| {
                Ok(c.query_row(
                    &format!("SELECT (SELECT COALESCE(MAX(sequence),0) FROM journal WHERE run_id=?1),max(COALESCE((SELECT heartbeat_at FROM runs WHERE id=?1),0),COALESCE((SELECT MAX(created_at) FROM journal WHERE run_id=?1 AND (kind IN ({PROGRESS_KINDS}) OR (kind='state_change' AND json_extract(payload,'$.state')='running'))),0)),(SELECT MAX(created_at) FROM journal WHERE run_id=?1 AND kind='tool_intent')"),
                    [r],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?)
            })
            .await?;
        if actual != sequence {
            return Ok(false);
        }
        // A durably registered pending tool is a declared long wait until its
        // timeout; the stall clock starts again at that deadline.
        let pending_tool = state
            .calls
            .values()
            .any(|call| call.prepared.is_some() && call.result.is_none());
        let reference = match (pending_tool, last_intent) {
            (true, Some(intent)) => reference.max(intent.saturating_add(
                i64::try_from(state.spec.budget.tool_timeout_seconds).unwrap_or(i64::MAX),
            )),
            _ => reference,
        };
        if at.saturating_sub(reference) < STALL_SECONDS {
            return Ok(false);
        }
        let (r, a) = (run.clone(), agent.to_owned());
        let admitted = self.inner.db.write(Priority::Journal, move |c| {
            let actual: u64 = c.query_row("SELECT COALESCE(MAX(sequence),0) FROM journal WHERE run_id=?1", [&r], |row| row.get(0))?;
            let heartbeat: i64 = c.query_row("SELECT COALESCE(heartbeat_at,0) FROM runs WHERE id=?1", [&r], |row| row.get(0))?;
            if actual != sequence || at.saturating_sub(heartbeat) < STALL_SECONDS {
                return Ok(false);
            }
            // Host intent precedes the CAS; only this host row plus the
            // confirmed exact-key transition can produce a watchdog alert.
            c.execute("INSERT OR IGNORE INTO orchestration_run_watchdog(run_id,sequence,agent_id,observed_at,stalled_since) VALUES(?1,?2,?3,?4,?5)", rusqlite::params![r, sequence, a, at, reference])?;
            Ok(true)
        }).await?;
        if !admitted {
            return Ok(false);
        }
        // Only the exact driver task observed here may later be aborted.
        self.inner
            .watchdog_targets
            .lock()
            .unwrap()
            .insert(agent.to_owned(), task);
        if self
            .inner
            .engine
            .interrupt_stalled_revision(&run, sequence)
            .await?
            .is_none()
        {
            return Ok(false);
        }
        // Best effort: the host may also stop the managed tool process.
        if let Some(host) = self.authority() {
            let _ = host.interrupt_tool(&run).await;
        }
        Ok(true)
    }
    async fn settle_watchdog_incidents(&self, at: i64) -> Result<()> {
        let rows = self.inner.db.read(ReadLane::Background, |c| {
            let mut q = c.prepare("SELECT w.run_id,w.sequence,w.agent_id,w.observed_at,w.aborted,w.stalled_since,(SELECT j.idempotency_key FROM journal j WHERE j.run_id=w.run_id AND j.sequence=w.sequence+1 AND j.kind='state_change' AND j.state='confirmed' AND j.idempotency_key IN ('watchdog-stalled:'||w.sequence,'watchdog-blocked:'||w.sequence)),(SELECT COALESCE(MAX(sequence),0) FROM journal j WHERE j.run_id=w.run_id),EXISTS(SELECT 1 FROM journal j WHERE j.run_id=w.run_id AND j.kind='state_change' AND j.state='confirmed' AND j.idempotency_key='watchdog-resume:'||w.sequence) FROM orchestration_run_watchdog w WHERE w.decision IS NULL ORDER BY w.observed_at,w.run_id LIMIT ?1")?;
            Ok(q.query_map([BATCH], |r| {
                Ok(Incident {
                    run: r.get(0)?,
                    sequence: r.get(1)?,
                    agent: r.get(2)?,
                    observed: r.get(3)?,
                    aborted: r.get(4)?,
                    stalled_since: r.get(5)?,
                    confirmed: r.get(6)?,
                    actual: r.get(7)?,
                    resumed: r.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
        }).await?;
        for incident in rows {
            if let Err(error) = self.settle_incident(&incident, at).await {
                eprintln!(
                    "run watchdog: settling run {} revision {} failed: {error}",
                    incident.run, incident.sequence
                );
            }
        }
        Ok(())
    }
    async fn settle_incident(&self, incident: &Incident, at: i64) -> Result<()> {
        let (run, sequence, agent) = (&incident.run, incident.sequence, &incident.agent);
        let Some(key) = &incident.confirmed else {
            let state = self.inner.engine.load(run).await?;
            if state.status == Status::Running
                && incident.actual == sequence
                && self.live_driver(agent).await
            {
                // The CAS may still apply on a later detection pass.
                return Ok(());
            }
            return self
                .record_watchdog_decision(
                    run,
                    sequence,
                    None,
                    "not_applied",
                    "the run progressed or changed before the interruption",
                )
                .await;
        };
        let action = if key.starts_with("watchdog-blocked:") {
            "blocked"
        } else {
            "paused"
        };
        // The incident is still owned by the watchdog only while the run stays
        // in the exact stop state that its CAS produced.
        let owned = |state: &excalibur_engine::Snapshot| {
            (action == "paused"
                && state.status == Status::Paused
                && state.reason.as_deref() == Some(WATCHDOG_PAUSE_REASON))
                || (action == "blocked"
                    && state.status == Status::Blocked
                    && state.reason.as_deref() == Some(WATCHDOG_BLOCK_REASON))
        };
        let before = self.inner.engine.load(run).await?;
        // Wait for the exact observed driver: cooperative stop, then abort.
        let target = self
            .inner
            .watchdog_targets
            .lock()
            .unwrap()
            .get(agent)
            .copied();
        let stuck = {
            let handles = self.inner.running.lock().await;
            match handles.get(agent) {
                Some(handle) if !handle.is_finished() && target != Some(handle.id()) => {
                    // A driver not observed at detection is never aborted. If
                    // the watchdog no longer owns the run, a later control
                    // started it and the incident is superseded; otherwise an
                    // unidentified driver is only awaited.
                    if target.is_some() || !owned(&before) || incident.resumed {
                        None
                    } else {
                        Some(at >= incident.observed.saturating_add(ABORT_GIVE_UP_SECONDS))
                    }
                }
                Some(handle) if !handle.is_finished() => {
                    if !incident.aborted
                        && at >= incident.observed.saturating_add(ABORT_GRACE_SECONDS)
                    {
                        // Persist the abort and remember the exact task before
                        // cancelling, so the reaper ignores only this
                        // cancellation, whatever is decided meanwhile.
                        let (r, s) = (run.clone(), sequence);
                        self.inner.db.write(Priority::Journal, move |c| {
                            c.execute("UPDATE orchestration_run_watchdog SET aborted=1 WHERE run_id=?1 AND sequence=?2", rusqlite::params![r, s])?;
                            Ok(())
                        }).await?;
                        self.inner
                            .watchdog_aborted_tasks
                            .lock()
                            .unwrap()
                            .insert(handle.id());
                        handle.abort();
                    }
                    Some(at >= incident.observed.saturating_add(ABORT_GIVE_UP_SECONDS))
                }
                _ => None,
            }
        };
        match stuck {
            Some(false) => return Ok(()),
            Some(true) => {
                return self
                    .record_watchdog_decision(
                        run,
                        sequence,
                        Some(action),
                        action,
                        "the stalled driver did not stop after abort; automatic resume refused",
                    )
                    .await;
            }
            None => {}
        }
        // A finished but unreaped driver is reaped before inspection.
        self.reap().await?;
        self.inner.watchdog_targets.lock().unwrap().remove(agent);
        let state = self.inner.engine.load(run).await?;
        let (decision, reason) = if incident.resumed {
            // The resume CAS committed earlier. If another control (for example
            // startup recovery after a long restart) owns the run now, the
            // incident is superseded and no "resumed" note is written.
            if matches!(state.status, Status::Paused | Status::Blocked) || terminal(&state.status) {
                ("superseded", superseded(&state))
            } else {
                let reason = "eligible after watchdog reconciliation";
                self.watchdog_note(run, sequence, "resumed", reason).await?;
                ("resumed", reason.to_owned())
            }
        } else if !owned(&state) {
            ("superseded", superseded(&state))
        } else if action == "blocked" {
            self.watchdog_note(run, sequence, "blocked", WATCHDOG_BLOCK_REASON)
                .await?;
            ("blocked", WATCHDOG_BLOCK_REASON.to_owned())
        } else {
            let host = self
                .inner
                .host_autonomy
                .lock()
                .unwrap()
                .unwrap_or(Autonomy::Supervised);
            // 17.3 downtime: from the stall reference (last heartbeat, progress
            // or registered tool deadline), plus the host downtime when the
            // incident predates the current daemon boot.
            let since = incident.stalled_since.unwrap_or(incident.observed);
            let mut downtime = at.saturating_sub(since);
            if let Some((started, boot_downtime)) = self.recovery().current_boot().await?
                && incident.observed < started
            {
                downtime = downtime.max(boot_downtime);
            }
            let mut blockers = self
                .assess_resume(run, &state)
                .await?
                .blockers(host, downtime);
            let recovery = self.recovery().status(at).await?;
            if recovery.recovering || recovery.safe_mode {
                blockers.push("startup recovery or safe mode is active");
            }
            if recovery.quarantined.contains(run) {
                blockers.push("run is quarantined");
            }
            let r = run.clone();
            let repeated: bool = self.inner.db.read(ReadLane::Interactive, move |c| {
                Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM orchestration_run_watchdog WHERE run_id=?1 AND decision='resumed')", [r], |row| row.get(0))?)
            }).await?;
            if repeated {
                blockers.push("an earlier watchdog incident already resumed this run");
            }
            if blockers.is_empty() {
                if !self
                    .inner
                    .engine
                    .resume_watchdog_pause(run, sequence)
                    .await?
                {
                    // A concurrent control won the race for the pause.
                    let now_state = self.inner.engine.load(run).await?;
                    if owned(&now_state) {
                        return Ok(());
                    }
                    return self
                        .record_watchdog_decision(
                            run,
                            sequence,
                            Some(action),
                            "superseded",
                            &superseded(&now_state),
                        )
                        .await;
                }
                let reason = "eligible after watchdog reconciliation";
                self.watchdog_note(run, sequence, "resumed", reason).await?;
                ("resumed", reason.to_owned())
            } else {
                let reason = blockers.join("; ");
                self.watchdog_note(run, sequence, "paused", &reason).await?;
                ("paused", reason)
            }
        };
        self.record_watchdog_decision(run, sequence, Some(action), decision, &reason)
            .await
    }
    /// Host recovery fact replayed by the engine into L5 continuation context.
    async fn watchdog_note(
        &self,
        run: &str,
        sequence: u64,
        decision: &str,
        reason: &str,
    ) -> Result<()> {
        let payload = json!({"source":"run_watchdog","sequence":sequence,"decision":decision,"reason":reason,"stall_seconds":STALL_SECONDS,"note":"the run was interrupted after ten minutes without a heartbeat; terminal sessions and in-flight tool processes may have been stopped"});
        let key = format!(
            "watchdog-note:{sequence}:{}",
            &digest(payload.to_string().as_bytes())[..16]
        );
        self.inner
            .engine
            .journal()
            .append(run, "recovery_note", &key, payload)
            .await?;
        Ok(())
    }
    async fn record_watchdog_decision(
        &self,
        run: &str,
        sequence: u64,
        action: Option<&str>,
        decision: &str,
        reason: &str,
    ) -> Result<()> {
        let (run, action, decision, reason) = (
            run.to_owned(),
            action.map(str::to_owned),
            decision.to_owned(),
            reason.to_owned(),
        );
        self.inner.db.write(Priority::Journal, move |c| {
            c.execute("UPDATE orchestration_run_watchdog SET action=?3,decision=?4,reason=?5,notified=CASE WHEN ?4='not_applied' THEN 1 ELSE notified END WHERE run_id=?1 AND sequence=?2 AND decision IS NULL", rusqlite::params![run, sequence, action, decision, reason])?;
            Ok(())
        }).await?;
        Ok(())
    }
    /// One alert per incident, committed atomically with its receipt flag in the
    /// ordinary notification table; the master delivery pipeline routes it.
    /// Content passes the same secret-literal and size checks as `notify`.
    async fn publish_watchdog_notices(&self) -> Result<()> {
        type Pending = (
            String,
            u64,
            String,
            i64,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<Pending> = self.inner.db.read(ReadLane::Background, |c| {
            let mut q = c.prepare("SELECT w.run_id,w.sequence,w.agent_id,w.observed_at,w.action,w.decision,w.reason,json_extract(a.body,'$.origin_conversation'),json_extract(a.body,'$.scope') FROM orchestration_run_watchdog w JOIN orchestration_agents a ON a.id=w.agent_id WHERE w.notified=0 AND w.decision IS NOT NULL ORDER BY w.observed_at,w.run_id LIMIT ?1")?;
            Ok(q.query_map([BATCH], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
        }).await?;
        let mut published = 0usize;
        for (run, sequence, agent, observed, action, decision, reason, conversation, scope) in rows
        {
            let action = action.unwrap_or_default();
            let reason = reason
                .unwrap_or_default()
                .chars()
                .take(MAX_ALERT_REASON)
                .collect::<String>();
            let build = |reason: &str| {
                let text = format!(
                    "Run {run} was declared stalled after at least ten minutes without a heartbeat and was interrupted ({action}). Decision: {decision}: {reason}. No uncertain effect was retried automatically."
                );
                json!({"kind":"run_watchdog","run_id":run,"agent_id":agent,"sequence":sequence,"observed_at":observed,"action":action,"decision":decision,"reason":reason,"origin_conversation":conversation,"scope":scope,"text":text}).to_string()
            };
            let mut content = build(&reason);
            if content.len() > 64 * 1024
                || self
                    .validate_durable(&json!({ "content": content }))
                    .is_err()
            {
                content = build("details withheld: they failed durable content checks");
            }
            let class = if decision == "resumed" {
                "normal"
            } else {
                "urgent"
            };
            let id = format!(
                "ntf_{}",
                digest(format!("watchdog:{run}:{sequence}").as_bytes())
            );
            let (agent, conversation) = (agent.clone(), conversation.clone().unwrap_or_default());
            let inserted = self.inner.db.write(Priority::Journal, move |c| {
                let open = c.execute("UPDATE orchestration_run_watchdog SET notified=1 WHERE run_id=?1 AND sequence=?2 AND notified=0", rusqlite::params![run, sequence])?;
                if open > 0 {
                    c.execute("INSERT OR IGNORE INTO orchestration_notifications(id,agent_id,conversation,class,content,created,acknowledged) VALUES(?1,?2,?3,?4,?5,?6,0)", rusqlite::params![id, agent, conversation, class, content, observed])?;
                }
                Ok(open > 0)
            }).await?;
            published += usize::from(inserted);
        }
        if published > 0 {
            self.inner.wake.notify_waiters();
        }
        Ok(())
    }
}

/// Bound on reason text copied into an alert.
const MAX_ALERT_REASON: usize = 1024;

struct Incident {
    run: String,
    sequence: u64,
    agent: String,
    observed: i64,
    aborted: bool,
    stalled_since: Option<i64>,
    confirmed: Option<String>,
    actual: u64,
    resumed: bool,
}

/// Only the state label: free-form reasons from other owners are not copied.
fn superseded(state: &excalibur_engine::Snapshot) -> String {
    format!(
        "a later control or engine transition owns the run: {}",
        label(&state.status)
    )
}

#[cfg(test)]
mod watchdog_unit_tests {
    use super::*;
    use crate::recovery_test_support::*;
    use std::sync::Arc;

    fn coordinated(name: &str) -> Delegation {
        let mut task = delegation(name, "general", 2000);
        task.profile = "coordinator".into();
        task
    }
    type Driver = tokio::task::JoinHandle<excalibur_engine::Result<excalibur_engine::Snapshot>>;
    fn hung_driver() -> Driver {
        tokio::spawn(std::future::pending())
    }
    async fn running(f: &Fixture, name: &str) -> (String, String) {
        let agent = f.host.spawn(&f.master.id, coordinated(name)).await.unwrap();
        let run = agent.run_id.unwrap();
        f.host
            .engine()
            .journal()
            .set_state(&run, "running", None, &format!("fixture:{name}"))
            .await
            .unwrap();
        (agent.id, run)
    }
    async fn finished(handle: &Driver) {
        for _ in 0..500 {
            if handle.is_finished() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("task did not finish");
    }
    async fn incident_row(
        f: &Fixture,
        run: &str,
        agent: &str,
        observed: i64,
        decision: Option<&str>,
        reason: &str,
    ) {
        let (run, agent, decision, reason) = (
            run.to_owned(),
            agent.to_owned(),
            decision.map(str::to_owned),
            reason.to_owned(),
        );
        f.db.write(Priority::Journal, move |c| {
            c.execute("INSERT INTO orchestration_run_watchdog(run_id,sequence,agent_id,observed_at,action,decision,reason) VALUES(?1,1,?2,?3,'paused',?4,?5)", rusqlite::params![run, agent, observed, decision, reason])?;
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_the_exact_watchdog_aborted_task_escapes_failure() {
        let f = fixture().await;
        let (plain, plain_run) = running(&f, "plain-cancel").await;
        let (watched, watched_run) = running(&f, "watchdog-cancel").await;
        let (a, b) = (hung_driver(), hung_driver());
        f.host
            .inner
            .watchdog_aborted_tasks
            .lock()
            .unwrap()
            .insert(b.id());
        a.abort();
        b.abort();
        finished(&a).await;
        finished(&b).await;
        {
            let mut handles = f.host.inner.running.lock().await;
            handles.insert(plain, a);
            handles.insert(watched, b);
        }
        assert_eq!(f.host.reap().await.unwrap(), 2);
        assert_eq!(
            f.host.engine().load(&plain_run).await.unwrap().status,
            Status::Failed
        );
        assert_ne!(
            f.host.engine().load(&watched_run).await.unwrap().status,
            Status::Failed
        );
        assert!(
            f.host
                .inner
                .watchdog_aborted_tasks
                .lock()
                .unwrap()
                .is_empty()
        );
        f.host.shutdown().await.unwrap();
        f.db.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_driver_not_observed_at_detection_is_never_aborted() {
        let f = fixture().await;
        let (agent, run) = running(&f, "replaced-driver").await;
        let observed = hung_driver();
        let observed_id = observed.id();
        f.host
            .inner
            .running
            .lock()
            .await
            .insert(agent.clone(), observed);
        let at = now() + 10_000;
        assert_eq!(f.host.watchdog_runs(at).await.unwrap(), vec![agent.clone()]);
        assert_eq!(
            f.host
                .inner
                .watchdog_targets
                .lock()
                .unwrap()
                .get(&agent)
                .copied(),
            Some(observed_id)
        );
        // A different driver task now occupies the agent slot.
        let newer = hung_driver();
        let old = f
            .host
            .inner
            .running
            .lock()
            .await
            .insert(agent.clone(), newer)
            .unwrap();
        old.abort();
        f.host
            .watchdog_runs(at + ABORT_GIVE_UP_SECONDS + 1)
            .await
            .unwrap();
        let incident = &f.host.watchdog_incidents(Some(&run)).await.unwrap()[0];
        assert!(!incident.aborted);
        let newer = f.host.inner.running.lock().await.remove(&agent).unwrap();
        assert!(!newer.is_finished());
        newer.abort();
        f.host.shutdown().await.unwrap();
        f.db.shutdown().await.unwrap();
    }

    struct Redacting;
    impl RunAuthority for Redacting {
        fn sanitize_durable(&self, value: &Value) -> std::result::Result<Value, String> {
            Ok(serde_json::from_str(
                &value
                    .to_string()
                    .replace("SYNTHETIC-SECRET-LITERAL", "[redacted]"),
            )
            .unwrap())
        }
        fn bind<'a>(
            &'a self,
            _: &'a Agent,
        ) -> excalibur_types::runtime::BoxFuture<'a, std::result::Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn alert_withholds_details_that_fail_durable_checks() {
        let f = fixture().await;
        let agent = f
            .host
            .spawn(&f.master.id, coordinated("alert-fallback"))
            .await
            .unwrap();
        let run = agent.run_id.clone().unwrap();
        let host = f.host.clone().with_authority(Arc::new(Redacting));
        incident_row(
            &f,
            &run,
            &agent.id,
            1,
            Some("paused"),
            "token SYNTHETIC-SECRET-LITERAL",
        )
        .await;
        host.watchdog_runs(now()).await.unwrap();
        let alerts = host
            .notifications(&agent.origin_conversation)
            .await
            .unwrap()
            .into_iter()
            .filter(|n| n.content.contains("run_watchdog"))
            .collect::<Vec<_>>();
        assert_eq!(alerts.len(), 1);
        assert!(!alerts[0].content.contains("SYNTHETIC-SECRET-LITERAL"));
        assert!(alerts[0].content.contains("details withheld"));
        assert!(host.watchdog_incidents(Some(&run)).await.unwrap()[0].notified);
        host.shutdown().await.unwrap();
        f.db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_failing_incident_does_not_stop_the_others() {
        let f = fixture().await;
        let agent = f
            .host
            .spawn(&f.master.id, coordinated("isolation"))
            .await
            .unwrap();
        let run = agent.run_id.clone().unwrap();
        // A run row with no durable journal cannot be loaded by the engine.
        f.db.write(Priority::Journal, |c| {
            c.execute(
                "INSERT INTO runs(id,state) VALUES('broken-run','running')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        incident_row(&f, "broken-run", &agent.id, 0, None, "").await;
        incident_row(&f, &run, &agent.id, 1, None, "").await;
        f.host.watchdog_runs(now()).await.unwrap();
        let broken = f.host.watchdog_incidents(Some("broken-run")).await.unwrap();
        assert!(broken[0].decision.is_none());
        let healthy = f.host.watchdog_incidents(Some(&run)).await.unwrap();
        assert_eq!(healthy[0].decision.as_deref(), Some("not_applied"));
        f.host.shutdown().await.unwrap();
        f.db.shutdown().await.unwrap();
    }
}

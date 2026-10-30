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
    let next = include_str!("run_watchdog2.sql");
    let next_checksum = digest(next.as_bytes());
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
        // Additive v2: columns are added exactly once in this transaction.
        let old: Option<String> = c
            .query_row(
                "SELECT checksum FROM orchestration_run_watchdog_schema WHERE version=2",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match old {
            Some(old) if old != next_checksum => {
                return Err(excalibur_store::Error::Configuration(
                    "run watchdog decision schema checksum mismatch".into(),
                ));
            }
            Some(_) => {}
            None => {
                c.execute_batch(next)?;
                c.execute(
                    "INSERT INTO orchestration_run_watchdog_schema VALUES(2,?1)",
                    [next_checksum],
                )?;
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
    /// an earlier process are still reconciled and reported.
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
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut interrupted = Vec::new();
        for agent in live {
            let Some(run) = self.agent(&agent).await?.run_id else {
                continue;
            };
            let state = self.inner.engine.load(&run).await?;
            if state.status != Status::Running {
                continue;
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
                continue;
            }
            // A durably registered pending tool is a declared long wait until
            // its timeout; the stall clock starts again at that deadline.
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
                continue;
            }
            let (r, a) = (run.clone(), agent.clone());
            let admitted = self.inner.db.write(Priority::Journal, move |c| {
                let actual: u64 = c.query_row("SELECT COALESCE(MAX(sequence),0) FROM journal WHERE run_id=?1", [&r], |row| row.get(0))?;
                let heartbeat: i64 = c.query_row("SELECT COALESCE(heartbeat_at,0) FROM runs WHERE id=?1", [&r], |row| row.get(0))?;
                if actual != sequence || at.saturating_sub(heartbeat) < STALL_SECONDS {
                    return Ok(false);
                }
                // Host intent precedes the CAS; only this host row plus the
                // confirmed exact-key transition can produce a watchdog alert.
                c.execute("INSERT OR IGNORE INTO orchestration_run_watchdog(run_id,sequence,agent_id,observed_at) VALUES(?1,?2,?3,?4)", rusqlite::params![r, sequence, a, at])?;
                Ok(true)
            }).await?;
            if !admitted {
                continue;
            }
            if self
                .inner
                .engine
                .interrupt_stalled_revision(&run, sequence)
                .await?
                .is_some()
            {
                // Best effort: the host may also stop the managed tool process.
                if let Some(host) = self.authority() {
                    let _ = host.interrupt_tool(&run).await;
                }
                interrupted.push(agent);
            }
        }
        Ok(interrupted)
    }
    async fn settle_watchdog_incidents(&self, at: i64) -> Result<()> {
        let rows = self.inner.db.read(ReadLane::Background, |c| {
            let mut q = c.prepare("SELECT w.run_id,w.sequence,w.agent_id,w.observed_at,w.aborted,(SELECT j.idempotency_key FROM journal j WHERE j.run_id=w.run_id AND j.sequence=w.sequence+1 AND j.kind='state_change' AND j.state='confirmed' AND j.idempotency_key IN ('watchdog-stalled:'||w.sequence,'watchdog-blocked:'||w.sequence)),(SELECT COALESCE(MAX(sequence),0) FROM journal j WHERE j.run_id=w.run_id),EXISTS(SELECT 1 FROM journal j WHERE j.run_id=w.run_id AND j.kind='state_change' AND j.state='confirmed' AND j.idempotency_key='watchdog-resume:'||w.sequence) FROM orchestration_run_watchdog w WHERE w.decision IS NULL ORDER BY w.observed_at,w.run_id LIMIT ?1")?;
            Ok(q.query_map([BATCH], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, u64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, bool>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, u64>(6)?,
                    r.get::<_, bool>(7)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
        }).await?;
        for (run, sequence, agent, observed, aborted, confirmed, actual, resumed) in rows {
            let Some(key) = confirmed else {
                let state = self.inner.engine.load(&run).await?;
                if state.status == Status::Running
                    && actual == sequence
                    && self.live_driver(&agent).await
                {
                    // The CAS may still apply on a later detection pass.
                    continue;
                }
                self.record_watchdog_decision(
                    &run,
                    sequence,
                    None,
                    "not_applied",
                    "the run progressed or changed before the interruption",
                )
                .await?;
                continue;
            };
            let action = if key.starts_with("watchdog-blocked:") {
                "blocked"
            } else {
                "paused"
            };
            // Wait for the old driver: cooperative stop first, then abort.
            let stuck = {
                let handles = self.inner.running.lock().await;
                match handles.get(&agent) {
                    Some(handle) if !handle.is_finished() => {
                        if !aborted && at >= observed.saturating_add(ABORT_GRACE_SECONDS) {
                            handle.abort();
                            let (r, s) = (run.clone(), sequence);
                            self.inner.db.write(Priority::Journal, move |c| {
                                c.execute("UPDATE orchestration_run_watchdog SET aborted=1 WHERE run_id=?1 AND sequence=?2", rusqlite::params![r, s])?;
                                Ok(())
                            }).await?;
                        }
                        Some(at >= observed.saturating_add(ABORT_GIVE_UP_SECONDS))
                    }
                    _ => None,
                }
            };
            match stuck {
                Some(false) => continue,
                Some(true) => {
                    self.record_watchdog_decision(
                        &run,
                        sequence,
                        Some(action),
                        action,
                        "the stalled driver did not stop after abort; automatic resume refused",
                    )
                    .await?;
                    continue;
                }
                None => {}
            }
            // A finished but unreaped driver is reaped before inspection.
            self.reap().await?;
            let state = self.inner.engine.load(&run).await?;
            let (decision, reason) = if resumed {
                (
                    "resumed",
                    "eligible after watchdog reconciliation".to_owned(),
                )
            } else if action == "blocked" {
                if state.status == Status::Blocked
                    && state.reason.as_deref() == Some(WATCHDOG_BLOCK_REASON)
                {
                    self.watchdog_note(&run, sequence, "blocked", WATCHDOG_BLOCK_REASON)
                        .await?;
                    ("blocked", WATCHDOG_BLOCK_REASON.to_owned())
                } else {
                    ("superseded", superseded(&state))
                }
            } else if state.status == Status::Paused
                && state.reason.as_deref() == Some(WATCHDOG_PAUSE_REASON)
            {
                let host = self
                    .inner
                    .host_autonomy
                    .lock()
                    .unwrap()
                    .unwrap_or(Autonomy::Supervised);
                let mut blockers = self.assess_resume(&run, &state).await?.blockers(host);
                let recovery = self.recovery().status(at).await?;
                if recovery.recovering || recovery.safe_mode {
                    blockers.push("startup recovery or safe mode is active");
                }
                if recovery.quarantined.contains(&run) {
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
                    let reason = "eligible after watchdog reconciliation";
                    self.watchdog_note(&run, sequence, "resumed", reason)
                        .await?;
                    if !self
                        .inner
                        .engine
                        .resume_watchdog_pause(&run, sequence)
                        .await?
                    {
                        // A concurrent control won; re-evaluate on the next pass.
                        continue;
                    }
                    ("resumed", reason.to_owned())
                } else {
                    let reason = blockers.join("; ");
                    self.watchdog_note(&run, sequence, "paused", &reason)
                        .await?;
                    ("paused", reason)
                }
            } else {
                ("superseded", superseded(&state))
            };
            self.record_watchdog_decision(&run, sequence, Some(action), decision, &reason)
                .await?;
        }
        Ok(())
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
    async fn publish_watchdog_notices(&self) -> Result<()> {
        let published = self.inner.db.write(Priority::Journal, |c| {
            let mut q = c.prepare("SELECT w.run_id,w.sequence,w.agent_id,w.observed_at,w.action,w.decision,w.reason,json_extract(a.body,'$.origin_conversation'),json_extract(a.body,'$.scope') FROM orchestration_run_watchdog w JOIN orchestration_agents a ON a.id=w.agent_id WHERE w.notified=0 AND w.decision IS NOT NULL ORDER BY w.observed_at,w.run_id LIMIT ?1")?;
            let rows = q.query_map([BATCH], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, u64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(q);
            let mut published = 0usize;
            for (run, sequence, agent, observed, action, decision, reason, conversation, scope) in rows {
                let action = action.unwrap_or_default();
                let reason = reason.unwrap_or_default();
                let class = if decision == "resumed" { "normal" } else { "urgent" };
                let text = format!("Run {run} was declared stalled after at least ten minutes without a heartbeat and was interrupted ({action}). Decision: {decision}: {reason}. No uncertain effect was retried automatically.");
                let content = json!({"kind":"run_watchdog","run_id":run,"agent_id":agent,"sequence":sequence,"observed_at":observed,"action":action,"decision":decision,"reason":reason,"origin_conversation":conversation,"scope":scope,"text":text});
                let id = format!("ntf_{}", digest(format!("watchdog:{run}:{sequence}").as_bytes()));
                c.execute("INSERT OR IGNORE INTO orchestration_notifications(id,agent_id,conversation,class,content,created,acknowledged) VALUES(?1,?2,?3,?4,?5,?6,0)", rusqlite::params![id, agent, conversation.unwrap_or_default(), class, content.to_string(), observed])?;
                c.execute("UPDATE orchestration_run_watchdog SET notified=1 WHERE run_id=?1 AND sequence=?2", rusqlite::params![run, sequence])?;
                published += 1;
            }
            Ok(published)
        }).await?;
        if published > 0 {
            self.inner.wake.notify_waiters();
        }
        Ok(())
    }
}

fn superseded(state: &excalibur_engine::Snapshot) -> String {
    format!(
        "a later control or engine transition owns the run: {}({})",
        label(&state.status),
        state.reason.as_deref().unwrap_or("none")
    )
}

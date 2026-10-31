//! Time-based retention classes of §4.7, evaluated against an injectable clock.
//! Detailed telemetry expiry is applied here; journal compaction and tool-output blobs
//! are reported as due so the host can run the compaction step it owns.
use crate::{
    Error, Result, install,
    sweep::{guarded, table_exists, with_secure_delete},
};
use excalibur_store::{
    Database, Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use excalibur_telemetry::Telemetry;
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc};

/// Unix seconds supplied by the host; tests pass a manual clock.
pub type RetentionClock = Arc<dyn Fn() -> i64 + Send + Sync>;
const DAY: i64 = 86_400;
const EXPIRY_BATCH: i64 = 1000;

/// Defaults from the §4.7 retention table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Full durable journal of terminal runs before compaction becomes due.
    pub journal_full_days: i64,
    /// Tool outputs stored in blobs, after the run ends.
    pub tool_output_days: i64,
    /// Detailed telemetry events, traces and metrics; daily aggregates are kept.
    pub telemetry_detail_days: i64,
    /// Shadow snapshots and worktrees after their task closes (owned by the workspace).
    pub shadow_snapshot_days: i64,
}
impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            journal_full_days: 30,
            tool_output_days: 30,
            telemetry_detail_days: 90,
            shadow_snapshot_days: 7,
        }
    }
}
impl RetentionPolicy {
    fn validate(&self) -> Result<()> {
        for days in [
            self.journal_full_days,
            self.tool_output_days,
            self.telemetry_detail_days,
            self.shadow_snapshot_days,
        ] {
            if !(1..=3650).contains(&days) {
                return Err(Error::Invalid("retention windows must be 1..3650 days"));
            }
        }
        Ok(())
    }
}
/// Runs kept complete regardless of age (§4.7, §12.6, §23.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldReason {
    Evaluation,
    Replay,
    SkillCandidate,
    SkillEvaluation,
}
impl HoldReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Evaluation => "evaluation",
            Self::Replay => "replay",
            Self::SkillCandidate => "skill_candidate",
            Self::SkillEvaluation => "skill_evaluation",
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExpiryReport {
    pub now: i64,
    pub telemetry_covered: bool,
    pub telemetry_events_expired: u64,
    pub telemetry_traces_expired: u64,
    pub telemetry_metrics_expired: u64,
    pub telemetry_runs_reexported: u64,
    /// Terminal runs older than the full-journal window and not held.
    pub journal_compaction_due: Vec<String>,
    /// Terminal runs past the window that a hold keeps complete.
    pub journal_held: Vec<String>,
    /// Journal payload blobs of due runs that no evidence, document or memory references.
    pub tool_output_blobs_due: u64,
}

#[derive(Clone)]
pub struct RetentionSweeper {
    main: Database,
    telemetry: Option<Telemetry>,
    policy: RetentionPolicy,
    clock: RetentionClock,
}
impl std::fmt::Debug for RetentionSweeper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetentionSweeper")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}
impl RetentionSweeper {
    pub async fn open(
        main: Database,
        telemetry: Option<Telemetry>,
        policy: RetentionPolicy,
        clock: RetentionClock,
    ) -> Result<Self> {
        policy.validate()?;
        install(&main).await?;
        Ok(Self {
            main,
            telemetry,
            policy,
            clock,
        })
    }
    pub fn policy(&self) -> RetentionPolicy {
        self.policy
    }
    /// Keep a run's journal complete (evaluation, replay or skill episodes).
    pub async fn hold_run(&self, run: &str, reason: HoldReason) -> Result<()> {
        if run.trim().is_empty() || run.len() > 512 {
            return Err(Error::Invalid("run required"));
        }
        let (run, now) = (run.to_string(), (self.clock)());
        self.main
            .write(Priority::Normal, move |c| {
                c.execute(
                    "INSERT INTO retention_holds(run_id,reason,held_at) VALUES(?1,?2,?3) ON CONFLICT(run_id) DO UPDATE SET reason=excluded.reason",
                    rusqlite::params![run, reason.as_str(), now],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    pub async fn release_hold(&self, run: &str) -> Result<()> {
        let run = run.to_string();
        self.main
            .write(Priority::Normal, move |c| {
                c.execute("DELETE FROM retention_holds WHERE run_id=?1", [run])?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    /// Evaluate every class once at the clock's current time. Idempotent.
    pub async fn run_once(&self) -> Result<ExpiryReport> {
        let now = (self.clock)();
        let mut report = ExpiryReport {
            now,
            ..ExpiryReport::default()
        };
        let journal_cutoff = now - self.policy.journal_full_days * DAY;
        let (due, held, blobs) = self
            .main
            .read(ReadLane::Background, move |c| {
                if !table_exists(c, "journal")? {
                    return Ok((Vec::new(), Vec::new(), 0));
                }
                let mut statement = c.prepare(
                    "SELECT j.run_id,max(j.created_at),(SELECT json_extract(s.payload,'$.state') FROM journal s WHERE s.run_id=j.run_id AND s.kind='state_change' ORDER BY s.sequence DESC LIMIT 1),EXISTS(SELECT 1 FROM retention_holds h WHERE h.run_id=j.run_id) FROM journal j WHERE j.run_id IS NOT NULL GROUP BY j.run_id ORDER BY j.run_id",
                )?;
                let rows = statement
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, bool>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(statement);
                let (mut due, mut held) = (Vec::new(), Vec::new());
                for (run, last, state, hold) in rows {
                    let terminal = matches!(
                        state.as_deref(),
                        Some("completed" | "failed" | "cancelled")
                    );
                    if !terminal || last >= journal_cutoff {
                        continue;
                    }
                    if hold { held.push(run) } else { due.push(run) }
                }
                let runs = Value::Array(due.iter().cloned().map(Value::String).collect()).to_string();
                let blobs: i64 = c.query_row(
                    "SELECT count(DISTINCT j.payload_blob) FROM journal j WHERE j.payload_blob IS NOT NULL AND j.run_id IN (SELECT value FROM json_each(?1)) AND NOT EXISTS(SELECT 1 FROM blob_refs r WHERE r.hash=j.payload_blob AND r.owner_type IN ('evidence','documents','document_versions','memories','messages'))",
                    [runs],
                    |r| r.get(0),
                )?;
                Ok((due, held, blobs as u64))
            })
            .await?;
        report.journal_compaction_due = due;
        report.journal_held = held;
        report.tool_output_blobs_due = blobs;
        let Some(telemetry) = &self.telemetry else {
            return Ok(report);
        };
        report.telemetry_covered = true;
        let tdb = telemetry.database().clone();
        let cutoff = now - self.policy.telemetry_detail_days * DAY;
        let mut runs = BTreeSet::new();
        loop {
            let (count, touched) = tdb
                .write(Priority::Background, move |c| {
                    with_secure_delete(c, || {
                        if !table_exists(c, "events")? {
                            return Ok((0u64, Vec::new()));
                        }
                        let mut statement = c.prepare(
                            "SELECT id,run_id FROM events WHERE created_at<?1 ORDER BY rowid LIMIT ?2",
                        )?;
                        let rows = statement
                            .query_map(rusqlite::params![cutoff, EXPIRY_BATCH], |r| {
                                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                            })?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        drop(statement);
                        if rows.is_empty() {
                            return Ok((0, Vec::new()));
                        }
                        let ids = Value::Array(
                            rows.iter().map(|(id, _)| Value::String(id.clone())).collect(),
                        )
                        .to_string();
                        if table_exists(c, "telemetry_offsets")? {
                            guarded(c, "telemetry_offsets", "DELETE", || {
                                Ok(c.execute(
                                    "DELETE FROM telemetry_offsets WHERE event_id IN (SELECT value FROM json_each(?1))",
                                    [&ids],
                                )?)
                            })?;
                        }
                        let deleted = guarded(c, "events", "DELETE", || {
                            Ok(c.execute(
                                "DELETE FROM events WHERE id IN (SELECT value FROM json_each(?1))",
                                [&ids],
                            )?)
                        })?;
                        Ok((
                            deleted as u64,
                            rows.into_iter().filter_map(|(_, run)| run).collect(),
                        ))
                    })
                })
                .await?;
            if count == 0 {
                break;
            }
            report.telemetry_events_expired += count;
            runs.extend(touched);
        }
        for (table, slot) in [
            ("traces", &mut report.telemetry_traces_expired),
            ("metrics", &mut report.telemetry_metrics_expired),
        ] {
            loop {
                let deleted = tdb
                    .write(Priority::Background, move |c| {
                        with_secure_delete(c, || {
                            if !table_exists(c, table)? {
                                return Ok(0u64);
                            }
                            Ok(c.execute(
                                &format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE created_at<?1 LIMIT ?2)"),
                                rusqlite::params![cutoff, EXPIRY_BATCH],
                            )? as u64)
                        })
                    })
                    .await?;
                if deleted == 0 {
                    break;
                }
                *slot += deleted;
            }
        }
        for run in runs {
            let probe = run.clone();
            let registered: Option<i64> = tdb
                .read(ReadLane::Interactive, move |c| {
                    Ok(c.query_row(
                        "SELECT 1 FROM telemetry_runs WHERE run_id=?1",
                        [probe],
                        |r| r.get(0),
                    )
                    .optional()?)
                })
                .await?;
            if registered.is_some() {
                telemetry.export_run(&run).await?;
                report.telemetry_runs_reexported += 1;
            }
        }
        Ok(report)
    }
}

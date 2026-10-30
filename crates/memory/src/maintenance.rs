//! Internal memory jobs run by the host in the background lane: `memory-expiry`, the
//! embedding indexing queue and `embedding-reindex`, segment closing after inactivity, and
//! `daily-memory-review` in the low-activity window (the window and the weekly
//! `activity-histogram` come from `plan_review`). A tick does bounded work and records when
//! each job last ran; the review itself checkpoints and yields to the user.
use crate::{
    ConsolidationRoles, MemoryStore, Result, ReviewBudget, ReviewOutcome, ScheduleDecision,
    UserActivity, MemoryGrant, placeholders,
};
use excalibur_store::{
    Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct MaintenanceConfig {
    pub principal: String,
    pub timezone: chrono_tz::Tz,
    /// Every scope the host manages; each job still works one scope at a time.
    pub scopes: Vec<String>,
    pub budget: ReviewBudget,
    pub queue_batch: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MaintenanceReport {
    pub expired: usize,
    pub indexed: usize,
    pub index_remaining: usize,
    pub migrations_finished: Vec<String>,
    pub segments_closed: Vec<String>,
    pub admitted: usize,
    pub schedule: Option<ScheduleDecision>,
    pub review: Option<ReviewOutcome>,
    pub skipped: Vec<String>,
}

impl MemoryStore {
    async fn job_mark(&self, principal: &str, job: &str, detail: Value, now: i64) -> Result<()> {
        let (principal, job) = (principal.to_string(), job.to_string());
        self.db
            .write(Priority::Background, move |c| {
                c.execute(
                    "INSERT INTO memory_maintenance(principal_id,job,last_at,detail) VALUES(?1,?2,?3,?4) ON CONFLICT(principal_id,job) DO UPDATE SET last_at=excluded.last_at,detail=excluded.detail",
                    rusqlite::params![principal, job, now, detail.to_string()],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    /// Latest review reports, restricted to the granted scopes.
    pub async fn recent_reviews(&self, grant: &MemoryGrant, limit: usize) -> Result<Vec<Value>> {
        let scopes = grant.select(&[])?;
        let (principal, limit) = (grant.principal.clone(), limit.clamp(1, 50) as i64);
        let rows: Vec<(String, String, i64, Option<i64>, String)> = self
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut s = c.prepare("SELECT id,state,started_at,finished_at,report FROM consolidation_runs WHERE principal_id=?1 ORDER BY started_at DESC,id DESC LIMIT ?2")?;
                let rows = s
                    .query_map(rusqlite::params![principal, limit], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(|(id, state, started, finished, report)| {
                let report: Value = serde_json::from_str(&report).unwrap_or_default();
                let visible: serde_json::Map<String, Value> = report["scopes"]
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .filter(|(scope, _)| scopes.contains(scope))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                json!({"id": id, "state": state, "started_at": started, "finished_at": finished, "scopes": visible})
            })
            .collect())
    }
    /// Last run time of an internal job.
    pub async fn job_last_run(&self, principal: &str, job: &str) -> Result<Option<i64>> {
        let (principal, job) = (principal.to_string(), job.to_string());
        Ok(self
            .db
            .read(ReadLane::Background, move |c| {
                Ok(c.query_row(
                    "SELECT last_at FROM memory_maintenance WHERE principal_id=?1 AND job=?2",
                    rusqlite::params![principal, job],
                    |r| r.get(0),
                )
                .optional()?)
            })
            .await?)
    }
    /// Scopes whose newest message is older than the segment idle time and that have turns
    /// after their extraction watermark.
    async fn idle_segments(&self, principal: &str, scopes: &[String], now: i64) -> Result<Vec<String>> {
        let (principal, scopes, idle) = (
            principal.to_string(),
            scopes.to_vec(),
            self.config.segment_idle_seconds,
        );
        if scopes.is_empty() {
            return Ok(vec![]);
        }
        Ok(self
            .db
            .read(ReadLane::Background, move |c| {
                let sql = format!(
                    "SELECT m.scope_id,max(m.sent_at),max(m.rowid),COALESCE((SELECT last_rowid FROM memory_watermarks w WHERE w.principal_id=?1 AND w.scope_id=m.scope_id),0) FROM messages m WHERE m.principal_id=?1 AND m.scope_id IN ({}) GROUP BY m.scope_id ORDER BY m.scope_id",
                    placeholders(2, scopes.len())
                );
                let mut s = c.prepare(&sql)?;
                let mut params: Vec<&dyn rusqlite::ToSql> = vec![&principal];
                for scope in &scopes {
                    params.push(scope);
                }
                let rows = s
                    .query_map(params.as_slice(), |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows
                    .into_iter()
                    .filter(|(_, last, rowid, mark)| now - last >= idle && rowid > mark)
                    .map(|(scope, ..)| scope)
                    .collect())
            })
            .await?)
    }

    /// One bounded maintenance pass. Model-backed steps are skipped (and reported) when no
    /// roles are configured; deterministic jobs always run.
    pub async fn maintenance_tick(
        &self,
        config: &MaintenanceConfig,
        roles: Option<&ConsolidationRoles>,
        activity: &dyn UserActivity,
        now: i64,
    ) -> Result<MaintenanceReport> {
        let mut report = MaintenanceReport::default();
        if config.scopes.is_empty() {
            return Ok(report);
        }
        self.ensure_principal(&config.principal).await?;
        for scope in &config.scopes {
            report.expired += self.expire(&config.principal, scope, now).await?;
        }
        self.job_mark(&config.principal, "memory-expiry", json!({"expired": report.expired}), now)
            .await?;
        let queue = self
            .embeddings
            .process_queue(config.queue_batch.max(1), now)
            .await?;
        report.indexed = queue.indexed;
        report.index_remaining = queue.remaining;
        if queue.remaining == 0 {
            for space in self.embeddings.spaces().await? {
                if space.state == "migrating"
                    && self.embeddings.finish_migration(&space.id, now).await.is_ok()
                {
                    report.migrations_finished.push(space.id);
                }
            }
            if !report.migrations_finished.is_empty() {
                self.job_mark(
                    &config.principal,
                    "embedding-reindex",
                    json!({"finished": report.migrations_finished}),
                    now,
                )
                .await?;
            }
        }
        let Some(roles) = roles else {
            report.skipped.push("model roles not configured".into());
            return Ok(report);
        };
        if !activity.active() {
            let grant = MemoryGrant::new(config.principal.clone(), config.scopes.iter().cloned())
                .with_timezone(config.timezone);
            for scope in self
                .idle_segments(&config.principal, &config.scopes, now)
                .await?
            {
                if activity.active() {
                    break;
                }
                match self
                    .close_segment(&grant, &scope, roles.extractor.as_ref(), now)
                    .await
                {
                    Ok(admitted) => {
                        report.admitted += admitted.len();
                        report.segments_closed.push(scope);
                    }
                    Err(error) => report.skipped.push(format!("segment {scope}: {error}")),
                }
            }
        }
        let schedule = self
            .plan_review(&config.principal, config.timezone, &config.budget, now)
            .await?;
        let last = self.job_last_run(&config.principal, "daily-memory-review").await?;
        let finished_recently = last.is_some_and(|t| now - t < 20 * 3600);
        if schedule.due_now && !finished_recently && !activity.active() {
            let outcome = self
                .run_review(
                    &config.principal,
                    &config.scopes,
                    roles,
                    &config.budget,
                    activity,
                    now,
                )
                .await?;
            if outcome.state == crate::ReviewState::Completed {
                self.job_mark(
                    &config.principal,
                    "daily-memory-review",
                    json!({"run": outcome.run_id}),
                    now,
                )
                .await?;
            }
            report.review = Some(outcome);
        }
        report.schedule = Some(schedule);
        Ok(report)
    }
}

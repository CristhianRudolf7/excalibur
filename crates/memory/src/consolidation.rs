//! Segment extraction and the daily background review (architecture 10.5 point 3 and 10.6).
//! Every model prompt contains a single scope; the review yields to user activity after each
//! batch and resumes from its durable checkpoint.
use crate::{
    BoxFuture, Error, MemoryGrant, MemoryInput, MemoryKind, MemoryStatus, MemoryStore, Result,
    SourceKind, SourceRef, Trust, digest,
    graph::{mentioned_entities, scope_entities},
    identity::Fragment,
    invalid, load_memory, new_id,
    text::normalize,
    transition,
};
use chrono::{Duration, NaiveTime, TimeZone, Timelike};
use excalibur_store::{
    Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub message_id: String,
    pub conversation: Option<String>,
    pub role: String,
    pub text: String,
    pub sent_at: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionRequest {
    pub scope: String,
    pub turns: Vec<Turn>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtractedFact {
    pub message_id: String,
    pub kind: MemoryKind,
    pub statement: String,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub attribute: Option<String>,
    #[serde(default)]
    pub valid_from: Option<i64>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub temporal_change: bool,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub importance: Option<f64>,
    /// Subject, relation, object for the contextual graph.
    #[serde(default)]
    pub relations: Vec<(String, String, String)>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtractionOutput {
    pub facts: Vec<ExtractedFact>,
    pub tokens_used: u64,
}
/// Extractor role (the cheapest model). Receives turns of exactly one scope.
pub trait Extractor: Send + Sync {
    fn extract<'a>(
        &'a self,
        request: &'a ExtractionRequest,
    ) -> BoxFuture<'a, std::result::Result<ExtractionOutput, String>>;
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compaction {
    pub compiled: String,
    /// Existing entry ids retained as separate entries.
    pub keep: Vec<String>,
    pub tokens_used: u64,
}
/// Note compactor producing a compiled truth for one scope's notebook section.
pub trait NoteCompactor: Send + Sync {
    fn compact<'a>(
        &'a self,
        scope: &'a str,
        entries: &'a [Fragment],
    ) -> BoxFuture<'a, std::result::Result<Compaction, String>>;
}
/// Live user-activity signal; the review yields when it reports activity.
pub trait UserActivity: Send + Sync {
    fn active(&self) -> bool;
}
impl UserActivity for std::sync::atomic::AtomicBool {
    fn active(&self) -> bool {
        self.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleMode {
    /// First days: light consolidation and a 03:30 local pass after two idle hours.
    Light,
    /// Lowest-activity contiguous window from the 14-day hourly histogram.
    Histogram,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleDecision {
    pub mode: ScheduleMode,
    pub active_days: usize,
    pub window_start_hour: u32,
    pub window_minute: u32,
    pub window_hours: u32,
    pub histogram: Vec<u32>,
    pub due_now: bool,
    pub next_start: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Running,
    Yielded,
    BudgetExhausted,
    Completed,
}
impl ReviewState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Yielded => "yielded",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Completed => "completed",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewOutcome {
    pub run_id: String,
    pub state: ReviewState,
    pub report: Value,
}

#[derive(Clone)]
pub struct ConsolidationRoles {
    pub extractor: Arc<dyn Extractor>,
    pub compactor: Option<Arc<dyn NoteCompactor>>,
}
#[derive(Clone, Debug)]
pub struct ReviewBudget {
    pub tokens_per_scope: u64,
    pub batch_size: usize,
    pub estimated_hours: u32,
}
impl Default for ReviewBudget {
    fn default() -> Self {
        Self {
            tokens_per_scope: 200_000,
            batch_size: 20,
            estimated_hours: 2,
        }
    }
}

const STEPS: &[&str] = &[
    "extract",
    "expire",
    "provenance",
    "contradictions",
    "graph",
    "compact",
    "profile",
    "skills",
];

fn trust_for(role: &str) -> Trust {
    match role {
        "user" => Trust::User,
        "assistant" | "agent" => Trust::Agent,
        _ => Trust::UntrustedExternal,
    }
}

impl MemoryStore {
    /// Host-reported user-initiated activity (runs started by the user) for the histogram.
    pub async fn record_activity(&self, principal: &str, at: i64, kind: &str) -> Result<()> {
        let (principal, kind) = (principal.to_string(), kind.to_string());
        self.db
            .write(Priority::Background, move |c| {
                c.execute(
                    "INSERT INTO memory_activity(principal_id,at,kind) VALUES(?1,?2,?3)",
                    rusqlite::params![principal, at, kind],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Whether a conversation segment closed by inactivity (30 minutes by default).
    pub async fn segment_idle(
        &self,
        grant: &MemoryGrant,
        conversation: &str,
        now: i64,
    ) -> Result<bool> {
        grant.validate()?;
        let (principal, conversation, idle) = (
            grant.principal.clone(),
            conversation.to_string(),
            self.config.segment_idle_seconds,
        );
        Ok(self
            .db
            .read(ReadLane::Background, move |c| {
                let last: Option<i64> = c.query_row("SELECT max(sent_at) FROM messages WHERE principal_id=?1 AND conversation_id=?2", rusqlite::params![principal, conversation], |r| r.get(0))?;
                Ok(last.is_some_and(|t| now - t >= idle))
            })
            .await?)
    }

    async fn activity(&self, principal: &str, since: i64) -> Result<Vec<i64>> {
        let principal = principal.to_string();
        Ok(self
            .db
            .read(ReadLane::Background, move |c| {
                let mut s = c.prepare("SELECT sent_at FROM messages WHERE principal_id=?1 AND role='user' AND sent_at>=?2 UNION ALL SELECT at FROM memory_activity WHERE principal_id=?1 AND at>=?2")?;
                let rows = s.query_map(rusqlite::params![principal, since], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }

    /// Decide the review window in the user's time zone; recomputed weekly.
    pub async fn plan_review(
        &self,
        principal: &str,
        tz: chrono_tz::Tz,
        budget: &ReviewBudget,
        now: i64,
    ) -> Result<ScheduleDecision> {
        let all = self.activity(principal, i64::MIN).await?;
        let local = |t: i64| tz.timestamp_opt(t, 0).single();
        let days: BTreeSet<_> = all
            .iter()
            .filter_map(|t| local(*t))
            .map(|d| d.date_naive())
            .collect();
        let now_local = local(now).ok_or_else(|| invalid("invalid time"))?;
        let last_activity = all.iter().copied().filter(|t| *t <= now).max();
        let mut histogram = vec![0u32; 24];
        for t in all
            .iter()
            .filter(|t| **t >= now - 14 * 86_400 && **t <= now)
        {
            if let Some(d) = local(*t) {
                histogram[d.hour() as usize] += 1;
            }
        }
        let next_at = |hour: u32, minute: u32| -> i64 {
            let today = now_local.date_naive();
            for offset in 0..3 {
                let date = today + Duration::days(offset);
                let naive = date.and_time(NaiveTime::from_hms_opt(hour, minute, 0).unwrap());
                if let Some(start) = tz.from_local_datetime(&naive).earliest()
                    && start.timestamp() >= now - 60
                {
                    return start.timestamp();
                }
            }
            now
        };
        if days.len() < 5 {
            let today_start = tz
                .from_local_datetime(
                    &now_local
                        .date_naive()
                        .and_time(NaiveTime::from_hms_opt(3, 30, 0).unwrap()),
                )
                .earliest()
                .map(|d| d.timestamp())
                .unwrap_or(now);
            let in_window = now >= today_start
                && now < today_start + 3600 * budget.estimated_hours.max(1) as i64;
            let idle = last_activity.is_none_or(|t| now - t >= 7200);
            return Ok(ScheduleDecision {
                mode: ScheduleMode::Light,
                active_days: days.len(),
                window_start_hour: 3,
                window_minute: 30,
                window_hours: budget.estimated_hours.max(1),
                histogram,
                due_now: in_window && idle,
                next_start: if in_window {
                    today_start
                } else {
                    next_at(3, 30)
                },
            });
        }
        let width = budget.estimated_hours.clamp(1, 12);
        let principal2 = principal.to_string();
        let stored = self
            .db
            .read(ReadLane::Background, move |c| {
                Ok(c.query_row("SELECT window_start_hour,window_hours,computed_at,mode,timezone FROM consolidation_schedules WHERE principal_id=?1", [&principal2], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))).optional()?)
            })
            .await?;
        let start = match stored {
            Some((start, hours, computed, mode, zone))
                if now - computed < 7 * 86_400
                    && hours == width
                    && mode == "histogram"
                    && zone == tz.name() =>
            {
                start
            }
            _ => {
                let mut best = (u32::MAX, 0u32);
                for start in 0..24u32 {
                    let load: u32 = (0..width)
                        .map(|i| histogram[((start + i) % 24) as usize])
                        .sum();
                    if load < best.0 {
                        best = (load, start);
                    }
                }
                let (principal3, zone) = (principal.to_string(), tz.name().to_string());
                self.db
                    .write(Priority::Background, move |c| {
                        c.execute("INSERT INTO consolidation_schedules(principal_id,timezone,mode,window_start_hour,window_hours,computed_at) VALUES(?1,?2,'histogram',?3,?4,?5) ON CONFLICT(principal_id) DO UPDATE SET timezone=excluded.timezone,mode=excluded.mode,window_start_hour=excluded.window_start_hour,window_hours=excluded.window_hours,computed_at=excluded.computed_at", rusqlite::params![principal3, zone, best.1, width, now])?;
                        Ok(())
                    })
                    .await?;
                best.1
            }
        };
        let hour = now_local.hour();
        let due = (0..width).any(|i| (start + i) % 24 == hour);
        Ok(ScheduleDecision {
            mode: ScheduleMode::Histogram,
            active_days: days.len(),
            window_start_hour: start,
            window_minute: 0,
            window_hours: width,
            histogram,
            due_now: due,
            next_start: next_at(start, 0),
        })
    }

    async fn turns_after(
        &self,
        principal: &str,
        scope: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<(i64, Turn)>> {
        let (principal, scope) = (principal.to_string(), scope.to_string());
        Ok(self
            .db
            .read(ReadLane::Background, move |c| {
                let mut s = c.prepare("SELECT rowid,id,conversation_id,role,text,sent_at FROM messages WHERE principal_id=?1 AND scope_id=?2 AND rowid>?3 ORDER BY rowid LIMIT ?4")?;
                let rows = s
                    .query_map(rusqlite::params![principal, scope, after, limit as i64], |r| Ok((r.get(0)?, Turn { message_id: r.get(1)?, conversation: r.get(2)?, role: r.get(3)?, text: r.get(4)?, sent_at: r.get(5)? })))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }
    async fn watermark(&self, principal: &str, scope: &str) -> Result<i64> {
        let (principal, scope) = (principal.to_string(), scope.to_string());
        Ok(self
            .db
            .read(ReadLane::Background, move |c| {
                Ok(c.query_row("SELECT last_rowid FROM memory_watermarks WHERE principal_id=?1 AND scope_id=?2", rusqlite::params![principal, scope], |r| r.get(0)).optional()?.unwrap_or(0))
            })
            .await?)
    }

    /// Extract and admit one batch of a single scope. Returns (turns, tokens, admitted ids).
    async fn extract_batch(
        &self,
        principal: &str,
        scope: &str,
        run: Option<&str>,
        extractor: &dyn Extractor,
        batch: usize,
        now: i64,
    ) -> Result<(usize, u64, Vec<String>)> {
        let after = self.watermark(principal, scope).await?;
        let rows = self.turns_after(principal, scope, after, batch).await?;
        if rows.is_empty() {
            return Ok((0, 0, Vec::new()));
        }
        let last = rows.last().map(|(rowid, _)| *rowid).unwrap_or(after);
        let turns: Vec<Turn> = rows.into_iter().map(|(_, t)| t).collect();
        let request = ExtractionRequest {
            scope: scope.to_string(),
            turns: turns.clone(),
        };
        let prompt_hash = digest(&serde_json::to_vec(&request)?);
        let (run2, scope2, hash2) = (
            run.unwrap_or("segment").to_string(),
            scope.to_string(),
            prompt_hash,
        );
        self.db
            .write(Priority::Background, move |c| {
                c.execute("INSERT INTO consolidation_prompts(run_id,scope_id,step,scopes,prompt_hash,at) VALUES(?1,?2,'extract',?3,?4,?5)", rusqlite::params![run2, scope2, json!([scope2]).to_string(), hash2, now])?;
                Ok(())
            })
            .await?;
        let output = extractor
            .extract(&request)
            .await
            .map_err(Error::Unavailable)?;
        let by_id: BTreeMap<&str, &Turn> =
            turns.iter().map(|t| (t.message_id.as_str(), t)).collect();
        let grant = MemoryGrant::new(principal, [scope]).with_max_trust(Trust::User);
        let mut admitted = Vec::new();
        for fact in output.facts {
            // Facts must point to a turn of this batch; anything else is dropped.
            let Some(turn) = by_id.get(fact.message_id.as_str()) else {
                continue;
            };
            let mut input = MemoryInput::new(scope, fact.kind, fact.statement.clone());
            input.entities = fact.entities.clone();
            input.attribute = fact.attribute.clone();
            input.valid_from = fact.valid_from;
            input.expires_at = fact.expires_at;
            input.temporal_change = fact.temporal_change;
            input.confidence = fact.confidence;
            input.importance = fact.importance;
            input.trust = trust_for(&turn.role);
            input.sources = vec![SourceRef {
                kind: if input.trust == Trust::UntrustedExternal {
                    SourceKind::Page
                } else {
                    SourceKind::Message
                },
                id: turn.message_id.clone(),
                context: turn.conversation.clone(),
            }];
            if input.trust == Trust::UntrustedExternal {
                input
                    .sources
                    .push(SourceRef::message(turn.message_id.clone()));
            }
            let (candidate, _, _) = self.insert_candidate(&grant, input, false, now).await?;
            let result = self.admit(principal, &candidate, now).await?;
            self.add_message_key(
                principal,
                scope,
                &turn.message_id,
                Some(&result.memory_id),
                &fact.statement,
                now,
            )
            .await?;
            for (subject, relation, object) in &fact.relations {
                self.relate(
                    &grant,
                    scope,
                    subject,
                    relation,
                    object,
                    fact.valid_from,
                    &result.memory_id,
                    now,
                )
                .await?;
            }
            admitted.push(result.memory_id);
        }
        let (principal2, scope2) = (principal.to_string(), scope.to_string());
        self.db
            .write(Priority::Background, move |c| {
                c.execute("INSERT INTO memory_watermarks(principal_id,scope_id,last_rowid,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(principal_id,scope_id) DO UPDATE SET last_rowid=max(last_rowid,excluded.last_rowid),updated_at=excluded.updated_at", rusqlite::params![principal2, scope2, last, now])?;
                Ok(())
            })
            .await?;
        Ok((turns.len(), output.tokens_used, admitted))
    }

    /// Light consolidation when a segment closes: extract and admit new turns of one scope.
    pub async fn close_segment(
        &self,
        grant: &MemoryGrant,
        scope: &str,
        extractor: &dyn Extractor,
        now: i64,
    ) -> Result<Vec<String>> {
        grant.require(scope)?;
        let mut all = Vec::new();
        loop {
            let (turns, _, admitted) = self
                .extract_batch(&grant.principal, scope, None, extractor, 50, now)
                .await?;
            all.extend(admitted);
            if turns == 0 {
                return Ok(all);
            }
        }
    }

    /// Run or resume the background review for the given scopes.
    pub async fn run_review(
        &self,
        principal: &str,
        scopes: &[String],
        roles: &ConsolidationRoles,
        budget: &ReviewBudget,
        activity: &dyn UserActivity,
        now: i64,
    ) -> Result<ReviewOutcome> {
        if scopes.is_empty() {
            return Err(invalid("review needs explicit scopes"));
        }
        self.ensure_principal(principal).await?;
        let principal2 = principal.to_string();
        let resumed: Option<(String, String, String)> = self
            .db
            .read(ReadLane::Background, move |c| {
                Ok(c.query_row("SELECT id,checkpoint,report FROM consolidation_runs WHERE principal_id=?1 AND state IN ('running','yielded','budget_exhausted') ORDER BY started_at DESC LIMIT 1", [&principal2], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?)
            })
            .await?;
        let (run, mut checkpoint, mut report): (String, Value, Value) = match resumed {
            Some((id, checkpoint, report)) => (
                id,
                serde_json::from_str(&checkpoint)?,
                serde_json::from_str(&report)?,
            ),
            None => {
                let id = new_id("review");
                let (id2, principal3) = (id.clone(), principal.to_string());
                self.db
                    .write(Priority::Background, move |c| {
                        c.execute("INSERT INTO consolidation_runs(id,principal_id,state,started_at,updated_at) VALUES(?1,?2,'running',?3,?3)", rusqlite::params![id2, principal3, now])?;
                        Ok(())
                    })
                    .await?;
                (id, json!({}), json!({"scopes": {}}))
            }
        };
        let mut state = ReviewState::Completed;
        'scopes: for scope in scopes {
            let mut progress = checkpoint
                .get(scope)
                .cloned()
                .unwrap_or_else(|| json!({"step": 0, "tokens": 0, "batches": 0}));
            let mut scope_report = report["scopes"]
                .get(scope)
                .cloned()
                .unwrap_or_else(|| json!({}));
            loop {
                let step = progress["step"].as_u64().unwrap_or(0) as usize;
                if step >= STEPS.len() {
                    break;
                }
                if activity.active() {
                    state = ReviewState::Yielded;
                    checkpoint[scope] = progress;
                    report["scopes"][scope] = scope_report;
                    break 'scopes;
                }
                let tokens = progress["tokens"].as_u64().unwrap_or(0);
                if tokens >= budget.tokens_per_scope {
                    state = ReviewState::BudgetExhausted;
                    checkpoint[scope] = progress;
                    report["scopes"][scope] = scope_report;
                    break 'scopes;
                }
                let mut advance = true;
                match STEPS[step] {
                    "extract" => {
                        let (turns, used, admitted) = self
                            .extract_batch(
                                principal,
                                scope,
                                Some(&run),
                                roles.extractor.as_ref(),
                                budget.batch_size.max(1),
                                now,
                            )
                            .await?;
                        // Admit proposals made during work in this scope.
                        let pending = if turns == 0 {
                            self.admit_pending_candidates(principal, scope, now).await?
                        } else {
                            Vec::new()
                        };
                        progress["tokens"] = json!(tokens + used);
                        progress["batches"] = json!(progress["batches"].as_u64().unwrap_or(0) + 1);
                        let total = scope_report["admitted"].as_u64().unwrap_or(0)
                            + (admitted.len() + pending.len()) as u64;
                        scope_report["admitted"] = json!(total);
                        scope_report["turns"] =
                            json!(scope_report["turns"].as_u64().unwrap_or(0) + turns as u64);
                        advance = turns == 0;
                    }
                    "expire" => {
                        scope_report["expired"] = json!(self.expire(principal, scope, now).await?)
                    }
                    "provenance" => {
                        let (retired, review) =
                            self.verify_provenance(principal, scope, now).await?;
                        scope_report["retired"] = json!(retired);
                        scope_report["provenance_review"] = json!(review);
                    }
                    "contradictions" => {
                        let (resolved, disputed) =
                            self.resolve_contradictions(principal, scope, now).await?;
                        scope_report["contradictions_resolved"] = json!(resolved);
                        scope_report["contradictions_disputed"] = json!(disputed);
                    }
                    "graph" => {
                        scope_report["graph_links"] =
                            json!(self.link_mentions(principal, scope).await?)
                    }
                    "compact" => {
                        if let Some(compactor) = &roles.compactor {
                            let (outcome, used) = self
                                .compact_notebook(principal, scope, &run, compactor.as_ref(), now)
                                .await?;
                            progress["tokens"] = json!(tokens + used);
                            scope_report["compaction"] = outcome;
                        } else {
                            scope_report["compaction"] = json!("compactor not configured");
                        }
                    }
                    "profile" => {
                        scope_report["profile_proposals"] = json!(
                            self.propose_profile_from_preferences(principal, scope, now)
                                .await?
                        )
                    }
                    "skills" => {
                        scope_report["skill_candidates"] =
                            json!(self.skill_candidates(principal, scope).await?)
                    }
                    _ => {}
                }
                if advance {
                    progress["step"] = json!(step + 1);
                }
                checkpoint[scope] = progress.clone();
                report["scopes"][scope] = scope_report.clone();
                self.save_review(&run, ReviewState::Running, &checkpoint, &report, now)
                    .await?;
            }
        }
        if state == ReviewState::Completed {
            report["relation_types_proposed"] =
                json!(self.consolidate_relation_types(principal, now).await?);
            report["profile_applied"] = json!(
                self.confirm_profile_proposals(
                    &MemoryGrant::new(principal, scopes.iter().cloned()),
                    None,
                    now
                )
                .await?
            );
        }
        report["state"] = json!(state.as_str());
        self.save_review(&run, state, &checkpoint, &report, now)
            .await?;
        Ok(ReviewOutcome {
            run_id: run,
            state,
            report,
        })
    }

    async fn save_review(
        &self,
        run: &str,
        state: ReviewState,
        checkpoint: &Value,
        report: &Value,
        now: i64,
    ) -> Result<()> {
        let (run, checkpoint, report) =
            (run.to_string(), checkpoint.to_string(), report.to_string());
        self.db
            .write(Priority::Background, move |c| {
                c.execute("UPDATE consolidation_runs SET state=?2,checkpoint=?3,report=?4,updated_at=?5,finished_at=CASE WHEN ?2='completed' THEN ?5 ELSE finished_at END WHERE id=?1", rusqlite::params![run, state.as_str(), checkpoint, report, now])?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Stored review report, readable by the user.
    pub async fn review_report(&self, grant: &MemoryGrant, run: &str) -> Result<Option<Value>> {
        grant.validate()?;
        let (run, principal) = (run.to_string(), grant.principal.clone());
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                let report: Option<String> = c
                    .query_row(
                        "SELECT report FROM consolidation_runs WHERE id=?1 AND principal_id=?2",
                        rusqlite::params![run, principal],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok(report.and_then(|r| serde_json::from_str(&r).ok()))
            })
            .await?)
    }

    /// Scopes named in every recorded prompt of a review (for audit of scope isolation).
    pub async fn review_prompt_scopes(&self, run: &str) -> Result<Vec<(String, Vec<String>)>> {
        let run = run.to_string();
        Ok(self
            .db
            .read(ReadLane::Background, move |c| {
                let mut s = c.prepare(
                    "SELECT scope_id,scopes FROM consolidation_prompts WHERE run_id=?1 ORDER BY id",
                )?;
                let rows = s
                    .query_map([&run], |r| {
                        let scopes: String = r.get(1)?;
                        Ok((r.get(0)?, serde_json::from_str(&scopes).unwrap_or_default()))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }

    async fn admit_pending_candidates(
        &self,
        principal: &str,
        scope: &str,
        now: i64,
    ) -> Result<Vec<String>> {
        let (p, s) = (principal.to_string(), scope.to_string());
        let ids: Vec<String> = self
            .db
            .read(ReadLane::Background, move |c| {
                let mut st = c.prepare("SELECT id FROM memories WHERE principal_id=?1 AND scope_id=?2 AND state='candidate' ORDER BY recorded_at,id LIMIT 200")?;
                let rows = st.query_map(rusqlite::params![p, s], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        let mut out = Vec::new();
        for id in ids {
            let result = self.admit(principal, &id, now).await?;
            if result.decision != crate::AdmissionDecision::Deferred {
                out.push(result.memory_id);
            }
        }
        Ok(out)
    }

    /// Record expiry as a state change; history and provenance stay.
    pub async fn expire(&self, principal: &str, scope: &str, now: i64) -> Result<usize> {
        let (principal, scope) = (principal.to_string(), scope.to_string());
        Ok(self
            .db
            .write(Priority::Background, move |c| {
                let mut s = c.prepare("SELECT id,state,expires_at FROM memories WHERE principal_id=?1 AND scope_id=?2 AND state IN ('active','disputed') AND expires_at IS NOT NULL AND expires_at<=?3")?;
                let rows = s.query_map(rusqlite::params![principal, scope, now], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                for (id, state, at) in &rows {
                    c.execute("UPDATE memories SET valid_to=COALESCE(valid_to,?2),invalidated_at=COALESCE(invalidated_at,?2) WHERE id=?1", rusqlite::params![id, at])?;
                    transition(c, id, MemoryStatus::parse(state).ok(), MemoryStatus::Expired, "expired", now)?;
                }
                Ok(rows.len())
            })
            .await?)
    }

    async fn verify_provenance(
        &self,
        principal: &str,
        scope: &str,
        now: i64,
    ) -> Result<(usize, Vec<String>)> {
        let (principal, scope) = (principal.to_string(), scope.to_string());
        Ok(self
            .db
            .write(Priority::Background, move |c| {
                let mut s = c.prepare("SELECT id FROM memories WHERE principal_id=?1 AND scope_id=?2 AND state IN ('active','disputed')")?;
                let ids = s.query_map(rusqlite::params![principal, scope], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                let mut retired = 0;
                let mut review = Vec::new();
                for id in ids {
                    let Some(m) = load_memory(c, &id)? else { continue };
                    let mut valid = 0;
                    for source in &m.sources {
                        let ok = match source.kind {
                            SourceKind::Message | SourceKind::Page => c.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1)", [&source.id], |r| r.get::<_, bool>(0))?,
                            SourceKind::Document => {
                                let current: Option<i64> = c.query_row("SELECT version FROM documents WHERE id=?1", [&source.id], |r| r.get(0)).optional()?;
                                if let (Some(current), Some(ctx)) = (current, source.context.as_deref())
                                    && ctx.trim_start_matches('v').parse::<i64>().is_ok_and(|v| v < current)
                                {
                                    review.push(m.id.clone());
                                }
                                current.is_some()
                            }
                            SourceKind::Memory => load_memory(c, &source.id)?.is_some(),
                            SourceKind::ToolCall => true,
                        };
                        if ok {
                            valid += 1;
                        }
                    }
                    if valid == 0 {
                        c.execute("UPDATE memories SET invalidated_at=COALESCE(invalidated_at,?2) WHERE id=?1", rusqlite::params![m.id, now])?;
                        transition(c, &m.id, Some(m.status), MemoryStatus::Retired, "no valid source", now)?;
                        retired += 1;
                    }
                }
                review.sort();
                review.dedup();
                Ok((retired, review))
            })
            .await?)
    }

    /// Several current records on the same entity attribute: the most recent explicit
    /// timestamped statement wins; otherwise all are disputed with a question.
    async fn resolve_contradictions(
        &self,
        principal: &str,
        scope: &str,
        now: i64,
    ) -> Result<(usize, usize)> {
        let (principal, scope) = (principal.to_string(), scope.to_string());
        Ok(self
            .db
            .write(Priority::Background, move |c| {
                let mut s = c.prepare("SELECT l.entity_id,m.attribute,m.id FROM memories m JOIN memory_entity_links l ON l.memory_id=m.id WHERE m.principal_id=?1 AND m.scope_id=?2 AND m.state IN ('active','disputed') AND m.attribute IS NOT NULL ORDER BY l.entity_id,m.attribute")?;
                let rows = s.query_map(rusqlite::params![principal, scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                let mut groups: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
                for (entity, attribute, id) in rows {
                    groups.entry((entity, attribute)).or_default().insert(id);
                }
                let (mut resolved, mut disputed) = (0, 0);
                let mut handled = BTreeSet::new();
                for ids in groups.into_values() {
                    let mut members = Vec::new();
                    for id in ids {
                        if !handled.contains(&id)
                            && let Some(m) = load_memory(c, &id)?
                            && matches!(m.status, MemoryStatus::Active | MemoryStatus::Disputed)
                        {
                            members.push(m);
                        }
                    }
                    let distinct: BTreeSet<String> = members.iter().map(|m| normalize(&m.statement)).collect();
                    if distinct.len() < 2 {
                        continue;
                    }
                    let latest = members.iter().filter(|m| m.valid_from.is_some()).max_by_key(|m| (m.valid_from, m.recorded_at));
                    let unique_latest = latest.filter(|l| members.iter().filter(|m| m.valid_from == l.valid_from).count() == 1);
                    if let Some(winner) = unique_latest.cloned() {
                        for m in members.iter().filter(|m| m.id != winner.id) {
                            c.execute("UPDATE memories SET superseded_by=?2,valid_to=COALESCE(valid_to,?3),invalidated_at=?4 WHERE id=?1", rusqlite::params![m.id, winner.id, winner.valid_from, now])?;
                            transition(c, &m.id, Some(m.status), MemoryStatus::Superseded, &format!("superseded by most recent explicit statement {}", winner.id), now)?;
                            handled.insert(m.id.clone());
                        }
                        if winner.status == MemoryStatus::Disputed {
                            transition(c, &winner.id, Some(winner.status), MemoryStatus::Active, "contradiction resolved by timestamp", now)?;
                        }
                        handled.insert(winner.id.clone());
                        resolved += 1;
                    } else {
                        let ids: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
                        for m in &members {
                            if m.status != MemoryStatus::Disputed {
                                transition(c, &m.id, Some(m.status), MemoryStatus::Disputed, "undecidable contradiction", now)?;
                            }
                            handled.insert(m.id.clone());
                        }
                        let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM memory_questions WHERE principal_id=?1 AND memory_ids=?2 AND state='open')", rusqlite::params![principal, json!(ids).to_string()], |r| r.get(0))?;
                        if !exists {
                            c.execute("INSERT INTO memory_questions(id,principal_id,scope_id,memory_ids,question,priority,state,created_at) VALUES(?1,?2,?3,?4,?5,'low','open',?6)", rusqlite::params![new_id("mq"), principal, scope, json!(ids).to_string(), format!("Which is current: {}?", members.iter().map(|m| format!("«{}»", m.statement)).collect::<Vec<_>>().join(" or ")), now])?;
                        }
                        disputed += 1;
                    }
                }
                Ok((resolved, disputed))
            })
            .await?)
    }

    /// Deterministic links from statements to known entity names and aliases.
    async fn link_mentions(&self, principal: &str, scope: &str) -> Result<usize> {
        let (principal, scope) = (principal.to_string(), scope.to_string());
        Ok(self
            .db
            .write(Priority::Background, move |c| {
                let entities = scope_entities(c, &principal, std::slice::from_ref(&scope))?;
                let mut s = c.prepare("SELECT id,statement FROM memories WHERE principal_id=?1 AND scope_id=?2 AND state IN ('active','disputed','candidate')")?;
                let rows = s.query_map(rusqlite::params![principal, scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                let mut added = 0;
                for (id, statement) in rows {
                    let found = mentioned_entities(&entities, &statement);
                    if found.is_empty() {
                        continue;
                    }
                    for entity in &found {
                        added += c.execute("INSERT OR IGNORE INTO memory_entity_links(memory_id,entity_id) VALUES(?1,?2)", rusqlite::params![id, entity])?;
                    }
                    let mut s2 = c.prepare("SELECT entity_id FROM memory_entity_links WHERE memory_id=?1 ORDER BY entity_id")?;
                    let all = s2.query_map([&id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                    c.execute("UPDATE memories SET entities=?2 WHERE id=?1", rusqlite::params![id, serde_json::to_string(&all).unwrap()])?;
                }
                // Contextual graph: each memory points to the messages, tool calls and
                // documents it was recorded from, and decisions point to the entities
                // they mention. Edge ids are deterministic, so repeated reviews add nothing.
                let edge_id = |parts: &[&str]| format!("edge_{}", &digest(parts.join("\0").as_bytes())[..32]);
                let mut s = c.prepare("SELECT id,sources FROM memories WHERE principal_id=?1 AND scope_id=?2 AND state IN ('active','disputed','candidate')")?;
                let rows = s.query_map(rusqlite::params![principal, scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                for (id, sources) in rows {
                    let sources: Vec<SourceRef> = serde_json::from_str(&sources).unwrap_or_default();
                    for source in sources {
                        let kind = match source.kind {
                            SourceKind::Message => "message",
                            SourceKind::ToolCall => "tool_call",
                            SourceKind::Document => "document",
                            SourceKind::Page => "page",
                            SourceKind::Memory => "memory",
                        };
                        added += c.execute(
                            "INSERT OR IGNORE INTO edges(id,layer,source_type,source_id,relation_type,destination_type,destination_id,provenance,principal_id,scope_id) VALUES(?1,'action','memory',?2,'recorded_from',?3,?4,'consolidation',?5,?6)",
                            rusqlite::params![edge_id(&["recorded_from", &id, kind, &source.id]), id, kind, source.id, principal, scope],
                        )?;
                    }
                }
                let mut s = c.prepare("SELECT id,statement FROM decisions WHERE principal_id=?1 AND scope_id=?2 AND state='active'")?;
                let decisions = s.query_map(rusqlite::params![principal, scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                for (id, statement) in decisions {
                    for entity in mentioned_entities(&entities, &statement) {
                        added += c.execute(
                            "INSERT OR IGNORE INTO edges(id,layer,source_type,source_id,relation_type,destination_type,destination_id,provenance,principal_id,scope_id) VALUES(?1,'memory','decision',?2,'decided','entity',?3,'consolidation',?4,?5)",
                            rusqlite::params![edge_id(&["decided", &id, &entity]), id, entity, principal, scope],
                        )?;
                    }
                }
                Ok(added)
            })
            .await?)
    }

    async fn compact_notebook(
        &self,
        principal: &str,
        scope: &str,
        run: &str,
        compactor: &dyn NoteCompactor,
        now: i64,
    ) -> Result<(Value, u64)> {
        let (document, entries) = self.notebook_section(principal, scope).await?;
        let entries: Vec<Fragment> = entries
            .into_iter()
            .filter(|f| f.heading_path != ["Compiled"])
            .collect();
        if entries.len() < 2 {
            return Ok((json!("nothing to compact"), 0));
        }
        let (run2, scope2, hash) = (
            run.to_string(),
            scope.to_string(),
            digest(&serde_json::to_vec(&entries)?),
        );
        self.db
            .write(Priority::Background, move |c| {
                c.execute("INSERT INTO consolidation_prompts(run_id,scope_id,step,scopes,prompt_hash,at) VALUES(?1,?2,'compact',?3,?4,?5)", rusqlite::params![run2, scope2, json!([scope2]).to_string(), hash, now])?;
                Ok(())
            })
            .await?;
        let compaction = compactor
            .compact(scope, &entries)
            .await
            .map_err(Error::Unavailable)?;
        let keep: BTreeSet<&str> = compaction.keep.iter().map(String::as_str).collect();
        let removed: Vec<Fragment> = entries
            .iter()
            .filter(|f| !keep.contains(f.id.as_str()))
            .cloned()
            .collect();
        let ratio = removed.len() as f64 / entries.len() as f64;
        if ratio > self.config.compaction_max_removed {
            return Ok((
                json!({"rejected": true, "removed_ratio": ratio, "entries": entries.len()}),
                compaction.tokens_used,
            ));
        }
        if removed.is_empty() {
            return Ok((
                json!({"rejected": false, "removed": 0}),
                compaction.tokens_used,
            ));
        }
        let count = removed.len();
        let compiled = self
            .apply_compaction(
                principal,
                scope,
                &document,
                removed,
                compaction.compiled,
                now,
            )
            .await?;
        Ok((
            json!({"rejected": false, "removed": count, "compiled_fragment": compiled}),
            compaction.tokens_used,
        ))
    }

    /// Consistent user preferences (repeated evidence) become user-profile proposals.
    async fn propose_profile_from_preferences(
        &self,
        principal: &str,
        scope: &str,
        now: i64,
    ) -> Result<usize> {
        let (p, s) = (principal.to_string(), scope.to_string());
        let rows: Vec<(String, String)> = self
            .db
            .read(ReadLane::Background, move |c| {
                let mut st = c.prepare("SELECT id,statement FROM memories WHERE principal_id=?1 AND scope_id=?2 AND kind='preference' AND trust='user' AND state='active'")?;
                let rows = st.query_map(rusqlite::params![p, s], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        let mut proposed = 0;
        for (id, statement) in rows {
            let id2 = id.clone();
            let Some(memory) = self
                .db
                .read(ReadLane::Background, move |c| Ok(load_memory(c, &id2)?))
                .await?
            else {
                continue;
            };
            let sources: BTreeSet<&str> = memory
                .sources
                .iter()
                .filter(|s| s.kind == SourceKind::Message)
                .map(|s| s.id.as_str())
                .collect();
            if sources.len() < self.config.profile_confirmation_evidence as usize {
                continue;
            }
            for source in sources {
                self.record_profile_evidence(
                    principal,
                    vec!["Preferences".into()],
                    &statement,
                    source,
                    now,
                )
                .await?;
            }
            proposed += 1;
        }
        Ok(proposed)
    }

    /// Repeated procedures are reported as skill-learning candidates (12.6 consumes them).
    async fn skill_candidates(&self, principal: &str, scope: &str) -> Result<Vec<String>> {
        let (p, s) = (principal.to_string(), scope.to_string());
        Ok(self
            .db
            .read(ReadLane::Background, move |c| {
                let mut st = c.prepare("SELECT id FROM memories WHERE principal_id=?1 AND scope_id=?2 AND kind='procedure' AND state='active' AND (json_array_length(sources)>=2 OR usage_count>=2) ORDER BY id")?;
                let rows = st.query_map(rusqlite::params![p, s], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }
}

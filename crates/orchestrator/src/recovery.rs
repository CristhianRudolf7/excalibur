//! Durable daemon incarnations and admission authority, independent from provider retries.
use crate::*;
use excalibur_security::Autonomy;
use excalibur_types::runtime::Effect;
use serde::{Deserialize, Serialize};

const WINDOW: i64 = 600;
const SAFE_FOR: i64 = 1800;
const MAX_DOWNTIME: i64 = 6 * 3600;

#[derive(Clone)]
pub struct RecoveryLedger {
    db: Database,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BootRecord {
    pub id: String,
    pub previous: Option<String>,
    pub downtime_seconds: i64,
    pub unclean: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryDecision {
    pub run_id: String,
    pub decision: String,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryStatus {
    pub boot_id: Option<String>,
    pub recovering: bool,
    pub safe_mode: bool,
    pub safe_until: i64,
    pub recent_crashes: u64,
    pub downtime_seconds: i64,
    pub quarantined: Vec<String>,
    pub decisions: Vec<RecoveryDecision>,
}

pub(crate) struct ResumeAssessment {
    pub(crate) pending_effect: bool,
    pub(crate) pending_approval: bool,
    pub(crate) frozen: Autonomy,
    pub(crate) healthy: bool,
    pub(crate) edited: bool,
    pub(crate) unanswered_request: bool,
    pub(crate) open_children: bool,
}
impl ResumeAssessment {
    /// Reasons that forbid an automatic resume; empty means eligible.
    /// `downtime_seconds` is the interruption length; 17.3 forbids automatic
    /// resume after six hours or more.
    pub(crate) fn blockers(&self, host: Autonomy, downtime_seconds: i64) -> Vec<&'static str> {
        [
            (
                downtime_seconds >= MAX_DOWNTIME,
                "interruption lasted six hours or more",
            ),
            (
                host < Autonomy::Autonomous,
                "host autonomy is below autonomous",
            ),
            (
                self.frozen < Autonomy::Autonomous,
                "run autonomy is below autonomous or unknown",
            ),
            (!self.healthy, "remaining budget is insufficient"),
            (self.pending_effect, "unresolved non-pure tool intention"),
            (self.pending_approval, "approval pending"),
            (self.edited, "recorded file edits require reconciliation"),
            (
                self.unanswered_request,
                "model request without a durable response",
            ),
            (self.open_children, "outstanding child reservation"),
        ]
        .into_iter()
        .filter_map(|(blocked, reason)| blocked.then_some(reason))
        .collect()
    }
}

impl RecoveryLedger {
    pub async fn open(db: Database) -> Result<Self> {
        let sql = include_str!("recovery.sql");
        let checksum = digest(sql.as_bytes());
        db.write(Priority::Journal, move |c| {
            c.execute_batch(sql)?;
            let known: Option<String> = c
                .query_row(
                    "SELECT checksum FROM daemon_recovery_schema WHERE version=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if known.as_ref().is_some_and(|v| v != &checksum) {
                return Err(excalibur_store::Error::Invalid(
                    "daemon recovery schema checksum mismatch",
                ));
            }
            c.execute(
                "INSERT OR IGNORE INTO daemon_recovery_schema VALUES(1,?1)",
                [checksum],
            )?;
            let next = include_str!("recovery2.sql");
            let checksum = digest(next.as_bytes());
            let known: Option<String> = c
                .query_row(
                    "SELECT checksum FROM daemon_recovery_schema WHERE version=2",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if known.as_ref().is_some_and(|v| v != &checksum) {
                return Err(excalibur_store::Error::Invalid(
                    "daemon recovery notification schema checksum mismatch",
                ));
            }
            c.execute_batch(next)?;
            c.execute(
                "INSERT OR IGNORE INTO daemon_recovery_schema VALUES(2,?1)",
                [checksum],
            )?;
            Ok(())
        })
        .await?;
        Ok(Self { db })
    }
    /// Called only by the exclusive daemon host, never by a provider retry.
    pub async fn begin(&self, at: i64) -> Result<BootRecord> {
        if at < 0 {
            return Err(invalid("nonnegative boot timestamp required"));
        }
        let id = AgentId::new().to_string();
        Ok(self.db.write(Priority::Journal, move |c| {
            // Only the host's exact confirmed CAS proves a recovery block. A
            // rejected CAS emits no row; user/model blocks use other keys.
            let registered_agents:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='orchestration_agents')",[],|r|r.get(0))?;
            if registered_agents {
                c.execute("INSERT OR IGNORE INTO daemon_recovery_decisions(boot_id,run_id,decision,reason,resume_after_safe) SELECT b.id,j.run_id,'blocked',json_extract(j.payload,'$.reason'),0 FROM daemon_boots b JOIN journal j ON j.idempotency_key='recovery:'||b.id||':'||j.run_id JOIN orchestration_agents a ON a.run_id=j.run_id WHERE j.kind='state_change' AND j.state='confirmed' AND json_extract(j.payload,'$.state')='blocked' AND json_extract(j.payload,'$.reason') IN ('possible crash cause: quarantined','recovery: unresolved effect requires reconciliation')", [])?;
            }
            let previous:Option<(String,i64,Option<i64>)>=c.query_row("SELECT b.id,b.last_seen,b.clean_exit FROM daemon_recovery_state s JOIN daemon_boots b ON b.id=s.boot_id WHERE s.id=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            let unclean=previous.as_ref().is_some_and(|v|v.2.is_none());
            if let Some((old,_,None))=&previous {
                c.execute("UPDATE daemon_boots SET crashed_at=?2 WHERE id=?1 AND crashed_at IS NULL",rusqlite::params![old,at])?;
                // The full confirmed journal, rather than a sampled heartbeat,
                // determines the exact step active at the interruption.
                let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='journal')",[],|r|r.get(0))?;
                if exists { c.execute("INSERT OR IGNORE INTO daemon_crash_positions SELECT b.boot_id,b.run_id,(SELECT count(*) FROM journal j WHERE j.run_id=b.run_id AND j.kind='step_completed' AND j.state='confirmed') FROM daemon_boot_runs b JOIN runs r ON r.id=b.run_id WHERE b.boot_id=?1 AND r.state IN('running','verifying','recovering')",[old])?; }
            }
            let crashes:u64=c.query_row("SELECT count(*) FROM daemon_boots WHERE crashed_at>=?1 AND crashed_at<=?2",rusqlite::params![at.saturating_sub(WINDOW),at],|r|r.get(0))?;
            let safe_until:i64=c.query_row("SELECT safe_until FROM daemon_recovery_state WHERE id=1",[],|r|r.get(0))?;
            let safe_until=if unclean&&(crashes>=3||safe_until>at){at.checked_add(SAFE_FOR).ok_or(excalibur_store::Error::Invalid("safe mode timestamp overflow"))?}else{safe_until};
            if unclean {c.execute("INSERT INTO daemon_quarantine(run_id,step,created_at) SELECT p.run_id,p.step,?1 FROM daemon_crash_positions p JOIN daemon_boots b ON b.id=p.boot_id WHERE b.crashed_at>=?2 AND b.crashed_at<=?1 GROUP BY p.run_id,p.step HAVING count(DISTINCT p.boot_id)>=3 ON CONFLICT(run_id) DO UPDATE SET step=excluded.step,created_at=excluded.created_at,released_at=NULL",rusqlite::params![at,at.saturating_sub(WINDOW)])?;}
            let downtime_seconds=previous.as_ref().map(|v|if at<v.1 {MAX_DOWNTIME}else{at.saturating_sub(v.1)}).unwrap_or(0);
            c.execute("INSERT INTO daemon_boots(id,started_at,last_seen,previous_boot,downtime_seconds) VALUES(?1,?2,?2,?3,?4)",rusqlite::params![id,at,previous.as_ref().map(|v|&v.0),downtime_seconds])?;
            c.execute("UPDATE daemon_recovery_state SET boot_id=?1,safe_until=?2,recovering=1 WHERE id=1",rusqlite::params![id,safe_until])?;
            Ok(BootRecord{id,previous:previous.map(|v|v.0),downtime_seconds,unclean})
        }).await?)
    }
    pub async fn status(&self, at: i64) -> Result<RecoveryStatus> {
        Ok(self.db.read(ReadLane::Interactive,move|c|{
            let (boot_id,safe_until,recovering)=c.query_row("SELECT boot_id,safe_until,recovering FROM daemon_recovery_state WHERE id=1",[],|r|Ok((r.get::<_,Option<String>>(0)?,r.get::<_,i64>(1)?,r.get::<_,bool>(2)?)))?;
            let downtime_seconds=c.query_row("SELECT downtime_seconds FROM daemon_boots WHERE id=?1",[&boot_id],|r|r.get(0)).optional()?.unwrap_or(0);
            let recent_crashes=c.query_row("SELECT count(*) FROM daemon_boots WHERE crashed_at>=?1 AND crashed_at<=?2",rusqlite::params![at.saturating_sub(WINDOW),at],|r|r.get(0))?;
            let quarantined=c.prepare("SELECT run_id FROM daemon_quarantine WHERE released_at IS NULL ORDER BY run_id")?.query_map([],|r|r.get(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let decisions=c.prepare("SELECT run_id,decision,reason FROM daemon_recovery_decisions WHERE boot_id=?1 ORDER BY run_id LIMIT 100")?.query_map([&boot_id],|r|Ok(RecoveryDecision{run_id:r.get(0)?,decision:r.get(1)?,reason:r.get(2)?}))?.collect::<std::result::Result<Vec<_>,_>>()?;
            Ok(RecoveryStatus{boot_id,recovering,safe_mode:safe_until>at,safe_until,recent_crashes,downtime_seconds,quarantined,decisions})
        }).await?)
    }
    /// Start time and measured downtime of the current daemon boot, if any.
    pub(crate) async fn current_boot(&self) -> Result<Option<(i64, i64)>> {
        Ok(self.db.read(ReadLane::Interactive, |c| {
            Ok(c.query_row("SELECT b.started_at,b.downtime_seconds FROM daemon_recovery_state s JOIN daemon_boots b ON b.id=s.boot_id WHERE s.id=1", [], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
        }).await?)
    }
    pub async fn heartbeat(&self, at: i64) -> Result<()> {
        self.db.write(Priority::Interactive,move|c|{c.execute("UPDATE daemon_boots SET last_seen=max(last_seen,?1) WHERE id=(SELECT boot_id FROM daemon_recovery_state WHERE id=1) AND clean_exit IS NULL",[at])?;Ok(())}).await?;
        Ok(())
    }
    pub async fn clean_exit(&self, boot: &str, at: i64) -> Result<()> {
        let boot = boot.to_owned();
        self.db.write(Priority::Journal,move|c|{c.execute("UPDATE daemon_boots SET clean_exit=?2,last_seen=max(last_seen,?2) WHERE id=?1 AND id=(SELECT boot_id FROM daemon_recovery_state WHERE id=1) AND crashed_at IS NULL",rusqlite::params![boot,at])?;Ok(())}).await?;
        Ok(())
    }
    pub async fn track_admission(&self, run: &str) -> Result<bool> {
        let run = run.to_owned();
        Ok(self.db.write(Priority::Journal,move|c|{
            let (boot,safe,recovering): (Option<String>,i64,bool)=c.query_row("SELECT boot_id,safe_until,recovering FROM daemon_recovery_state WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            let quarantined:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM daemon_quarantine WHERE run_id=?1 AND released_at IS NULL)",[&run],|r|r.get(0))?;
            if recovering||safe>now()||quarantined{return Ok(false)}
            if let Some(boot)=boot {c.execute("INSERT OR IGNORE INTO daemon_boot_runs VALUES(?1,?2)",rusqlite::params![boot,run])?;}
            Ok(true)
        }).await?)
    }
    async fn decision(
        &self,
        boot: &str,
        run: &str,
        decision: &str,
        reason: &str,
        resume: bool,
    ) -> Result<()> {
        let (boot, run, decision, reason) = (
            boot.to_owned(),
            run.to_owned(),
            decision.to_owned(),
            reason.to_owned(),
        );
        self.db.write(Priority::Journal,move|c|{c.execute("INSERT INTO daemon_recovery_decisions VALUES(?1,?2,?3,?4,?5) ON CONFLICT(boot_id,run_id) DO UPDATE SET decision=excluded.decision,reason=excluded.reason,resume_after_safe=excluded.resume_after_safe",rusqlite::params![boot,run,decision,reason,resume])?;Ok(())}).await?;
        Ok(())
    }
    async fn finish(&self, boot: &str) -> Result<()> {
        let boot = boot.to_owned();
        self.db.write(Priority::Journal,move|c|{let summary:String=c.query_row("SELECT json_object('decisions',count(*),'resumable',sum(resume_after_safe)) FROM daemon_recovery_decisions WHERE boot_id=?1",[&boot],|r|r.get(0))?;c.execute("UPDATE daemon_boots SET summary=?2 WHERE id=?1",rusqlite::params![boot,summary])?;c.execute("UPDATE daemon_recovery_state SET recovering=0 WHERE id=1 AND boot_id=?1",[boot])?;Ok(())}).await?;
        Ok(())
    }
    /// Atomically enqueue scoped notices through the normal delivery pipeline.
    /// Each blocked decision is urgent; the separate conversation summary is
    /// unique per boot. Existing v1 summary events remain immutable history.
    pub async fn publish_summary(&self, boot: &str, at: i64) -> Result<()> {
        let boot = boot.to_owned();
        self.db.write(Priority::Journal,move|c|{
            let safe_until:i64=c.query_row("SELECT safe_until FROM daemon_recovery_state WHERE id=1",[],|r|r.get(0))?;
            let crashes:u64=c.query_row("SELECT count(*) FROM daemon_boots WHERE crashed_at>=?1 AND crashed_at<=?2",rusqlite::params![at.saturating_sub(WINDOW),at],|r|r.get(0))?;
            let master: String=c.query_row("SELECT id FROM orchestration_agents WHERE depth=0 ORDER BY id LIMIT 1",[],|r|r.get(0))?;
            let conversations=c.prepare("SELECT id,COALESCE(active_scope,scope_id,'general') FROM conversations ORDER BY id")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let mut query=c.prepare("SELECT d.run_id,d.decision,d.reason,a.id,json_extract(a.body,'$.origin_conversation'),json_extract(a.body,'$.scope') FROM daemon_recovery_decisions d JOIN orchestration_agents a ON a.run_id=d.run_id WHERE d.boot_id=?1 ORDER BY d.run_id")?;
            let decisions=query.query_map([&boot],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let enqueue=|notice_boot:&str,subject:&str,agent:&str,conversation:&str,class:&str,content:Value|->excalibur_store::Result<()> {
                let id=format!("ntf_{}",digest(format!("startup:{notice_boot}:{subject}").as_bytes()));
                let inserted=c.execute("INSERT OR IGNORE INTO daemon_recovery_notification_receipts VALUES(?1,?2,?3)",rusqlite::params![notice_boot,subject,id])?;
                if inserted>0 {c.execute("INSERT INTO orchestration_notifications(id,agent_id,conversation,class,content,created,acknowledged) VALUES(?1,?2,?3,?4,?5,?6,0)",rusqlite::params![id,agent,conversation,class,content.to_string(),at])?;}
                Ok(())
            };
            // A process may stop after persisting a decision but before publishing.
            // Drain every unreceipted blocked decision, retaining its original boot.
            let mut pending=c.prepare("SELECT d.boot_id,d.run_id,d.decision,d.reason,a.id,json_extract(a.body,'$.origin_conversation'),json_extract(a.body,'$.scope') FROM daemon_recovery_decisions d JOIN orchestration_agents a ON a.run_id=d.run_id WHERE d.decision='blocked' AND NOT EXISTS(SELECT 1 FROM daemon_recovery_notification_receipts r WHERE r.boot_id=d.boot_id AND r.subject='decision:'||d.run_id||':'||d.decision) ORDER BY d.boot_id,d.run_id")?;
            let pending=pending.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            for (notice_boot,run,decision,reason,agent,origin,scope) in pending {
                if conversations.iter().any(|(id,_)|id==&origin) {
                    enqueue(&notice_boot,&format!("decision:{run}:{decision}"),&agent,&origin,"urgent",json!({"kind":"recovery_blocked","boot_id":notice_boot,"run_id":run,"decision":decision,"reason":reason,"origin_conversation":origin,"scope":scope,"text":format!("{run}: {decision}: {reason}")}))?;
                }
            }
            for (conversation,scope) in conversations {
                let scoped=decisions.iter().filter(|(_,_,_,_,origin,own_scope)|origin==&conversation&&own_scope==&scope).collect::<Vec<_>>();
                let mut details=Vec::new();
                for (run,decision,reason,_,_,_) in &scoped {
                    let item=json!({"run_id":run,"decision":decision,"reason":reason.chars().take(256).collect::<String>()});
                    if details.len()>=100 || serde_json::to_vec(&details).map_err(|_|excalibur_store::Error::Invalid("recovery summary serialization failed"))?.len()+item.to_string().len()>32*1024 {break}
                    details.push(item);
                }
                let counts=scoped.iter().fold(std::collections::BTreeMap::<String,usize>::new(),|mut counts,row|{*counts.entry(row.1.clone()).or_default()+=1;counts});
                let text=format!("Startup recovery: {crashes} recent unclean restarts; safe mode {}. Decisions in {scope}: {}. Details are attached; full recovery state is available.",if safe_until>at{"active"}else{"inactive"},serde_json::to_string(&counts).unwrap_or_default());
                let agent=scoped.first().map(|row|row.3.as_str()).unwrap_or(&master);
                enqueue(&boot,&format!("conversation:{conversation}"),agent,&conversation,if safe_until>at{"urgent"}else{"normal"},json!({"kind":"recovery_summary","boot_id":boot,"scope":scope,"origin_conversation":conversation,"safe_mode":safe_until>at,"safe_until":safe_until,"recent_crashes":crashes,"counts":counts,"decisions":details,"omitted_decisions":scoped.len().saturating_sub(details.len()),"text":text}))?;
            }
            Ok(())
        }).await?;
        Ok(())
    }
    /// Host scope comes only from a receipt-backed recovery notification, never
    /// from an ordinary agent's user-supplied JSON content.
    pub async fn notification_scope(&self, id: &str) -> Result<Option<String>> {
        let id = id.to_owned();
        Ok(self.db.read(ReadLane::Interactive, move |c| {
            Ok(c.query_row("SELECT json_extract(n.content,'$.scope') FROM daemon_recovery_notification_receipts r JOIN orchestration_notifications n ON n.id=r.notification_id WHERE n.id=?1", [id], |r| r.get(0)).optional()?)
        }).await?)
    }
    pub async fn exit_safe_mode(&self, operation: &str, at: i64) -> Result<()> {
        self.control(operation, "exit_safe_mode", "", at).await
    }
    pub async fn release_quarantine(&self, operation: &str, run: &str, at: i64) -> Result<()> {
        self.control(operation, "release_quarantine", run, at).await
    }
    async fn control(&self, operation: &str, kind: &str, target: &str, at: i64) -> Result<()> {
        if operation.is_empty() {
            return Err(invalid("recovery control operation required"));
        }
        let (op, kind, target) = (operation.to_owned(), kind.to_owned(), target.to_owned());
        self.db
            .write(Priority::Interactive, move |c| {
                let old: Option<(String, String)> = c
                    .query_row(
                        "SELECT kind,target FROM daemon_recovery_controls WHERE operation=?1",
                        [&op],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some(old) = old {
                    if old != (kind, target) {
                        return Err(excalibur_store::Error::Invalid(
                            "recovery control operation reused",
                        ));
                    }
                    return Ok(());
                }
                if kind == "release_quarantine"
                    && !c.query_row(
                        "SELECT EXISTS(SELECT 1 FROM daemon_quarantine WHERE run_id=?1)",
                        [&target],
                        |r| r.get::<_, bool>(0),
                    )?
                {
                    return Err(excalibur_store::Error::Invalid("unknown quarantined run"));
                }
                if kind == "exit_safe_mode" {
                    c.execute(
                        "UPDATE daemon_recovery_state SET safe_until=0 WHERE id=1",
                        [],
                    )?;
                } else {
                    c.execute(
                        "UPDATE daemon_quarantine SET released_at=?2 WHERE run_id=?1",
                        rusqlite::params![target, at],
                    )?;
                }
                c.execute(
                    "INSERT INTO daemon_recovery_controls VALUES(?1,?2,?3,?4)",
                    rusqlite::params![op, kind, target, at],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}

impl Orchestrator {
    pub fn recovery(&self) -> &RecoveryLedger {
        &self.inner.recovery
    }
    /// Startup runs before admission. User/approval waits and uncertain effects
    /// retain authority; this method never invokes an external tool or model.
    pub async fn recover_startup(
        &self,
        boot: &BootRecord,
        autonomy: Autonomy,
    ) -> Result<RecoveryStatus> {
        self.recover_startup_inner(boot, autonomy, |_| Ok(())).await
    }
    async fn recover_startup_inner(
        &self,
        boot: &BootRecord,
        autonomy: Autonomy,
        after_state: impl Fn(&str) -> Result<()>,
    ) -> Result<RecoveryStatus> {
        *self.inner.host_autonomy.lock().unwrap() = Some(autonomy);
        let status = self.recovery().status(now()).await?;
        let mut agents = self.active_agents().await?;
        agents.sort_by_key(|a| {
            (
                a.delegation.as_ref().map(|d| d.priority).unwrap_or(0),
                a.depth,
                a.id.clone(),
            )
        });
        for agent in agents {
            let Some(run) = &agent.run_id else { continue };
            let state = self.inner.engine.load(run).await?;
            if terminal(&state.status) {
                continue;
            }
            let quarantined = status.quarantined.contains(run);
            let owned_pause = state.status == Status::Paused
                && matches!(state.reason.as_deref(), Some("safe_mode" | "shutdown"));
            let preserved = matches!(
                state.status,
                Status::Paused | Status::Waiting | Status::Blocked
            ) && !owned_pause;
            let assessment = self.assess_resume(run, &state).await?;
            let pending_effect = assessment.pending_effect;
            let pending_approval = assessment.pending_approval;
            let eligible = assessment
                .blockers(autonomy, boot.downtime_seconds)
                .is_empty();
            let (decision, reason, resume) = if quarantined && !preserved {
                ("blocked", "possible crash cause: quarantined", false)
            } else if preserved {
                (
                    "preserved",
                    state.reason.as_deref().unwrap_or("existing wait/control"),
                    false,
                )
            } else if pending_approval {
                ("paused", "approval required", false)
            } else if pending_effect {
                (
                    "blocked",
                    "recovery: unresolved effect requires reconciliation",
                    false,
                )
            } else if status.safe_mode {
                ("paused", "safe_mode", eligible)
            } else if boot.previous.is_some() && !eligible {
                ("paused", "recovery", false)
            } else {
                (
                    "ready",
                    "startup admission eligible; engine reconciliation required",
                    false,
                )
            };
            if decision != "preserved" && decision != "ready" {
                let next = if decision == "blocked" {
                    "blocked"
                } else {
                    "paused"
                };
                let mut revision = state.last_sequence;
                let mut prior = label(&state.status).to_owned();
                if next == "blocked" && matches!(state.status, Status::Ready | Status::Paused) {
                    let staged = self
                        .inner
                        .engine
                        .journal()
                        .set_state_if_revision(
                            run,
                            &[&prior],
                            revision,
                            "running",
                            Some("startup recovery staging".into()),
                            &format!("recovery-stage:{}:{run}", boot.id),
                        )
                        .await?;
                    if !staged {
                        continue;
                    }
                    let staged = self.inner.engine.load(run).await?;
                    if staged.status != Status::Running
                        || staged.reason.as_deref() != Some("startup recovery staging")
                    {
                        continue;
                    }
                    revision = staged.last_sequence;
                    prior = "running".into();
                }
                let applied = self
                    .inner
                    .engine
                    .journal()
                    .set_state_if_revision(
                        run,
                        &[&prior],
                        revision,
                        next,
                        Some(reason.into()),
                        &format!("recovery:{}:{run}", boot.id),
                    )
                    .await?;
                if !applied {
                    continue;
                }
                after_state(run)?;
            }
            if decision == "ready" && owned_pause {
                self.inner
                    .engine
                    .resume_if_reason(run, state.reason.as_deref().unwrap())
                    .await?;
            }
            self.recovery()
                .decision(&boot.id, run, decision, reason, resume)
                .await?;
            self.inner.engine.journal().append(run,"recovery_note",&format!("recovery-note:{}",boot.id),json!({"boot":boot.id,"decision":decision,"reason":reason,"terminal_sessions":"lost after host restart","downtime_seconds":boot.downtime_seconds})).await?;
        }
        self.recovery().finish(&boot.id).await?;
        self.recovery().status(now()).await
    }
    /// The 17.3 automatic-resume facts shared by startup recovery and the stall
    /// watchdog. Any unresolved non-pure intention, approval wait, recorded edit,
    /// unanswered model request, outstanding child reservation, insufficient
    /// budget or non-autonomous host/frozen autonomy prevents automatic resume.
    pub(crate) async fn assess_resume(
        &self,
        run: &str,
        state: &excalibur_engine::Snapshot,
    ) -> Result<ResumeAssessment> {
        Ok(ResumeAssessment {
            pending_effect: state.calls.values().any(|c| {
                c.result.is_none()
                    && c.prepared
                        .as_ref()
                        .is_some_and(|p| p.effect != Effect::Pure)
            }),
            pending_approval: self.pending_startup_approval(run).await?,
            frozen: self.frozen_startup_autonomy(run).await?,
            healthy: excalibur_engine::remaining_budget_for(self.inner.engine.journal(), run)
                .await
                .map(|b| b.max_steps > 1 && b.max_tokens > 0 && b.max_wall_seconds > 30)
                .unwrap_or(false),
            edited: self.has_recorded_startup_edits(run).await?,
            unanswered_request: state.request.is_some() && state.response.is_none(),
            open_children: state
                .child_allocations
                .values()
                .any(|a| a.settled.is_none()),
        })
    }
    async fn has_recorded_startup_edits(&self, run: &str) -> Result<bool> {
        let run = run.to_owned();
        Ok(self
            .inner
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM file_versions WHERE run_id=?1)",
                    [run],
                    |r| r.get(0),
                )?)
            })
            .await?)
    }
    async fn pending_startup_approval(&self, run: &str) -> Result<bool> {
        let run = run.to_owned();
        Ok(self.inner.db.read(ReadLane::Interactive,move|c|{
            let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='security_approvals')",[],|r|r.get(0))?;
            if !exists{return Ok(false)}
            Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM security_approvals WHERE run_id=?1 AND status='pending')",[run],|r|r.get(0))?)
        }).await?)
    }
    async fn frozen_startup_autonomy(&self, run: &str) -> Result<Autonomy> {
        let run = run.to_owned();
        let value: Option<String> = self
            .inner
            .db
            .read(ReadLane::Interactive, move |c| {
                let exists: bool = c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='security_runs')",
                    [],
                    |r| r.get(0),
                )?;
                if !exists {
                    return Ok(None);
                }
                Ok(c.query_row(
                    "SELECT definition FROM security_runs WHERE run_id=?1",
                    [run],
                    |r| r.get(0),
                )
                .optional()?)
            })
            .await?;
        Ok(value
            .and_then(|s| serde_json::from_str::<excalibur_security::RunSecurity>(&s).ok())
            .and_then(|v| v.autonomy.into_iter().min())
            .unwrap_or(Autonomy::Supervised))
    }
    pub async fn resume_safe_mode_owned(&self) -> Result<()> {
        if self.recovery().status(now()).await?.safe_mode {
            return Ok(());
        }
        let elapsed_ok=self.inner.db.read(ReadLane::Interactive,|c|{Ok(c.query_row("SELECT started_at,downtime_seconds FROM daemon_boots WHERE id=(SELECT boot_id FROM daemon_recovery_state WHERE id=1)",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?))).optional()?.is_none_or(|(start,downtime)|now()>=start && downtime.saturating_add(now().saturating_sub(start))<MAX_DOWNTIME))}).await?;
        let rows=self.inner.db.read(ReadLane::Interactive,|c|{let mut q=c.prepare("SELECT DISTINCT run_id,resume_after_safe FROM daemon_recovery_decisions WHERE reason='safe_mode' AND boot_id=(SELECT boot_id FROM daemon_recovery_state WHERE id=1)")?;Ok(q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,bool>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        for (run, resume) in rows {
            // Do not acquire a running driver's boundary on every maintenance
            // tick after an owned pause has already been released.
            let state = self.inner.engine.load(&run).await?;
            if state.status != Status::Paused || state.reason.as_deref() != Some("safe_mode") {
                continue;
            }
            let healthy = excalibur_engine::remaining_budget_for(self.inner.engine.journal(), &run)
                .await
                .map(|b| b.max_steps > 1 && b.max_tokens > 0 && b.max_wall_seconds > 30)
                .unwrap_or(false);
            if resume
                && elapsed_ok
                && healthy
                && !self.pending_startup_approval(&run).await?
                && !self.has_recorded_startup_edits(&run).await?
            {
                self.inner
                    .engine
                    .resume_if_reason(&run, "safe_mode")
                    .await?;
            } else {
                self.inner
                    .engine
                    .journal()
                    .set_state_if_revision(
                        &run,
                        &["paused"],
                        state.last_sequence,
                        "paused",
                        Some("recovery".into()),
                        &format!("safe-mode-ended:{}", state.last_sequence),
                    )
                    .await?;
            }
        }
        Ok(())
    }
    pub async fn release_recovery_quarantine(&self, operation: &str, run: &str) -> Result<()> {
        self.recovery()
            .release_quarantine(operation, run, now())
            .await?;
        if self
            .recovery()
            .status(now())
            .await?
            .quarantined
            .iter()
            .any(|v| v == run)
        {
            return Ok(());
        }
        let state = self.inner.engine.load(run).await?;
        if state.status == Status::Blocked
            && state.reason.as_deref() == Some("possible crash cause: quarantined")
        {
            self.inner
                .engine
                .journal()
                .set_state_if_revision(
                    run,
                    &["blocked"],
                    state.last_sequence,
                    "paused",
                    Some("recovery".into()),
                    &format!("quarantine-released:{operation}"),
                )
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod atomic_notice_tests {
    use super::*;
    use crate::recovery_test_support::*;
    use excalibur_types::runtime::{PreparedTool, ToolCall};

    #[tokio::test]
    async fn confirmed_recovery_cas_survives_loss_before_decision_without_repeating_effect() {
        let f = fixture().await;
        let at = now();
        let first = f.host.recovery().begin(at).await.unwrap();
        f.host
            .recover_startup(&first, Autonomy::Autonomous)
            .await
            .unwrap();
        f.db.write(Priority::Journal, |c| {
            c.execute("INSERT INTO conversations(id,active_scope) VALUES('origin','general'),('other','general')", [])?;
            Ok(())
        }).await.unwrap();
        let mut task = delegation("uncertain-cas", "general", 300);
        task.profile = "coordinator".into();
        task.origin_conversation = "origin".into();
        let agent = f.host.spawn(&f.master.id, task).await.unwrap();
        let run = agent.run_id.unwrap();
        f.host
            .engine()
            .journal()
            .set_state(&run, "running", None, "fixture:running")
            .await
            .unwrap();
        let prepared = PreparedTool {
            call: ToolCall {
                id: "external".into(),
                name: "irreversible".into(),
                arguments: "{}".into(),
            },
            effect: Effect::NonIdempotent,
            resources: vec![],
            exclusive: true,
            data: json!({}),
        };
        f.host
            .engine()
            .journal()
            .append(
                &run,
                "tool_intent",
                "fixture:uncertain",
                json!({"index":0,"prepared":prepared}),
            )
            .await
            .unwrap();
        let interrupted = f.host.recovery().begin(at + 1).await.unwrap();
        let result = f
            .host
            .recover_startup_inner(&interrupted, Autonomy::Autonomous, |actual| {
                assert_eq!(actual, run);
                Err(invalid(
                    "injected process loss after confirmed recovery CAS",
                ))
            })
            .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("injected process loss")
        );
        assert_eq!(
            f.host.engine().load(&run).await.unwrap().status,
            Status::Blocked
        );
        assert!(
            f.host
                .recovery()
                .status(at + 1)
                .await
                .unwrap()
                .decisions
                .is_empty()
        );
        assert!(f.host.notifications("origin").await.unwrap().is_empty());
        f.host.shutdown().await.unwrap();
        drop(f.host);
        f.db.shutdown().await.unwrap();
        drop(f.db);
        let f = open_fixture(f.dir).await;
        let next = f.host.recovery().begin(at + 2).await.unwrap();
        f.host
            .recover_startup(&next, Autonomy::Autonomous)
            .await
            .unwrap();
        for _ in 0..2 {
            f.host
                .recovery()
                .publish_summary(&next.id, at + 2)
                .await
                .unwrap();
        }
        let notices = f.host.notifications("origin").await.unwrap();
        let blocked = notices
            .iter()
            .filter(|notice| {
                serde_json::from_str::<Value>(&notice.content).unwrap()["kind"]
                    == "recovery_blocked"
            })
            .collect::<Vec<_>>();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].class, "urgent");
        let payload: Value = serde_json::from_str(&blocked[0].content).unwrap();
        assert_eq!(payload["boot_id"], interrupted.id);
        assert_eq!(payload["run_id"], run);
        assert_eq!(payload["origin_conversation"], "origin");
        assert_eq!(payload["scope"], "general");
        assert_eq!(
            payload["reason"],
            "recovery: unresolved effect requires reconciliation"
        );
        assert!(
            f.host
                .notifications("other")
                .await
                .unwrap()
                .iter()
                .all(|n| !n.content.contains(&run))
        );
        assert!(f.host.tick().await.unwrap().is_empty());
        let owner = run.clone();
        let (intents, results): (u64, u64) =
            f.db.read(ReadLane::Interactive, move |c| {
                Ok((
                    c.query_row(
                        "SELECT count(*) FROM journal WHERE run_id=?1 AND kind='tool_intent'",
                        [&owner],
                        |r| r.get(0),
                    )?,
                    c.query_row(
                        "SELECT count(*) FROM journal WHERE run_id=?1 AND kind='tool_result'",
                        [&owner],
                        |r| r.get(0),
                    )?,
                ))
            })
            .await
            .unwrap();
        assert_eq!((intents, results), (1, 0));
        f.host.shutdown().await.unwrap();
        f.db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn rejected_cas_and_user_block_are_not_reconstructed_as_host_decisions() {
        let f = fixture().await;
        let at = now();
        let first = f.host.recovery().begin(at).await.unwrap();
        f.host
            .recover_startup(&first, Autonomy::Autonomous)
            .await
            .unwrap();
        f.db.write(Priority::Journal, |c| {
            c.execute(
                "INSERT INTO conversations(id,active_scope) VALUES('origin','general')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let mut task = delegation("user-block", "general", 300);
        task.origin_conversation = "origin".into();
        task.profile = "coordinator".into();
        let agent = f.host.spawn(&f.master.id, task).await.unwrap();
        let run = agent.run_id.unwrap();
        let journal = f.host.engine().journal();
        journal
            .set_state(&run, "running", None, "fixture:running")
            .await
            .unwrap();
        journal
            .set_state(
                &run,
                "blocked",
                Some("recovery: unresolved effect requires reconciliation".into()),
                "user:block",
            )
            .await
            .unwrap();
        let snapshot = f.host.engine().load(&run).await.unwrap();
        let key = format!("recovery:{}:{run}", first.id);
        assert!(
            !journal
                .set_state_if_revision(
                    &run,
                    &["running"],
                    snapshot.last_sequence,
                    "blocked",
                    Some("recovery: unresolved effect requires reconciliation".into()),
                    &key
                )
                .await
                .unwrap()
        );
        // Even an exact-looking operation key is insufficient without the
        // confirmed state_change kind and blocked payload from the real CAS.
        let forged_response = excalibur_types::runtime::ModelResponse {
            message: excalibur_types::runtime::Message::text(
                "assistant",
                "Synthetic forged recovery claim",
            ),
            usage: excalibur_types::runtime::Usage::default(),
        };
        journal.append(&run, "model_response", &key, json!({"response":forged_response,"state":"blocked","reason":"recovery: unresolved effect requires reconciliation"})).await.unwrap();
        let next = f.host.recovery().begin(at + 1).await.unwrap();
        f.host
            .recover_startup(&next, Autonomy::Autonomous)
            .await
            .unwrap();
        f.host
            .recovery()
            .publish_summary(&next.id, at + 1)
            .await
            .unwrap();
        assert!(
            f.host
                .notifications("origin")
                .await
                .unwrap()
                .iter()
                .all(|n| {
                    serde_json::from_str::<Value>(&n.content).unwrap()["kind"] != "recovery_blocked"
                })
        );
        assert_eq!(
            f.host.engine().load(&run).await.unwrap().status,
            Status::Blocked
        );
        let id = first.id;
        let missing: u64 = f.db.read(ReadLane::Interactive, move |c| Ok(c.query_row("SELECT count(*) FROM daemon_recovery_decisions WHERE boot_id=?1 AND decision='blocked'", [id], |r| r.get(0))?)).await.unwrap();
        assert_eq!(missing, 0);
        f.host.shutdown().await.unwrap();
        f.db.shutdown().await.unwrap();
    }
}

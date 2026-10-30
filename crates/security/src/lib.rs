//! Deterministic authorization, durable approvals and append-only audit for host adapters.
mod executor;
mod file_control;
mod host_effect;
pub use file_control::{FileScanner, content_type};
mod finalization;
mod network;
mod policy;
mod secured;
use excalibur_store::{Database, Priority, ReadLane};
pub use excalibur_vault::Redactor;
pub use executor::SecretExecutor;
pub use policy::*;
pub use secured::SecuredTools;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("security storage: {0}")]
    Store(#[from] excalibur_store::Error),
    #[error("security serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("security rejected operation: {0}")]
    Denied(String),
    #[error("security approval required: {0}")]
    ApprovalPending(String),
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunSecurity {
    pub parent: Option<String>,
    pub project: String,
    pub workspace: PathBuf,
    pub token: CapabilityToken,
    pub autonomy: Vec<Autonomy>,
    pub full_acknowledged: bool,
    /// Describes the original task only; it never attests later concrete actions.
    pub user_requested: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ApprovalScope {
    Once,
    Task,
    Project,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask { approval_id: String, reason: String },
    Deny { reason: String },
}
#[derive(Clone)]
pub struct Security {
    db: Database,
    redactor: Redactor,
    ceiling: Arc<CapabilityToken>,
    protected: Arc<Vec<PathBuf>>,
    evaluations: Arc<Vec<PathBuf>>,
    noninteractive: bool,
    verification_checks: Option<Arc<std::collections::BTreeSet<String>>>,
}
const SQL: &str = include_str!("schema.sql");
const DISPATCH_SQL: &str = include_str!("dispatch.sql");
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn action_key(action: &Action) -> Result<String> {
    Ok(digest(&serde_json::to_vec(action)?))
}
impl Security {
    pub async fn new(db: Database, ceiling: CapabilityToken, redactor: Redactor) -> Result<Self> {
        ceiling.validate().map_err(Error::Denied)?;
        let checksum = digest(SQL.as_bytes());
        db.write(Priority::Journal, move |c| {
            use excalibur_store::rusqlite::OptionalExtension;
            c.execute_batch(SQL)?;
            let old = c
                .query_row(
                    "SELECT checksum FROM security_schema WHERE version=1",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            if old.is_some_and(|s| s != checksum) {
                return Err(excalibur_store::Error::Invalid(
                    "security schema checksum mismatch",
                ));
            }
            c.execute(
                "INSERT OR IGNORE INTO security_schema VALUES(1,?1)",
                [checksum],
            )?;
            let dispatch_checksum = digest(DISPATCH_SQL.as_bytes());
            let existing = c
                .query_row(
                    "SELECT checksum FROM security_schema WHERE version=2",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            if existing.is_some_and(|old| old != dispatch_checksum) {
                return Err(excalibur_store::Error::Invalid(
                    "security dispatch schema checksum mismatch",
                ));
            }
            c.execute_batch(DISPATCH_SQL)?;
            c.execute(
                "INSERT OR IGNORE INTO security_schema VALUES(2,?1)",
                [dispatch_checksum],
            )?;
            Ok(())
        })
        .await?;
        Ok(Self {
            db,
            redactor,
            ceiling: Arc::new(ceiling),
            protected: Arc::new(Vec::new()),
            evaluations: Arc::new(Vec::new()),
            noninteractive: false,
            verification_checks: None,
        })
    }
    /// Host-owned encrypted vault and evaluation paths are never delegated to model policy.
    pub fn with_protected_paths(
        mut self,
        vault_paths: Vec<PathBuf>,
        evaluation_paths: Vec<PathBuf>,
    ) -> Result<Self> {
        if vault_paths
            .iter()
            .chain(&evaluation_paths)
            .any(|p| !p.is_absolute())
        {
            return Err(Error::Denied("protected paths must be absolute".into()));
        }
        self.protected = Arc::new(
            vault_paths
                .iter()
                .map(|p| canonical_target(p).map_err(Error::Denied))
                .collect::<Result<Vec<_>>>()?,
        );
        self.evaluations = Arc::new(
            evaluation_paths
                .iter()
                .map(|p| canonical_target(p).map_err(Error::Denied))
                .collect::<Result<Vec<_>>>()?,
        );
        Ok(self)
    }
    pub fn redactor(&self) -> Redactor {
        self.redactor.clone()
    }
    pub fn database(&self) -> &Database {
        &self.db
    }
    pub async fn register_run(&self, run: &str, config: RunSecurity, now: i64) -> Result<()> {
        config.token.validate().map_err(Error::Denied)?;
        if !config.workspace.is_absolute()
            || config
                .workspace
                .canonicalize()
                .map_err(|e| Error::Denied(e.to_string()))?
                != config.workspace
        {
            return Err(Error::Denied(
                "workspace must be canonical and host-selected".into(),
            ));
        }
        let autonomy = config.autonomy.iter().copied().min().unwrap_or_default();
        if autonomy == Autonomy::Full && (!config.full_acknowledged || config.project.is_empty()) {
            return Err(Error::Denied(
                "full autonomy requires explicit project acknowledgement".into(),
            ));
        }
        let limit = match &config.parent {
            Some(parent) => self.effective_token(parent, now).await?,
            None => (*self.ceiling).clone(),
        };
        if !limit.contains(&config.token) {
            return Err(Error::Denied(
                "child capability exceeds parent or host ceiling".into(),
            ));
        }
        let (run, payload, redactor) = (
            run.to_owned(),
            serde_json::to_string(&config)?,
            self.redactor.clone(),
        );
        if redactor.contains_secret(payload.as_bytes()) {
            return Err(Error::Denied(
                "capability metadata contains a secret literal".into(),
            ));
        }
        self.db.write(Priority::Journal,move|c|{
            use excalibur_store::rusqlite::OptionalExtension;
            let old=c.query_row("SELECT definition FROM security_runs WHERE run_id=?1",[&run],|r|r.get::<_,String>(0)).optional()?;
            if old.as_ref().is_some_and(|s|s!=&payload){return Err(excalibur_store::Error::Invalid("execution capability base is immutable"));}
            c.execute("INSERT OR IGNORE INTO security_runs(run_id,definition) VALUES(?1,?2)",[&run,&payload])?;
            c.execute("INSERT OR IGNORE INTO security_state(run_id,tainted_secret,untrusted_seen) VALUES(?1,0,0)",[&run])?;
            append_audit(c,&redactor,json!({"kind":"capability_base","run":run,"definition":serde_json::from_str::<Value>(&payload).unwrap(),"at":now}))?;Ok(())
        }).await?;
        Ok(())
    }
    pub async fn run_security(&self, run: &str) -> Result<RunSecurity> {
        let run = run.to_owned();
        let text = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT definition FROM security_runs WHERE run_id=?1",
                    [run],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .await?;
        Ok(serde_json::from_str(&text)?)
    }
    pub async fn effective_token(&self, run: &str, now: i64) -> Result<CapabilityToken> {
        let config = self.run_security(run).await?;
        let (id, mut token) = (run.to_owned(), config.token);
        let definitions=self.db.read(ReadLane::Interactive,move|c|{let mut stmt=c.prepare("SELECT grant_definition FROM capability_grants WHERE run_id=?1 AND (expires_at IS NULL OR expires_at>?2) ORDER BY rowid")?;Ok(stmt.query_map(excalibur_store::rusqlite::params![id,now],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        for grant in definitions {
            token.merge(&serde_json::from_str::<CapabilityToken>(&grant)?);
        }
        if !self.ceiling.contains(&token) {
            return Err(Error::Denied(
                "execution exceeds current host capability ceiling".into(),
            ));
        }
        if let Some(parent) = config.parent {
            let ceiling = Box::pin(self.effective_token(&parent, now)).await?;
            if !ceiling.contains(&token) {
                return Err(Error::Denied(
                    "execution exceeds current parent capabilities after expiry".into(),
                ));
            }
        }
        Ok(token)
    }
    pub async fn add_rule(&self, rule: Rule, user_confirmed_diff: bool, now: i64) -> Result<()> {
        if !user_confirmed_diff {
            return Err(Error::Denied(
                "security settings require explicit user-approved diff".into(),
            ));
        }
        globset::Glob::new(&rule.pattern).map_err(|e| Error::Denied(e.to_string()))?;
        let (text, redactor) = (serde_json::to_string(&rule)?, self.redactor.clone());
        self.db
            .write(Priority::Journal, move |c| {
                c.execute(
                    "INSERT INTO security_rules(id,definition) VALUES(?1,?2)",
                    [&rule.id, &text],
                )?;
                append_audit(
                    c,
                    &redactor,
                    json!({"kind":"rule_added","rule":rule,"at":now}),
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    async fn rules(&self) -> Result<Vec<Rule>> {
        let values = self
            .db
            .read(ReadLane::Interactive, |c| {
                let mut stmt = c.prepare("SELECT definition FROM security_rules ORDER BY id")?;
                Ok(stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        values
            .into_iter()
            .map(|s| Ok(serde_json::from_str(&s)?))
            .collect()
    }
    fn absolute_deny(&self, action: &Action) -> Option<String> {
        match action {
            Action::DisableSandbox|Action::ReadVault|Action::SecuritySettings{..}=>Some("absolute denial; security configuration requires a separate authenticated user settings operation".into()),
            Action::Read{path}|Action::Write{path}|Action::WriteExact{path,..} if self.protected.iter().any(|p|path.starts_with(p))=>Some("encrypted vault storage is inaccessible to agents".into()),
            Action::Write{path}|Action::WriteExact{path,..} if sensitive(path)||self.evaluations.iter().any(|p|path.starts_with(p))=>Some("sensitive configuration and evaluation paths are read-only".into()),
            _=>None,
        }
    }
    /// Frozen host policy for unattended execution: requests needing approval
    /// become audited denials, without creating a pending approval or waiting.
    pub fn with_noninteractive_policy(mut self) -> Self {
        self.noninteractive = true;
        self
    }
    /// Host-selected exact check commands, confined by a read-only, offline
    /// capability. This does not create or impersonate a user approval.
    pub fn with_verification_checks(
        mut self,
        commands: std::collections::BTreeSet<String>,
    ) -> Result<Self> {
        if self.ceiling.roots.iter().any(|root| root.write)
            || self.ceiling.network != Network::None
            || !self.ceiling.secrets.is_empty()
            || !self.ceiling.external_actions.is_empty()
            || !matches!(
                self.ceiling.sandbox_profile.as_deref(),
                Some("read-only" | "eval")
            )
        {
            return Err(Error::Denied("verification checks require a read-only offline capability without secrets or external actions".into()));
        }
        self.verification_checks = Some(Arc::new(commands));
        Ok(self)
    }
    pub async fn authorize(&self, run: &str, action: &Action, now: i64) -> Result<Decision> {
        self.authorize_with_ticket(run, action, now, None).await
    }
    async fn authorize_with_ticket(
        &self,
        run: &str,
        action: &Action,
        now: i64,
        ticket: Option<&str>,
    ) -> Result<Decision> {
        let config = self.run_security(run).await?;
        let token = self.effective_token(run, now).await?;
        let mut denial = self.absolute_deny(action);
        if !token.permits(action) {
            denial.get_or_insert("action exceeds execution capabilities".into());
        }
        let mut rule_effect = RuleEffect::Allow;
        for rule in self.rules().await? {
            if rule.matches(run, &config.project, action, now) {
                rule_effect = rule_effect.max(rule.effect);
            }
        }
        if rule_effect == RuleEffect::Deny {
            denial = Some("deny rule matched".into());
        }
        if let Some(reason) = denial {
            self.audit(json!({"kind":"authorization_denied","run":run,"action":action,"reason":reason,"at":now})).await?;
            return Ok(Decision::Deny { reason });
        }
        if let (Action::Exec { command }, Some(checks)) = (action, &self.verification_checks) {
            let report = classify(action);
            let allowed = checks.contains(command)
                && token.roots.iter().all(|root| !root.write)
                && token.network == Network::None
                && token.secrets.is_empty()
                && token.external_actions.is_empty()
                && matches!(token.sandbox_profile.as_deref(), Some("read-only" | "eval"))
                && !report.sensitive
                && !report.network_command
                && report.risk != Risk::High;
            self.audit(json!({"kind":"verification_check_authorized","run":run,"action":action,"allowed":allowed,"at":now})).await?;
            return Ok(if allowed {
                Decision::Allow
            } else {
                Decision::Deny {
                    reason: "command is outside the declared offline read-only verification policy"
                        .into(),
                }
            });
        }
        if self
            .approved(run, &config.project, action, now, ticket)
            .await?
            .is_some()
        {
            return Ok(Decision::Allow);
        }
        let run_copy = run.to_owned();
        let (tainted, untrusted) = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT tainted_secret,untrusted_seen FROM security_state WHERE run_id=?1",
                    [run_copy],
                    |r| Ok((r.get::<_, bool>(0)?, r.get::<_, bool>(1)?)),
                )?)
            })
            .await?;
        let report = classify(action);
        let autonomy = config.autonomy.iter().copied().min().unwrap_or_default();
        let outside = match action {
            Action::Read { path } | Action::Write { path } | Action::WriteExact { path, .. } => {
                !path.starts_with(&config.workspace)
            }
            _ => false,
        };
        let mut reasons = Vec::new();
        if rule_effect == RuleEffect::Ask {
            reasons.push("ask rule matched");
        }
        // An explicitly frozen A3 container episode preauthorizes ordinary and
        // unknown programming commands; absolute denials and host ceilings above
        // remain effective. Interactive/project policy is unchanged.
        let unattended_eval = self.noninteractive
            && config.full_acknowledged
            && autonomy == Autonomy::Full
            && token.sandbox_profile.as_deref() == Some("eval");
        if untrusted && report.risk >= Risk::Medium && !unattended_eval {
            reasons.push("risky action motivated only by untrusted content");
        }
        if (tainted || report.sensitive)
            && matches!(action, Action::Network { .. } | Action::Exec { .. })
            && (report.network_command || matches!(action, Action::Network { .. }))
        {
            reasons
                .push("secret-tainted execution requests a network destination or network command");
        }
        match autonomy {
            Autonomy::Consultative
                if !matches!(action, Action::Read { .. } | Action::Data { .. }) =>
            {
                reasons.push("consultative autonomy")
            }
            Autonomy::Supervised
                if outside
                    || report.risk >= Risk::High
                    || matches!(
                        action,
                        Action::Network { .. } | Action::External { .. } | Action::UseSecret { .. }
                    )
                    || report.network_command =>
            {
                reasons.push("supervised autonomy requires approval")
            }
            Autonomy::Autonomous
                if report.risk >= Risk::High
                    || matches!(action, Action::External { .. } | Action::UseSecret { .. }) =>
            {
                reasons.push("risky autonomous operation requires approval")
            }
            _ => {}
        }
        if report.risk == Risk::Unknown && !unattended_eval {
            reasons.push("unknown or unparseable command requires approval");
        }
        if reasons.is_empty() {
            Ok(Decision::Allow)
        } else {
            let reason = reasons.join("; ");
            if self.noninteractive {
                let reason = format!("noninteractive policy denied approval: {reason}");
                self.audit(json!({"kind":"authorization_denied","run":run,"action":action,"reason":reason,"at":now})).await?;
                return Ok(Decision::Deny { reason });
            }
            let approval_id = self
                .request_approval(run, action, &reason, &report, now)
                .await?;
            Ok(Decision::Ask {
                approval_id,
                reason,
            })
        }
    }
    async fn approved(
        &self,
        run: &str,
        project: &str,
        action: &Action,
        now: i64,
        ticket: Option<&str>,
    ) -> Result<Option<(String, ApprovalScope)>> {
        let (run, project, hash, ticket) = (
            run.to_owned(),
            project.to_owned(),
            action_key(action)?,
            ticket.map(str::to_owned),
        );
        let row=self.db.read(ReadLane::Interactive,move|c|{use excalibur_store::rusqlite::OptionalExtension;Ok(c.query_row("SELECT id,scope FROM security_approvals WHERE action_hash=?1 AND status='approved' AND expires_at>?2 AND (scope='Project' AND project=?3 OR scope IN('Once','Task') AND run_id=?4) AND (scope!='Once' OR consumed=0 OR consumed_by=?5) ORDER BY rowid DESC LIMIT 1",excalibur_store::rusqlite::params![hash,now,project,run,ticket],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?)}).await?;
        row.map(|(id, scope)| Ok((id, serde_json::from_str(&format!("\"{scope}\""))?)))
            .transpose()
    }
    async fn request_approval(
        &self,
        run: &str,
        action: &Action,
        reason: &str,
        risk: &RiskReport,
        now: i64,
    ) -> Result<String> {
        let _approval_ordering = self.approval_ordering_lock(run).await?;
        let hash = action_key(action)?;
        let base_id = format!("approval_{}", digest(format!("{run}:{hash}").as_bytes()));
        let config = self.run_security(run).await?;
        let request = json!({"action":action,"reason":reason,"risk":risk,"reversible":matches!(action,Action::Read{..}|Action::Write{..}|Action::WriteExact{..}),"proposed_scope":"Once"});
        let (run, project, text, redactor) = (
            run.to_owned(),
            config.project,
            serde_json::to_string(&self.redactor.redact_json(&request))?,
            self.redactor.clone(),
        );
        Ok(self.db.write(Priority::Journal, move |c| {
            use excalibur_store::rusqlite::OptionalExtension;
            if finalization::terminal(&finalization::authoritative_run_state(c,&run)?) { return Err(excalibur_store::Error::Invalid("terminal execution cannot create an approval")); }
            let previous = c.query_row("SELECT id,status,consumed FROM security_approvals WHERE run_id=?1 AND action_hash=?2 ORDER BY rowid DESC LIMIT 1",excalibur_store::rusqlite::params![run,hash],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,bool>(2)?))).optional()?;
            if let Some((id,status,consumed)) = &previous && (status != "approved" || !consumed) { return Ok(id.clone()); }
            let id = match previous { None=>base_id, Some((id,_,_))=>format!("{base_id}_{}",digest(id.as_bytes())) };
            c.execute("INSERT INTO approvals(id,run_id,content,deadline,state) VALUES(?1,?2,?3,?4,'pending')",excalibur_store::rusqlite::params![id,run,text,now+7*86400])?;
            c.execute("INSERT INTO security_approvals(id,run_id,project,action_hash,request,created_at,expires_at,status,scope,consumed) VALUES(?1,?2,?3,?4,?5,?6,?7,'pending','Once',0)",excalibur_store::rusqlite::params![id,run,project,hash,text,now,now+7*86400])?;
            append_audit(c,&redactor,json!({"kind":"approval_requested","id":id,"run":run,"request":serde_json::from_str::<Value>(&text).unwrap(),"at":now}))?;
            Ok(id)
        }).await?)
    }
    /// Reopen a rejected/expired request only after a new authenticated user instruction.
    /// Automatic tool retries never call this; pending retries retain their original ID.
    pub async fn renew_approval(
        &self,
        id: &str,
        authenticated_instruction: &str,
        now: i64,
    ) -> Result<bool> {
        if authenticated_instruction.is_empty() {
            return Err(Error::Denied(
                "authenticated instruction ID required".into(),
            ));
        }
        let lookup = id.to_owned();
        let run: String = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT run_id FROM security_approvals WHERE id=?1",
                    [lookup],
                    |row| row.get(0),
                )?)
            })
            .await?;
        let _approval_ordering = self.approval_ordering_lock(&run).await?;
        let (id, instruction, redactor) = (
            id.to_owned(),
            authenticated_instruction.to_owned(),
            self.redactor.clone(),
        );
        Ok(self.db.write(Priority::Journal,move|c| {
            if finalization::terminal(&finalization::authoritative_run_state(c,&run)?) { return Err(excalibur_store::Error::Invalid("terminal execution cannot renew an approval")); }
            let changed = c.execute("UPDATE security_approvals SET status='pending',created_at=?2,expires_at=?3,response_id=NULL,reminded=0 WHERE id=?1 AND status IN('rejected','expired')",excalibur_store::rusqlite::params![id,now,now+7*86400])?;
            if changed != 0 {
                c.execute("UPDATE approvals SET state='pending',response=NULL,deadline=?2 WHERE id=?1",excalibur_store::rusqlite::params![id,now+7*86400])?;
                append_audit(c,&redactor,json!({"kind":"approval_renewed","id":id,"instruction":instruction,"at":now}))?;
            }
            Ok(changed != 0)
        }).await?)
    }
    /// The host must authenticate this user response before calling this API.
    pub async fn answer_approval(
        &self,
        id: &str,
        approved: bool,
        scope: ApprovalScope,
        response_id: &str,
        now: i64,
    ) -> Result<bool> {
        if response_id.is_empty() {
            return Err(Error::Denied("authenticated response ID required".into()));
        }
        let (id, response, scope, redactor) = (
            id.to_owned(),
            response_id.to_owned(),
            format!("{scope:?}"),
            self.redactor.clone(),
        );
        Ok(self.db.write(Priority::Journal,move|c|{
            let status=if approved{"approved"}else{"rejected"};
            let changed=c.execute("UPDATE security_approvals SET status=?2,scope=?3,response_id=?4,expires_at=CASE WHEN ?2='approved' AND ?3='Project' THEN 9223372036854775807 ELSE expires_at END WHERE id=?1 AND status='pending' AND expires_at>?5",excalibur_store::rusqlite::params![id,status,scope,response,now])?;
            if changed==0{return Ok(false);}
            c.execute("UPDATE approvals SET state=?2,response=?3,extent=?4 WHERE id=?1",excalibur_store::rusqlite::params![id,status,response,scope])?;
            append_audit(c,&redactor,json!({"kind":"approval_answered","id":id,"approved":approved,"scope":scope,"response_id":response,"at":now}))?;Ok(true)
        }).await?)
    }
    pub async fn request_grant(
        &self,
        run: &str,
        grant: &CapabilityToken,
        expires_at: i64,
        reason: &str,
        now: i64,
    ) -> Result<String> {
        grant.validate().map_err(Error::Denied)?;
        let config = self.run_security(run).await?;
        let limit = match config.parent {
            Some(parent) => self.effective_token(&parent, now).await?,
            None => (*self.ceiling).clone(),
        };
        if expires_at <= now || !limit.contains(grant) {
            return Err(Error::Denied(
                "grant exceeds parent or expiry; escalate to parent authority".into(),
            ));
        }
        let action = Action::External {
            kind: "capability_grant".into(),
            target: serde_json::to_string(&json!({"grant":grant,"expires_at":expires_at}))?,
        };
        self.request_approval(
            run,
            &action,
            reason,
            &RiskReport {
                risk: Risk::High,
                reasons: vec!["capability expansion".into()],
                network_command: false,
                sensitive: true,
            },
            now,
        )
        .await
    }
    pub async fn grant(
        &self,
        run: &str,
        grant: CapabilityToken,
        approval: &str,
        expires_at: i64,
        now: i64,
    ) -> Result<()> {
        grant.validate().map_err(Error::Denied)?;
        if expires_at <= now {
            return Err(Error::Denied("grant already expired".into()));
        }
        let config = self.run_security(run).await?;
        let limit = match config.parent {
            Some(parent) => self.effective_token(&parent, now).await?,
            None => (*self.ceiling).clone(),
        };
        if !limit.contains(&grant) {
            return Err(Error::Denied(
                "grant exceeds parent; escalate to that authority".into(),
            ));
        }
        let proposal = Action::External {
            kind: "capability_grant".into(),
            target: serde_json::to_string(&json!({"grant":grant,"expires_at":expires_at}))?,
        };
        let expected_hash = action_key(&proposal)?;
        let (run, approval, text, redactor) = (
            run.to_owned(),
            approval.to_owned(),
            serde_json::to_string(&grant)?,
            self.redactor.clone(),
        );
        self.db.write(Priority::Journal,move|c|{
            let id=format!("grant_{}",digest(format!("{run}:{approval}:{text}:{expires_at}").as_bytes()));
            let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM capability_grants WHERE id=?1)",[&id],|r|r.get(0))?;if exists{return Ok(());}
            let valid:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM security_approvals WHERE id=?1 AND run_id=?2 AND action_hash=?3 AND status='approved' AND expires_at>?4)",excalibur_store::rusqlite::params![approval,run,expected_hash,now],|r|r.get(0))?;
            if !valid{return Err(excalibur_store::Error::Invalid("exact grant proposal approval required"));}
            c.execute("INSERT INTO capability_grants(id,run_id,grant_definition,extent,expires_at,approval_id) VALUES(?1,?2,?3,'task',?4,?5)",excalibur_store::rusqlite::params![id,run,text,expires_at,approval])?;
            append_audit(c,&redactor,json!({"kind":"capability_grant","id":id,"run":run,"approval":approval,"grant":serde_json::from_str::<Value>(&text).unwrap(),"expires_at":expires_at,"at":now}))?;Ok(())
        }).await?;
        Ok(())
    }
    pub(crate) async fn admit(
        &self,
        run: &str,
        prepared: &excalibur_types::runtime::PreparedTool,
        actions: &[Action],
        now: i64,
    ) -> Result<String> {
        let config = self.run_security(run).await?;
        let mut approvals = Vec::new();
        for action in actions {
            match self.authorize(run, action, now).await? {
                Decision::Allow => {}
                other => {
                    return Err(Error::Denied(format!(
                        "authorization changed before admission: {other:?}"
                    )));
                }
            }
            if let Some((id, scope)) = self
                .approved(run, &config.project, action, now, None)
                .await?
                && scope == ApprovalScope::Once
            {
                approvals.push(id);
            }
        }
        let call_hash = digest(&serde_json::to_vec(prepared)?);
        let ticket = format!("permit_{}", digest(format!("{run}:{call_hash}").as_bytes()));
        let (run, ticket_copy, actions, approval_ids, redactor) = (
            run.to_owned(),
            ticket.clone(),
            serde_json::to_string(actions)?,
            serde_json::to_string(&approvals)?,
            self.redactor.clone(),
        );
        self.db.write(Priority::Journal,move|c|{c.execute("INSERT OR IGNORE INTO security_admissions(ticket,run_id,call_hash,expires_at,actions,approval_ids) VALUES(?1,?2,?3,?4,?5,?6)",excalibur_store::rusqlite::params![ticket_copy,run,call_hash,now+3600,actions,approval_ids])?;append_audit(c,&redactor,json!({"kind":"tool_admitted","run":run,"ticket":ticket_copy,"at":now}))?;Ok(())}).await?;
        Ok(ticket)
    }
    pub(crate) async fn validate_ticket(
        &self,
        run: &str,
        prepared: &excalibur_types::runtime::PreparedTool,
        ticket: &str,
        now: i64,
    ) -> Result<Vec<Action>> {
        let (ticket_copy, run_copy, call_hash) = (
            ticket.to_owned(),
            run.to_owned(),
            digest(&serde_json::to_vec(prepared)?),
        );
        let (actions,ids)=self.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT actions,approval_ids FROM security_admissions WHERE ticket=?1 AND run_id=?2 AND call_hash=?3 AND expires_at>?4",excalibur_store::rusqlite::params![ticket_copy,run_copy,call_hash,now],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?)).await?;
        let actions: Vec<Action> = serde_json::from_str(&actions)?;
        let mut ids: Vec<String> = serde_json::from_str(&ids)?;
        let config = self.run_security(run).await?;
        // Admission does not freeze trust or rules: a secret read/new policy can
        // require an approval after preparation but before actual dispatch.
        for action in &actions {
            match self
                .authorize_with_ticket(run, action, now, Some(ticket))
                .await?
            {
                Decision::Allow => {}
                Decision::Ask { approval_id, .. } => {
                    return Err(Error::ApprovalPending(approval_id));
                }
                Decision::Deny { reason } => return Err(Error::Denied(reason)),
            }
            if let Some((id, scope)) = self
                .approved(run, &config.project, action, now, Some(ticket))
                .await?
                && scope == ApprovalScope::Once
                && !ids.contains(&id)
            {
                ids.push(id);
            }
        }
        let ticket = ticket.to_owned();
        self.db.write(Priority::Journal,move|c|{for id in ids {let changed=c.execute("UPDATE security_approvals SET consumed=1,consumed_by=?2 WHERE id=?1 AND status='approved' AND expires_at>?3 AND (consumed=0 OR consumed_by=?2)",excalibur_store::rusqlite::params![id,ticket,now])?;if changed==0{return Err(excalibur_store::Error::Invalid("single-use approval already used by another operation"));}}Ok(())}).await?;
        Ok(actions)
    }
    pub(crate) async fn dispatch_state(
        &self,
        ticket: &str,
    ) -> Result<(bool, Option<excalibur_types::runtime::ToolResult>)> {
        let ticket = ticket.to_owned();
        let (dispatched, result) = self
            .db
            .read(ReadLane::Interactive, move |c| {
                use excalibur_store::rusqlite::OptionalExtension;
                Ok(c.query_row(
                    "SELECT dispatched,result FROM security_dispatch WHERE ticket=?1",
                    [ticket],
                    |r| Ok((r.get::<_, bool>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .optional()?
                .unwrap_or((false, None)))
            })
            .await?;
        Ok((
            dispatched,
            result.map(|text| serde_json::from_str(&text)).transpose()?,
        ))
    }
    pub(crate) async fn begin_dispatch(&self, ticket: &str) -> Result<bool> {
        let (ticket, redactor) = (ticket.to_owned(), self.redactor.clone());
        Ok(self.db.write(Priority::Journal,move|c| {
            let changed=c.execute("INSERT INTO security_dispatch(ticket,dispatched) VALUES(?1,1) ON CONFLICT(ticket) DO NOTHING",[&ticket])?;
            if changed!=0 { append_audit(c,&redactor,json!({"kind":"tool_dispatch_started","ticket":ticket}))?; }
            Ok(changed!=0)
        }).await?)
    }
    pub(crate) async fn finish_dispatch(
        &self,
        ticket: &str,
        result: &excalibur_types::runtime::ToolResult,
    ) -> Result<()> {
        let (ticket, text, redactor) = (
            ticket.to_owned(),
            serde_json::to_string(&self.redactor.redact_json(&serde_json::to_value(result)?))?,
            self.redactor.clone(),
        );
        self.db.write(Priority::Journal,move|c| {
            c.execute("UPDATE security_dispatch SET result=?2 WHERE ticket=?1",excalibur_store::rusqlite::params![ticket,text])?;
            append_audit(c,&redactor,json!({"kind":"tool_dispatch_completed","ticket":ticket,"result":serde_json::from_str::<Value>(&text).unwrap()}))?;
            Ok(())
        }).await?;
        Ok(())
    }
    pub async fn approval_blocked(&self, run: &str, now: i64) -> Result<Option<String>> {
        self.approval_maintenance(now).await?;
        let run = run.to_owned();
        Ok(self.db.read(ReadLane::Interactive,move|c|{use excalibur_store::rusqlite::OptionalExtension;Ok(c.query_row("SELECT status FROM security_approvals WHERE run_id=?1 AND status IN('rejected','expired') ORDER BY rowid DESC LIMIT 1",[run],|r|r.get::<_,String>(0)).optional()?)}).await?.map(|state|format!("approval {state}")))
    }
    pub(crate) async fn record_action_result(
        &self,
        run: &str,
        actions: &[Action],
        now: i64,
    ) -> Result<()> {
        for action in actions {
            match action {
                Action::Read { path } if taints(path) => {
                    self.state_fact(
                        run,
                        "tainted_secret",
                        json!({"kind":"sensitive_file_read","path":path,"at":now}),
                    )
                    .await?
                }
                Action::Exec { .. } if classify(action).sensitive => {
                    self.state_fact(
                        run,
                        "tainted_secret",
                        json!({"kind":"sensitive_command","at":now}),
                    )
                    .await?
                }
                Action::UseSecret { reference } => {
                    self.record_secret_use(run, reference, now).await?
                }
                Action::Network { domain } => {
                    let (run, domain) = (run.to_owned(), domain.to_ascii_lowercase());
                    self.db
                        .write(Priority::Normal, move |c| {
                            c.execute(
                                "INSERT OR IGNORE INTO security_domains VALUES(?1,?2)",
                                [run, domain],
                            )?;
                            Ok(())
                        })
                        .await?;
                }
                _ => {}
            }
        }
        self.mark_untrusted(run, "tool_output", now).await
    }
    pub async fn mark_untrusted(&self, run: &str, source: &str, now: i64) -> Result<()> {
        self.state_fact(
            run,
            "untrusted_seen",
            json!({"kind":"untrusted_input","source":source,"at":now}),
        )
        .await
    }
    pub async fn record_secret_use(&self, run: &str, reference: &str, now: i64) -> Result<()> {
        let token = self.effective_token(run, now).await?;
        if !token.secrets.contains(reference) {
            return Err(Error::Denied("secret reference outside capability".into()));
        }
        self.state_fact(
            run,
            "tainted_secret",
            json!({"kind":"secret_used","reference":reference,"at":now}),
        )
        .await
    }
    async fn state_fact(&self, run: &str, column: &'static str, event: Value) -> Result<()> {
        let (run, redactor) = (run.to_owned(), self.redactor.clone());
        self.db
            .write(Priority::Journal, move |c| {
                c.execute(
                    &format!("UPDATE security_state SET {column}=1 WHERE run_id=?1"),
                    [&run],
                )?;
                append_audit(c, &redactor, json!({"run":run,"event":event}))?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    pub async fn audit(&self, event: Value) -> Result<()> {
        let redactor = self.redactor.clone();
        self.db
            .write(Priority::Journal, move |c| {
                append_audit(c, &redactor, event)
            })
            .await?;
        Ok(())
    }
    pub async fn verify_audit(&self) -> Result<u64> {
        Ok(self.db.read(ReadLane::Interactive, |c| {
            let mut stmt = c.prepare("SELECT sequence,previous_hash,hash,redacted_event FROM audit ORDER BY sequence")?;
            let mut rows = stmt.query([])?;
            let mut previous = String::new();
            let mut sequence = 0_u64;
            while let Some(row) = rows.next()? {
                sequence += 1;
                let actual_sequence = row.get::<_,u64>(0)?;
                let predecessor = row.get::<_,String>(1)?;
                let hash = row.get::<_,String>(2)?;
                let text = row.get::<_,String>(3)?;
                let expected = digest(format!("{sequence}\0{previous}\0{text}").as_bytes());
                if actual_sequence != sequence || predecessor != previous || hash != expected {
                    return Err(excalibur_store::Error::Invalid("audit chain sequence, predecessor or hash mismatch"));
                }
                previous = expected;
            }
            Ok(sequence)
        }).await?)
    }
    pub async fn audit_query(&self, limit: usize) -> Result<Vec<Value>> {
        let limit = limit.min(100000) as i64;
        Ok(self.db.read(ReadLane::Interactive,move|c|{let mut s=c.prepare("SELECT sequence,previous_hash,hash,redacted_event FROM audit ORDER BY sequence LIMIT ?1")?;Ok(s.query_map([limit],|r|Ok(json!({"sequence":r.get::<_,i64>(0)?,"previous_hash":r.get::<_,String>(1)?,"hash":r.get::<_,String>(2)?,"redacted_event":r.get::<_,String>(3)?})))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?)
    }
    pub async fn pending_approval(&self, run: &str, now: i64) -> Result<bool> {
        let run = run.to_owned();
        Ok(self.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM security_approvals WHERE run_id=?1 AND status='pending' AND expires_at>?2)",excalibur_store::rusqlite::params![run,now],|r|r.get(0))?)).await?)
    }
    pub async fn approval_maintenance(&self, now: i64) -> Result<Value> {
        let redactor = self.redactor.clone();
        Ok(self.db.write(Priority::Journal,move|c|{let expired=c.execute("UPDATE security_approvals SET status='expired' WHERE status='pending' AND expires_at<=?1",[now])?;c.execute("UPDATE approvals SET state='rejected' WHERE id IN(SELECT id FROM security_approvals WHERE status='expired')",[])?;let reminders=c.execute("UPDATE security_approvals SET reminded=1 WHERE status='pending' AND reminded=0 AND created_at<=?1",[now-86400])?;append_audit(c,&redactor,json!({"kind":"approval_maintenance","expired":expired,"urgent_reminders":reminders,"at":now}))?;Ok(json!({"expired":expired,"urgent_reminders":reminders}))}).await?)
    }
}
fn append_audit(
    c: &excalibur_store::rusqlite::Connection,
    redactor: &Redactor,
    event: Value,
) -> excalibur_store::Result<()> {
    use excalibur_store::rusqlite::OptionalExtension;
    let last = c
        .query_row(
            "SELECT sequence,hash FROM audit ORDER BY sequence DESC LIMIT 1",
            [],
            |r| Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    let (sequence, previous) = last.map_or((1, String::new()), |(n, h)| (n + 1, h));
    let event = serde_json::to_string(&redactor.redact_json(&event))
        .map_err(|_| excalibur_store::Error::Invalid("audit serialization"))?;
    let hash = digest(format!("{sequence}\0{previous}\0{event}").as_bytes());
    c.execute(
        "INSERT INTO audit(sequence,previous_hash,hash,redacted_event) VALUES(?1,?2,?3,?4)",
        excalibur_store::rusqlite::params![sequence, previous, hash, event],
    )?;
    Ok(())
}

//! Durable, scope-bound tasks, plans and verifiable work.
pub mod checklist;
mod control;
pub mod evidence;
mod milestones;
mod progress;
mod snapshot;
pub mod verification;
pub use checklist::*;
pub use evidence::*;
pub use verification::*;
pub mod entities;
mod ops;
mod planning;
pub use entities::*;
use excalibur_store::{
    Database, Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock, Weak},
};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("task storage: {0}")]
    Store(#[from] excalibur_store::Error),
    #[error("task journal: {0}")]
    Journal(#[from] excalibur_journal::Error),
    #[error("task serialization failed")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}
impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone)]
pub struct TaskStore {
    pub(crate) db: Database,
    pub(crate) mutation_lock: Arc<tokio::sync::RwLock<()>>,
    pub(crate) redactor: Redactor,
    pub(crate) policy: TaskPolicy,
}
impl std::fmt::Debug for TaskStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskStore").finish_non_exhaustive()
    }
}
fn fail(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", ulid::Ulid::new())
}
type Locks = Mutex<BTreeMap<String, Weak<tokio::sync::RwLock<()>>>>;
static LOCKS: OnceLock<Locks> = OnceLock::new();
impl TaskStore {
    pub async fn open(db: Database) -> Result<Self> {
        excalibur_journal::Journal::new(db.clone()).await?;
        let sql = include_str!("schema.sql");
        let hash = digest(sql.as_bytes());
        db.write(Priority::Journal,move|c|{
            c.execute_batch("CREATE TABLE IF NOT EXISTS tasks_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL)")?;
            let old:Option<String>=c.query_row("SELECT checksum FROM tasks_schema WHERE version=1",[],|r|r.get(0)).optional()?;
            match old {Some(old)if old!=hash=>return Err(excalibur_store::Error::Invalid("tasks schema checksum mismatch")),Some(_)=>{},None=>{c.execute_batch(sql)?;c.execute("INSERT INTO tasks_schema VALUES(1,?1)",[hash])?;}}
            let legacy:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='eval_episode_records')",[],|r|r.get(0))?;
            if legacy{c.execute_batch("INSERT OR IGNORE INTO task_drafts(scope_id,principal_key,id,entity,version,data) SELECT scope,'',id,entity,version,data FROM eval_episode_records")?;}
            let receipts:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='eval_task_receipts')",[],|r|r.get(0))?;
            if receipts{c.execute_batch("INSERT OR IGNORE INTO task_batch_receipts(scope_id,principal_key,operation_key,request_hash,result) SELECT scope,'',operation_key,request_hash,result FROM eval_task_receipts")?;}
            Ok(())
        }).await?;
        let key = db.path().to_string_lossy().to_string();
        let mutation_lock = {
            let mut locks = LOCKS.get_or_init(Default::default).lock().unwrap();
            locks.retain(|_, v| v.strong_count() > 0);
            match locks.get(&key).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(tokio::sync::RwLock::new(()));
                    locks.insert(key, Arc::downgrade(&lock));
                    lock
                }
            }
        };
        let store = Self {
            db,
            mutation_lock,
            redactor: Redactor::default(),
            policy: TaskPolicy::default(),
        };
        store.initialize_evidence().await?;
        Ok(store)
    }
    pub fn with_policy(mut self, policy: TaskPolicy) -> Self {
        self.policy = policy;
        self
    }
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }
    pub fn database(&self) -> &Database {
        &self.db
    }
    pub fn validate_batch(&self, scope: &str, args: &Value) -> Result<()> {
        self.check_scope(scope)?;
        ops::validate(scope, args, &self.policy)
    }
    /// Trusted host entry point; never exposed as a model tool operation.
    pub async fn host_batch(&self, scope: &str, key: &str, args: Value) -> Result<Value> {
        let mut policy = self.policy.clone();
        policy.trusted_host = true;
        self.clone()
            .with_policy(policy)
            .batch(scope, key, args)
            .await
    }
    pub async fn batch(&self, scope: &str, key: &str, args: Value) -> Result<Value> {
        self.validate_batch(scope, &args)?;
        if key.is_empty() || key.len() > 1024 {
            return Err(fail("operation key required, at most 1024 bytes"));
        }
        let _guard = self.mutation_lock.write().await;
        let hash = digest(&serde_json::to_vec(&args)?);
        let args = self.redactor.redact_json(&args);
        let assessments = if args["ops"].as_array().is_some_and(|ops| {
            ops.iter().any(|op| {
                op["op"] == "query"
                    && matches!(op["entity"].as_str(), Some("criterion" | "criteria"))
            })
        }) {
            self.compute_criteria_scope_locked(scope).await?
        } else {
            Vec::new()
        };
        let scope = scope.to_owned();
        let key = key.to_owned();
        let policy = self.policy.clone();
        let redactor = self.redactor.clone();
        self.db.write(Priority::Normal,move|c|{
            let principal=policy.principal_id.clone().unwrap_or_default();
            let old:Option<(String,String)>=c.query_row("SELECT request_hash,result FROM task_batch_receipts WHERE scope_id=?1 AND principal_key=?2 AND operation_key=?3",rusqlite::params![scope,principal,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((old,result))=old{if old!=hash{return Err(excalibur_store::Error::Invalid("task operation key reused with different arguments"));}return serde_json::from_str(&result).map_err(|_|excalibur_store::Error::Invalid("task receipt corrupt"));}
            for (assessment,revision) in assessments{let state=serde_json::to_value(assessment.state).map_err(|_|excalibur_store::Error::Invalid("criterion state invalid"))?.as_str().unwrap_or("unverified").to_owned();c.execute("INSERT INTO task_criterion_state(criterion_id,state,reason,latest_evidence_id,instruction_revision) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(criterion_id) DO UPDATE SET state=excluded.state,reason=excluded.reason,latest_evidence_id=excluded.latest_evidence_id,instruction_revision=excluded.instruction_revision",rusqlite::params![assessment.criterion_id,state,assessment.reason,assessment.evidence.map(|e|e.id),revision])?;}
            let value=ops::apply(c,&scope,&args,&policy).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;
            let value=redactor.redact_json(&value);
            c.execute("INSERT INTO task_batch_receipts VALUES(?1,?2,?3,?4,?5)",rusqlite::params![scope,principal,key,hash,value.to_string()])?;
            Ok(value)
        }).await.map_err(Error::from)
    }
    pub async fn receipt(&self, scope: &str, key: &str, args: &Value) -> Result<Option<Value>> {
        self.check_scope(scope)?;
        let (scope, key, hash, principal) = (
            scope.to_owned(),
            key.to_owned(),
            digest(&serde_json::to_vec(args)?),
            self.policy.principal_id.clone().unwrap_or_default(),
        );
        let value=self.db.read(ReadLane::Interactive,move|c|{let old:Option<(String,String)>=c.query_row("SELECT request_hash,result FROM task_batch_receipts WHERE scope_id=?1 AND principal_key=?2 AND operation_key=?3",rusqlite::params![scope,principal,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;match old{Some((old,_))if old!=hash=>Err(excalibur_store::Error::Invalid("task operation key reused with different arguments")),Some((_,result))=>serde_json::from_str(&result).map(Some).map_err(|_|excalibur_store::Error::Invalid("task receipt corrupt")),None=>Ok(None)}}).await?;
        Ok(value.map(|v| self.redactor.redact_json(&v)))
    }
    fn check_scope(&self, scope: &str) -> Result<()> {
        if scope.is_empty() || scope.len() > 256 || scope.chars().any(char::is_control) {
            return Err(fail("invalid scope"));
        }
        if self
            .policy
            .allowed_scopes
            .as_ref()
            .is_some_and(|s| !s.contains(scope))
        {
            return Err(fail("task scope is not authorized"));
        }
        Ok(())
    }
    pub async fn task(&self, scope: &str, id: &str) -> Result<TaskRecord> {
        self.check_scope(scope)?;
        let (scope, id, principal) = (
            scope.to_owned(),
            id.to_owned(),
            self.policy.principal_id.clone(),
        );
        let value = self
            .db
            .read(ReadLane::Interactive, move |c| {
                ops::load(c, "task", &scope, &id, principal.as_deref())
                    .map_err(|e| excalibur_store::Error::Configuration(e.to_string()))
            })
            .await?;
        task_record(self.redactor.redact_json(&value))
    }
    pub async fn task_for_run(&self, run: &str) -> Result<Option<TaskRecord>> {
        let run = run.to_owned();
        let principal = self.policy.principal_id.clone();
        let value=self.db.read(ReadLane::Interactive,move|c|{let row:Option<(String,String)>=c.query_row("SELECT t.scope_id,t.id FROM tasks t JOIN task_run_links l ON l.task_id=t.id WHERE l.run_id=?1 AND t.principal_id IS ?2",rusqlite::params![run,principal],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;row.map(|(scope,id)|ops::load(c,"task",&scope,&id,principal.as_deref()).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))).transpose()}).await?;
        let task = value
            .map(|value| task_record(self.redactor.redact_json(&value)))
            .transpose()?;
        if let Some(task) = &task {
            self.check_scope(&task.scope_id)?;
        }
        Ok(task)
    }
    pub async fn compact_micro_plan(&self, run: &str) -> Result<Option<String>> {
        let Some(task) = self.task_for_run(run).await? else {
            return Ok(None);
        };
        let id = task.id;
        let lookup_id = id.clone();
        let scope = task.scope_id;
        let value=self.db.read(ReadLane::Interactive,move|c|{let mut s=c.prepare("SELECT p.id,p.version,i.code,i.title,i.state FROM plans p LEFT JOIN plan_items i ON i.plan_id=p.id WHERE p.owner_task_id=?1 AND p.kind='micro' AND p.state NOT IN('archived','retired') AND p.scope_id=?2 ORDER BY p.version DESC,i.priority DESC,i.id LIMIT 30")?;Ok(s.query_map(rusqlite::params![id,scope],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        let replans = self.pending_replans(&lookup_id).await?;
        if value.is_empty() && replans.is_empty() {
            return Ok(None);
        }
        let mut text = String::new();
        for request in replans {
            let line = format!(
                "Pending replan {}: {}\n",
                request["id"].as_str().unwrap_or_default(),
                request["reason"].as_str().unwrap_or_default()
            );
            if text.len() + line.len() > 4000 {
                break;
            }
            text.push_str(&line);
        }

        for (plan, version, code, title, state) in value {
            let line = format!(
                "{plan}@{version} {}: {} [{}]\n",
                code.unwrap_or_default(),
                title.unwrap_or_default(),
                state.unwrap_or_default()
            );
            if text.len() + line.len() > 4000 {
                break;
            }
            text.push_str(&line);
        }
        Ok(Some(self.redactor.redact_text(&text)))
    }
}
fn task_record(value: Value) -> Result<TaskRecord> {
    let level = match value["level"].as_str().unwrap_or("N1") {
        "N2" => WorkLevel::N2,
        "N3" => WorkLevel::N3,
        _ => WorkLevel::N1,
    };
    Ok(TaskRecord {
        id: value["id"].as_str().unwrap_or_default().into(),
        project_id: value["project_id"].as_str().map(str::to_owned),
        scope_id: value["scope_id"].as_str().unwrap_or("general").into(),
        code: value["code"].as_str().unwrap_or_default().into(),
        title: value["title"].as_str().unwrap_or_default().into(),
        detail: value["detail"].as_str().unwrap_or_default().into(),
        level,
        root_run_id: value["root_run_id"].as_str().map(str::to_owned),
        derived_state: value["derived_state"].as_str().unwrap_or("open").into(),
        version: value["version"].as_u64().unwrap_or(1),
        request_card: value["request_card"].clone(),
        budget: value["budget"].clone(),
        origin_conversation_id: value["origin_conversation_id"].as_str().map(str::to_owned),
        notification_policy: value["notification_policy"].as_str().map(str::to_owned),
        macro_item_id: value["macro_item_id"].as_str().map(str::to_owned),
        forced_level: value["forced_level"]
            .as_bool()
            .unwrap_or_else(|| value["forced_level"].as_u64() == Some(1)),
        estimate_seconds: value["estimate_seconds"].as_u64(),
        metadata: value["metadata"].clone(),
    })
}
pub fn validate_batch(scope: &str, args: &Value) -> Result<()> {
    ops::validate(scope, args, &TaskPolicy::default())
}
pub fn task_schema() -> Value {
    json!({"type":"object","required":["ops"],"properties":{"ops":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"object","required":["op"],"properties":{"op":{"type":"string"},"id":{"type":"string"},"scope":{"type":"string"},"entity":{"type":"string"},"expected_version":{"type":"integer","minimum":1},"data":{"type":"object"},"fields":{"type":"array","items":{"type":"string"}},"where":{"type":"object"},"limit":{"type":"integer","minimum":1,"maximum":20}}}}},"additionalProperties":false})
}

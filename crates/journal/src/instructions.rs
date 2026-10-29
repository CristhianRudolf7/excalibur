//! Literal, revisioned instructions. Mutation is a trusted-host API, never a tool.
use crate::{Error, Journal, Result};
use excalibur_store::{
    Database, Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use excalibur_types::InstructionId;
use excalibur_vault::Redactor;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, Weak},
};
use tokio::sync::{OwnedRwLockReadGuard, RwLock};

type RegistryLocks = Mutex<BTreeMap<PathBuf, Weak<RwLock<()>>>>;
static REGISTRY_LOCKS: OnceLock<RegistryLocks> = OnceLock::new();
fn registry_lock(db: &Database) -> Result<Arc<RwLock<()>>> {
    let path = db
        .path()
        .canonicalize()
        .map_err(excalibur_store::Error::from)?;
    let mut locks = REGISTRY_LOCKS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| Error::Invalid("instruction registry ordering lock unavailable"))?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(RwLock::new(()));
    locks.insert(path, Arc::downgrade(&lock));
    Ok(lock)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum InstructionScope {
    General,
    Project(String),
    Task(String),
    Conversation(String),
}
impl InstructionScope {
    fn parts(&self) -> (&'static str, &str) {
        match self {
            Self::General => ("general", "general"),
            Self::Project(id) => ("project", id),
            Self::Task(id) => ("task", id),
            Self::Conversation(id) => ("conversation", id),
        }
    }
    fn from_parts(kind: &str, id: String) -> Option<Self> {
        Some(match kind {
            "general" => Self::General,
            "project" => Self::Project(id),
            "task" => Self::Task(id),
            "conversation" => Self::Conversation(id),
            _ => return None,
        })
    }
    fn valid(&self) -> bool {
        let (_, id) = self.parts();
        !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstructionOriginKind {
    UserMessage,
    ProjectFile,
    Host,
    Legacy,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstructionOrigin {
    pub kind: InstructionOriginKind,
    pub reference: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstructionState {
    Active,
    Retired,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstructionRecord {
    pub id: String,
    pub revision: u64,
    pub literal_text: String,
    pub origin: InstructionOrigin,
    pub state: InstructionState,
    pub scope: InstructionScope,
    #[serde(default)]
    pub delegated: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstructionSnapshot {
    /// SHA-256 over the selected current revisions, not a token/model estimate.
    pub revision: String,
    pub stable: Vec<InstructionRecord>,
    pub task: Vec<InstructionRecord>,
}
impl InstructionSnapshot {
    pub fn records(&self) -> impl Iterator<Item = &InstructionRecord> {
        self.stable.iter().chain(&self.task)
    }
    pub fn stable_literal(&self) -> String {
        self.stable
            .iter()
            .map(|r| r.literal_text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
    pub fn task_literal(&self) -> String {
        self.task
            .iter()
            .map(|r| r.literal_text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
#[derive(Clone)]
pub struct InstructionStore {
    db: Database,
    redactor: Redactor,
}
const SQL: &str = include_str!("instructions.sql");
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn principal(c: &rusqlite::Connection, run: &str) -> excalibur_store::Result<String> {
    c.query_row(
        "SELECT COALESCE(principal_id,'') FROM runs WHERE id=?1",
        [run],
        |r| r.get(0),
    )
    .map_err(Into::into)
}
async fn ensure_schema(db: &Database) -> Result<()> {
    let checksum = digest(SQL.as_bytes());
    let expected = checksum.clone();
    let initialized = db.read(ReadLane::Interactive, move |c| {
        let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='context_instruction_schema')", [], |r| r.get(0))?;
        if !exists { return Ok(false); }
        let old: Option<String> = c.query_row("SELECT checksum FROM context_instruction_schema WHERE version=1", [], |r| r.get(0)).optional()?;
        match old {
            Some(old) if old == expected => Ok(true),
            Some(_) => Err(excalibur_store::Error::Invalid("instruction schema checksum mismatch")),
            None => Ok(false),
        }
    }).await?;
    if initialized {
        return Ok(());
    }
    db.write(Priority::Journal, move |c| {
        c.execute_batch(SQL)?;
        let old: Option<String> = c
            .query_row(
                "SELECT checksum FROM context_instruction_schema WHERE version=1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            if old != checksum {
                return Err(excalibur_store::Error::Invalid(
                    "instruction schema checksum mismatch",
                ));
            }
        } else {
            c.execute(
                "INSERT INTO context_instruction_schema VALUES(1,?1)",
                [checksum],
            )?;
        }
        Ok(())
    })
    .await?;
    Ok(())
}
impl InstructionStore {
    pub async fn open(db: Database, redactor: Redactor) -> Result<Self> {
        ensure_schema(&db).await?;
        Ok(Self { db, redactor })
    }
    pub async fn register(
        &self,
        operation: &str,
        principal_id: Option<String>,
        scope: InstructionScope,
        origin: InstructionOrigin,
        literal: &str,
    ) -> Result<InstructionRecord> {
        self.mutate(
            operation,
            principal_id,
            scope,
            None,
            None,
            origin,
            literal,
            InstructionState::Active,
        )
        .await
    }
    pub async fn revise(
        &self,
        operation: &str,
        principal_id: Option<String>,
        id: &str,
        expected_revision: u64,
        origin: InstructionOrigin,
        literal: &str,
    ) -> Result<InstructionRecord> {
        let current = self.get(principal_id.clone(), id).await?;
        self.mutate(
            operation,
            principal_id,
            current.scope,
            Some(id.to_owned()),
            Some(expected_revision),
            origin,
            literal,
            InstructionState::Active,
        )
        .await
    }
    pub async fn retire(
        &self,
        operation: &str,
        principal_id: Option<String>,
        id: &str,
        expected_revision: u64,
    ) -> Result<InstructionRecord> {
        let current = self.get(principal_id.clone(), id).await?;
        self.mutate(
            operation,
            principal_id,
            current.scope,
            Some(id.to_owned()),
            Some(expected_revision),
            current.origin,
            &current.literal_text,
            InstructionState::Retired,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn mutate(
        &self,
        operation: &str,
        principal_id: Option<String>,
        scope: InstructionScope,
        id: Option<String>,
        expected: Option<u64>,
        origin: InstructionOrigin,
        literal: &str,
        state: InstructionState,
    ) -> Result<InstructionRecord> {
        if operation.is_empty()
            || operation.len() > 256
            || !scope.valid()
            || literal.trim().is_empty()
            || literal.len() > 64 * 1024
            || origin.reference.is_empty()
            || origin.reference.len() > 2048
        {
            return Err(Error::Invalid("invalid instruction arguments"));
        }
        if self.redactor.redact_text(literal) != literal
            || self.redactor.redact_json(&json!(origin)) != json!(origin)
        {
            return Err(Error::Invalid(
                "secret literals cannot enter the instruction registry",
            ));
        }
        let _registry_write = registry_lock(&self.db)?.write_owned().await;
        let key = principal_id.clone().unwrap_or_default();
        let operation = operation.to_owned();
        let literal = literal.to_owned();
        let origin_text = serde_json::to_string(&origin)?;
        let request=digest(serde_json::to_string(&json!({"id":id,"expected":expected,"scope":scope,"origin":origin,"literal":literal,"state":state}))?.as_bytes());
        let id = id.unwrap_or_else(|| InstructionId::new().to_string());
        let result=self.db.write(Priority::Journal,move|c|{
            let receipt:Option<(String,String)>=c.query_row("SELECT request_hash,result FROM context_instruction_receipts WHERE principal_key=?1 AND operation_key=?2",rusqlite::params![key,operation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((hash,result))=receipt{if hash!=request{return Err(excalibur_store::Error::Invalid("instruction operation key conflict"));}return Ok(result);}
            let revision=if let Some(expected)=expected{
                let(old,owner):(u64,String)=c.query_row("SELECT revision,COALESCE(principal_id,'') FROM instructions WHERE id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?)))?;
                if owner!=key||old!=expected{return Err(excalibur_store::Error::Invalid("instruction revision or principal conflict"));}old.checked_add(1).ok_or(excalibur_store::Error::Invalid("instruction revision overflow"))?
            }else{1};
            if origin.kind==InstructionOriginKind::UserMessage{
                let trusted:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1 AND role='user' AND COALESCE(principal_id,'')=?2)",rusqlite::params![origin.reference,key],|r|r.get(0))?;
                if !trusted{return Err(excalibur_store::Error::Invalid("instruction source must be a user message owned by this principal"));}
            }
            let(kind,scope_key)=scope.parts();let state_name=if state==InstructionState::Active{"active"}else{"retired"};
            if expected.is_some(){
                // Preserve the original baseline row before its first managed revision.
                let previous = record(c, &id, &key, false)?;
                let previous_origin = serde_json::to_string(&previous.origin).map_err(|_| excalibur_store::Error::Invalid("invalid instruction origin"))?;
                let (previous_kind, previous_scope) = previous.scope.parts();
                let previous_state = if previous.state == InstructionState::Active { "active" } else { "retired" };
                c.execute("INSERT OR IGNORE INTO context_instruction_revisions(instruction_id,revision,literal_text,origin,state,principal_key,kind,scope_key) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", rusqlite::params![id,previous.revision,previous.literal_text,previous_origin,previous_state,key,previous_kind,previous_scope])?;
                c.execute("INSERT OR IGNORE INTO context_instruction_scopes VALUES(?1,?2,?3)", rusqlite::params![id,kind,scope_key])?;
                c.execute("UPDATE instructions SET literal_text=?2,revision=?3,origin=?4,state=?5,updated_at=unixepoch() WHERE id=?1",rusqlite::params![id,literal,revision,origin_text,state_name])?;}else{
                c.execute("INSERT INTO instructions(id,literal_text,revision,origin,state,principal_id,scope_id) VALUES(?1,?2,?3,?4,?5,?6,?7)",rusqlite::params![id,literal,revision,origin_text,state_name,principal_id,scope_key])?;
                c.execute("INSERT INTO context_instruction_scopes VALUES(?1,?2,?3)",rusqlite::params![id,kind,scope_key])?;
            }
            c.execute("INSERT INTO context_instruction_revisions(instruction_id,revision,literal_text,origin,state,principal_key,kind,scope_key) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",rusqlite::params![id,revision,literal,origin_text,state_name,key,kind,scope_key])?;
            let record=InstructionRecord{id,revision,literal_text:literal,origin,state,scope,delegated:false};let text=serde_json::to_string(&record).map_err(|_|excalibur_store::Error::Invalid("instruction serialization failed"))?;
            c.execute("INSERT INTO context_instruction_receipts VALUES(?1,?2,?3,?4)",rusqlite::params![key,operation,request,text])?;Ok(text)
        }).await?;
        Ok(serde_json::from_str(&result)?)
    }
    pub async fn get(&self, principal_id: Option<String>, id: &str) -> Result<InstructionRecord> {
        let (key, id) = (principal_id.unwrap_or_default(), id.to_owned());
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| record(c, &id, &key, false))
            .await?)
    }
    /// Every retained literal revision in ascending order; current ownership is required.
    pub async fn history(
        &self,
        principal_id: Option<String>,
        id: &str,
    ) -> Result<Vec<InstructionRecord>> {
        let (key, id) = (principal_id.unwrap_or_default(), id.to_owned());
        Ok(self.db.read(ReadLane::Interactive,move |c| {
            let current=record(c,&id,&key,false)?;
            let mut statement=c.prepare("SELECT revision,literal_text,origin,state,kind,scope_key FROM context_instruction_revisions WHERE instruction_id=?1 AND principal_key=?2 ORDER BY revision")?;
            let rows=statement.query_map(rusqlite::params![id,key],|r| Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let mut result=Vec::new();
            for(revision,literal_text,origin,state,kind,scope_key) in rows {
                result.push(InstructionRecord{id:id.clone(),revision,literal_text,origin:serde_json::from_str(&origin).map_err(|_| excalibur_store::Error::Invalid("invalid instruction history origin"))?,state:if state=="active"{InstructionState::Active}else{InstructionState::Retired},scope:InstructionScope::from_parts(&kind,scope_key).ok_or(excalibur_store::Error::Invalid("invalid instruction history scope"))?,delegated:false});
            }
            if result.is_empty(){result.push(current);}
            Ok(result)
        }).await?)
    }
    /// Classify an existing legacy scope explicitly. This cannot alter its literal or owner.
    pub async fn bind_legacy_scope(
        &self,
        principal_id: Option<String>,
        id: &str,
        scope: InstructionScope,
    ) -> Result<()> {
        if !scope.valid() {
            return Err(Error::Invalid("invalid legacy instruction scope"));
        }
        let _registry_write = registry_lock(&self.db)?.write_owned().await;
        let (key, id) = (principal_id.unwrap_or_default(), id.to_owned());
        self.db.write(Priority::Journal,move |c| {
            let owned:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM instructions WHERE id=?1 AND COALESCE(principal_id,'')=?2)",rusqlite::params![id,key],|r|r.get(0))?;
            if !owned{return Err(excalibur_store::Error::Invalid("instruction principal mismatch"));}
            let(kind,scope_key)=scope.parts();
            let old:Option<(String,String)>=c.query_row("SELECT kind,scope_key FROM context_instruction_scopes WHERE instruction_id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((old_kind,old_key))=old {
                if old_kind!=kind||old_key!=scope_key{return Err(excalibur_store::Error::Invalid("instruction scope is immutable"));}
            }else{c.execute("INSERT INTO context_instruction_scopes VALUES(?1,?2,?3)",rusqlite::params![id,kind,scope_key])?;}
            Ok(())
        }).await?;
        Ok(())
    }
    /// Host bindings are immutable, principal-checked and never model arguments.
    pub async fn bind_run(
        &self,
        run: &str,
        principal_id: Option<String>,
        scopes: Vec<InstructionScope>,
    ) -> Result<()> {
        if scopes.iter().any(|scope| !scope.valid()) || scopes.len() > 32 {
            return Err(Error::Invalid("invalid run instruction scopes"));
        }
        let _registry_write = registry_lock(&self.db)?.write_owned().await;
        let (run, key) = (run.to_owned(), principal_id.unwrap_or_default());
        let scopes = serde_json::to_string(&scopes.into_iter().collect::<BTreeSet<_>>())?;
        self.db
            .write(Priority::Journal, move |c| {
                if principal(c, &run)? != key {
                    return Err(excalibur_store::Error::Invalid(
                        "run instruction principal mismatch",
                    ));
                }
                let old: Option<String> = c
                    .query_row(
                        "SELECT scopes FROM context_instruction_run_bindings WHERE run_id=?1",
                        [&run],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(old) = old {
                    if old != scopes {
                        return Err(excalibur_store::Error::Invalid(
                            "run instruction scopes are already bound",
                        ));
                    }
                } else {
                    c.execute(
                        "INSERT INTO context_instruction_run_bindings VALUES(?1,?2,?3)",
                        rusqlite::params![run, key, scopes],
                    )?;
                }
                Ok(())
            })
            .await?;
        Ok(())
    }
    pub async fn delegate(&self, parent: &str, child: &str, ids: &[String]) -> Result<()> {
        let _registry_write = registry_lock(&self.db)?.write_owned().await;
        let visible = Journal {
            db: self.db.clone(),
        }
        .context_instructions(parent)
        .await?;
        let allowed = visible
            .records()
            .map(|record| record.id.clone())
            .collect::<BTreeSet<_>>();
        if ids.len() > 256 || ids.iter().any(|id| !allowed.contains(id)) {
            return Err(Error::Invalid(
                "instruction delegation exceeds the parent's selected instructions",
            ));
        }
        let (parent, child, ids) = (parent.to_owned(), child.to_owned(), ids.to_vec());
        self.db
            .write(Priority::Journal, move |c| {
                if parent == child || principal(c, &parent)? != principal(c, &child)? {
                    return Err(excalibur_store::Error::Invalid(
                        "cross-principal or self instruction delegation",
                    ));
                }
                for id in ids {
                    let revision: u64 = c.query_row(
                        "SELECT revision FROM instructions WHERE id=?1",
                        [&id],
                        |r| r.get(0),
                    )?;
                    c.execute(
                        "INSERT OR IGNORE INTO context_instruction_delegations VALUES(?1,?2,?3,?4)",
                        rusqlite::params![child, parent, id, revision],
                    )?;
                }
                Ok(())
            })
            .await?;
        Ok(())
    }
}
fn record(
    c: &rusqlite::Connection,
    id: &str,
    key: &str,
    delegated: bool,
) -> excalibur_store::Result<InstructionRecord> {
    let(literal,revision,origin,state,scope_id):(String,u64,Option<String>,String,Option<String>)=c.query_row("SELECT literal_text,revision,origin,state,scope_id FROM instructions WHERE id=?1 AND COALESCE(principal_id,'')=?2",rusqlite::params![id,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    let scope: Option<(String, String)> = c
        .query_row(
            "SELECT kind,scope_key FROM context_instruction_scopes WHERE instruction_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let scope = if let Some((kind, id)) = scope {
        InstructionScope::from_parts(&kind, id)
    } else {
        let id = scope_id.unwrap_or_else(|| "general".into());
        if id == "general" {
            Some(InstructionScope::General)
        } else if c.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)",
            [&id],
            |r| r.get::<_, bool>(0),
        )? {
            Some(InstructionScope::Project(id))
        } else if c.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",
            [&id],
            |r| r.get::<_, bool>(0),
        )? {
            Some(InstructionScope::Task(id))
        } else if c.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1)",
            [&id],
            |r| r.get::<_, bool>(0),
        )? {
            Some(InstructionScope::Conversation(id))
        } else {
            None
        }
    }
    .ok_or(excalibur_store::Error::Invalid(
        "legacy instruction scope requires an explicit host binding",
    ))?;
    let origin = origin.unwrap_or_else(|| "legacy instruction record".into());
    let origin = serde_json::from_str(&origin).unwrap_or(InstructionOrigin {
        kind: InstructionOriginKind::Legacy,
        reference: origin,
    });
    Ok(InstructionRecord {
        id: id.into(),
        revision,
        literal_text: literal,
        origin,
        state: if state == "active" {
            InstructionState::Active
        } else {
            InstructionState::Retired
        },
        scope,
        delegated,
    })
}
impl Journal {
    /// Freeze the registry selection through the caller's short finalization CAS.
    /// Do not retain this guard during inference or other external work. All host
    /// registry mutation APIs share this barrier across Database/Journal clones.
    pub async fn context_instruction_guard(
        &self,
        run: &str,
    ) -> Result<(InstructionSnapshot, OwnedRwLockReadGuard<()>)> {
        let guard = registry_lock(&self.db)?.read_owned().await;
        let snapshot = self.context_instructions(run).await?;
        Ok((snapshot, guard))
    }
    pub async fn context_instructions(&self, run: &str) -> Result<InstructionSnapshot> {
        ensure_schema(&self.db).await?;
        let run = run.to_owned();
        Ok(self.db.read(ReadLane::Interactive,move|connection|{
            let transaction=connection.unchecked_transaction()?;
            let c=&transaction;
            let key=principal(c,&run)?;
            let bound:Option<String>=c.query_row("SELECT scopes FROM context_instruction_run_bindings WHERE run_id=?1 AND principal_key=?2",rusqlite::params![run,key],|r|r.get(0)).optional()?;
            let mut scopes:BTreeSet<InstructionScope>=if let Some(ref bound)=bound{serde_json::from_str(bound).map_err(|_|excalibur_store::Error::Invalid("invalid run instruction bindings"))?}else{BTreeSet::from([InstructionScope::General])};
            if bound.is_none(){
                let task:Option<String>=c.query_row("SELECT task_id FROM runs WHERE id=?1",[&run],|r|r.get(0))?;
                let task=if task.is_some(){task}else{let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='task_run_links')",[],|r|r.get(0))?;if exists{c.query_row("SELECT task_id FROM task_run_links WHERE run_id=?1",[&run],|r|r.get(0)).optional()?}else{None}};
                if let Some(task)=task{let detail:Option<(Option<String>,Option<String>)>=c.query_row("SELECT project_id,origin_conversation_id FROM tasks WHERE id=?1 AND COALESCE(principal_id,'')=?2",rusqlite::params![task,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;if let Some((project,conversation))=detail{scopes.insert(InstructionScope::Task(task));if let Some(id)=project{scopes.insert(InstructionScope::Project(id));}
if let Some(id)=conversation{scopes.insert(InstructionScope::Conversation(id));}}}
            }
            let mut stmt=c.prepare("SELECT id FROM instructions WHERE state='active' AND COALESCE(principal_id,'')=?1 ORDER BY id")?;let ids=stmt.query_map([&key],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let mut stable=vec![];let mut task=vec![];
            for id in ids{let mut item=record(c,&id,&key,false)?;let delegated:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM context_instruction_delegations WHERE child_run=?1 AND instruction_id=?2)",rusqlite::params![run,id],|r|r.get(0))?;
                if !scopes.contains(&item.scope)&&!delegated{continue;}
item.delegated=delegated;
                if !delegated&&matches!(item.scope,InstructionScope::General|InstructionScope::Project(_)){stable.push(item);}else{task.push(item);}
            }
            stable.sort_by(|a,b|a.scope.cmp(&b.scope).then(a.id.cmp(&b.id)));task.sort_by(|a,b|a.scope.cmp(&b.scope).then(a.id.cmp(&b.id)));
            let revision=digest(serde_json::to_string(&json!({"stable":stable,"task":task})).map_err(|_|excalibur_store::Error::Invalid("instruction snapshot serialization failed"))?.as_bytes());
            Ok(InstructionSnapshot{revision,stable,task})
        }).await?)
    }
}

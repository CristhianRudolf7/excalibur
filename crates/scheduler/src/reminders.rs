//! Host-only reminder domain. Authority is supplied by the authenticated host,
//! never inferred from reminder text, timer payloads, or model arguments.
use super::*;

#[derive(Clone)]
pub struct Reminders {
    scheduler: Scheduler,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reminder {
    pub id: String,
    pub generation: i64,
    pub text: String,
    pub timing: Timing,
    pub preferred_channel: Option<String>,
    pub task: Option<String>,
    pub state: State,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum State {
    Active,
    Completed,
    Deleted,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Definition {
    pub id: String,
    pub text: String,
    pub timing: Timing,
    pub preferred_channel: Option<String>,
    pub task: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Operation {
    Create(Definition),
    Snooze {
        id: String,
        generation: i64,
        timing: Timing,
    },
    Complete {
        id: String,
        generation: i64,
    },
    Delete {
        id: String,
        generation: i64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReminder {
    pub reminder: Reminder,
    pub fire: Fire,
}
fn bounded(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max
}
fn encode<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(json_error)
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(json_error)
}
fn replay<T: serde::de::DeserializeOwned>(
    c: &rusqlite::Connection,
    scope: &str,
    key: &str,
    request: &str,
) -> Result<Option<T>> {
    let row: Option<(String,String)> = c.query_row("SELECT request,response FROM scheduler_reminder_receipts WHERE scope=?1 AND operation_key=?2",rusqlite::params![scope,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    match row {
        Some((old, response)) if old == request => Ok(Some(decode(&response)?)),
        Some(_) => Err(Error::Invalid(
            "reminder operation key reused with different arguments",
        )),
        None => Ok(None),
    }
}
impl Reminders {
    /// All four authority fields must come from host authentication. `source`
    /// distinguishes host transports where required. The resulting scope is
    /// sealed inside this handle and is never accepted in a model request.
    pub async fn open(
        db: Database,
        scope: String,
        principal: String,
        conversation: String,
        source: Option<String>,
    ) -> Result<Self> {
        if [&scope, &principal, &conversation]
            .iter()
            .any(|s| !bounded(s, 512))
            || source.as_ref().is_some_and(|s| !bounded(s, 512))
        {
            return Err(invalid());
        }
        let authority = encode(&(scope, principal, conversation, source))?;
        let sealed = format!("reminder:{:x}", Sha256::digest(authority.as_bytes()));
        Ok(Self {
            scheduler: Scheduler::open(db, sealed).await?,
        })
    }
    /// Exact keyed replay returns the original snapshot, even after later edits.
    /// Calendar work finishes before entering the shared SQLite writer.
    pub async fn apply(&self, key: String, operation: Operation) -> Result<Reminder> {
        self.apply_host(key, operation, None).await
    }
    /// Host-only metadata commits with the winning generation and keyed receipt.
    pub async fn apply_host(
        &self,
        key: String,
        operation: Operation,
        metadata: Option<serde_json::Value>,
    ) -> Result<Reminder> {
        self.apply_host_checked(key, operation, metadata, |_| Ok(()))
            .await
    }
    /// Synchronous host validation runs inside the mutation transaction, including replay.
    pub async fn apply_host_checked(
        &self,
        key: String,
        operation: Operation,
        metadata: Option<serde_json::Value>,
        check: impl FnOnce(&rusqlite::Connection) -> Result<()> + Send + 'static,
    ) -> Result<Reminder> {
        if !bounded(&key, 256) {
            return Err(invalid());
        }
        let request = match &metadata {
            None => encode(&operation)?,
            Some(value) => encode(&(&operation, value))?,
        };
        if request.len() > 65536 {
            return Err(invalid());
        }
        let scope = self.scheduler.scope.clone();
        let timing = match &operation {
            Operation::Create(d) => Some(d.timing.clone()),
            Operation::Snooze { timing, .. } => Some(timing.clone()),
            _ => None,
        };
        let first = if let Some(timing) = timing {
            let at = self
                .scheduler
                .run_calendar(move || first_at(&timing))
                .await?;
            if at < 0 {
                return Err(invalid());
            }
            Some(at)
        } else {
            None
        };
        self.scheduler.db.write(Priority::Journal,move|c|{
            check(c)?;
            if let Some(previous)=replay(c,&scope,&key,&request)? {return Ok(previous);}
            let (id, expected)=match &operation {
                Operation::Create(d)=>(d.id.clone(),None),
                Operation::Snooze{id,generation,..}|Operation::Complete{id,generation}|Operation::Delete{id,generation}=>(id.clone(),Some(*generation)),
            };
            if !bounded(&id,256) {return Err(invalid());}
            let current:Option<String>=c.query_row("SELECT record FROM scheduler_reminders WHERE scope=?1 AND id=?2",rusqlite::params![scope,id],|r|r.get(0)).optional()?;
            let mut reminder=match (expected,current) {
                (None,None)=>match &operation {Operation::Create(d)=> {
                    if !bounded(&d.text,16384) || [&d.preferred_channel,&d.task].iter().any(|s|s.as_ref().is_some_and(|s|!bounded(s,512))) {return Err(invalid());}
                    Reminder{id:d.id.clone(),generation:1,text:d.text.clone(),timing:d.timing.clone(),preferred_channel:d.preferred_channel.clone(),task:d.task.clone(),state:State::Active}
                },_=>unreachable!()},
                (Some(expected),Some(record))=>{
                    let mut r:Reminder=decode(&record)?;
                    if r.generation!=expected || r.state!=State::Active {return Err(Error::Invalid("reminder generation conflict or inactive reminder"));}
                    r.generation=r.generation.checked_add(1).ok_or_else(invalid)?;
                    r
                },
                _=>return Err(Error::Invalid("reminder already exists or does not exist")),
            };
            if expected.is_some() {
                // Disable the cursor before a concurrent tick can commit its plan.
                c.execute("UPDATE scheduler_timers SET next_at=NULL WHERE scope=?1 AND id IN (SELECT timer_id FROM scheduler_reminder_timers WHERE scope=?1 AND reminder_id=?2)",rusqlite::params![scope,id])?;
                c.execute("INSERT OR IGNORE INTO scheduler_cancelled_fires(scope,fire_id,operation_key) SELECT scope,id,?3 FROM scheduler_fires WHERE scope=?1 AND delivered=0 AND timer_id IN (SELECT timer_id FROM scheduler_reminder_timers WHERE scope=?1 AND reminder_id=?2)",rusqlite::params![scope,id,key])?;
            }
            match operation {
                Operation::Snooze{timing,..}=>reminder.timing=timing,
                Operation::Complete{..}=>reminder.state=State::Completed,
                Operation::Delete{..}=>reminder.state=State::Deleted,
                Operation::Create(_)=>{},
            }
            if let Some(first)=first {
                let timer=encode(&("reminder",&id,reminder.generation))?;
                let definition=encode(&Schedule{id:timer.clone(),scope:scope.clone(),timing:reminder.timing.clone(),missed:MissedPolicy::RunOnceNow,payload:serde_json::Value::Null})?;
                c.execute("INSERT INTO scheduler_timers(scope,id,definition,next_at) VALUES(?1,?2,?3,?4)",rusqlite::params![scope,timer,definition,first])?;
                c.execute("INSERT INTO scheduler_reminder_timers(scope,timer_id,reminder_id,generation) VALUES(?1,?2,?3,?4)",rusqlite::params![scope,timer,id,reminder.generation])?;
            }
            if let Some(metadata)=metadata {
                c.execute("INSERT INTO scheduler_reminder_metadata(scope,id,generation,body) VALUES(?1,?2,?3,?4)",rusqlite::params![scope,id,reminder.generation,encode(&metadata)?])?;
            } else if let Some(expected)=expected {
                c.execute("INSERT INTO scheduler_reminder_metadata SELECT scope,id,?4,body FROM scheduler_reminder_metadata WHERE scope=?1 AND id=?2 AND generation=?3",rusqlite::params![scope,id,expected,reminder.generation])?;
            }
            let record=encode(&reminder)?;
            c.execute("INSERT INTO scheduler_reminders(scope,id,generation,record) VALUES(?1,?2,?3,?4) ON CONFLICT(scope,id) DO UPDATE SET generation=excluded.generation,record=excluded.record",rusqlite::params![scope,id,reminder.generation,record])?;
            c.execute("INSERT INTO scheduler_reminder_receipts(scope,operation_key,request,response) VALUES(?1,?2,?3,?4)",rusqlite::params![scope,key,request,record])?;
            Ok(reminder)
        }).await
    }
    /// Keyed, bounded snapshot with keyset pagination; includes tombstones.
    pub async fn list(
        &self,
        key: String,
        after: Option<String>,
        limit: u32,
    ) -> Result<Vec<Reminder>> {
        if !bounded(&key, 256)
            || limit == 0
            || limit > 1000
            || after.as_ref().is_some_and(|s| !bounded(s, 256))
        {
            return Err(invalid());
        }
        let scope = self.scheduler.scope.clone();
        let request = encode(&("list", &after, limit))?;
        self.scheduler.db.write(Priority::Journal,move|c|{
            if let Some(previous)=replay(c,&scope,&key,&request)? {return Ok(previous);}
            let mut q=c.prepare("SELECT record FROM scheduler_reminders WHERE scope=?1 AND (?2 IS NULL OR id>?2) ORDER BY id LIMIT ?3")?;
            let rows=q.query_map(rusqlite::params![scope,after,limit],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let result:Vec<Reminder>=rows.into_iter().map(|s|decode(&s)).collect::<Result<_>>()?;
            c.execute("INSERT INTO scheduler_reminder_receipts(scope,operation_key,request,response) VALUES(?1,?2,?3,?4)",rusqlite::params![scope,key,request,encode(&result)?])?;
            Ok(result)
        }).await
    }
    pub async fn tick(&self, now: i64, recovering: bool, limit: u32) -> Result<usize> {
        self.scheduler.tick(now, recovering, limit).await
    }
    /// This is a snapshot, not authorization for an external delivery. Hosts
    /// must atomically guard this generation with the notice and effect receipts.
    pub async fn pending(&self, limit: u32) -> Result<Vec<PendingReminder>> {
        self.pending_after(None, limit).await
    }
    /// Host keyset cursor lets revoked or unbound rows remain durable without
    /// starving later eligible reminders in an explicit bounded admission tick.
    pub async fn pending_after(
        &self,
        after: Option<(i64, String)>,
        limit: u32,
    ) -> Result<Vec<PendingReminder>> {
        if limit == 0 || limit > 1000 {
            return Err(invalid());
        }
        let scope = self.scheduler.scope.clone();
        self.scheduler.db.read(ReadLane::Background,move|c|{
            let mut q=c.prepare("SELECT r.record,f.id,f.timer_id,f.scheduled_at,f.payload FROM scheduler_fires f JOIN scheduler_reminder_timers t ON t.scope=f.scope AND t.timer_id=f.timer_id JOIN scheduler_reminders r ON r.scope=t.scope AND r.id=t.reminder_id AND r.generation=t.generation WHERE f.scope=?1 AND f.delivered=0 AND json_extract(r.record,'$.state')='Active' AND NOT EXISTS (SELECT 1 FROM scheduler_cancelled_fires x WHERE x.scope=f.scope AND x.fire_id=f.id) AND (?2 IS NULL OR f.scheduled_at>?2 OR (f.scheduled_at=?2 AND f.id>?3)) ORDER BY f.scheduled_at,f.id LIMIT ?4")?;
            let (at,id)=match after {None=>(None,None),Some((at,id))=>(Some(at),Some(id))};
            let rows=q.query_map(rusqlite::params![scope,at,id,limit],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            rows.into_iter().map(|(record,id,schedule,scheduled_at,payload)|Ok(PendingReminder{reminder:decode(&record)?,fire:Fire{id,schedule,scheduled_at,payload:decode(&payload)?}})).collect()
        }).await
    }
}

/// Sealed generation condition obtained only from a scoped reminder handle.
#[derive(Clone, Debug, Serialize)]
pub struct FireCondition {
    scope: String,
    id: String,
    generation: i64,
    fire: String,
}
#[derive(Deserialize)]
struct StoredFireCondition {
    scope: String,
    id: String,
    generation: i64,
    fire: String,
}
/// Read only a durable host condition; the public guard cannot be deserialized
/// from model JSON. Absence is the legacy unguarded notification contract.
pub fn notification_eligible(c: &rusqlite::Connection, notification: &str) -> Result<bool> {
    let exists: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='scheduler_notification_conditions')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(true);
    }
    let body: Option<String> = c
        .query_row(
            "SELECT body FROM scheduler_notification_conditions WHERE notification_id=?1",
            [notification],
            |r| r.get(0),
        )
        .optional()?;
    let Some(body) = body else {
        return Ok(true);
    };
    let stored: StoredFireCondition = decode(&body)?;
    FireCondition {
        scope: stored.scope,
        id: stored.id,
        generation: stored.generation,
        fire: stored.fire,
    }
    .eligible(c)
}
impl FireCondition {
    pub fn eligible(&self, c: &rusqlite::Connection) -> Result<bool> {
        c.query_row("SELECT EXISTS(SELECT 1 FROM scheduler_reminders r JOIN scheduler_reminder_timers t ON t.scope=r.scope AND t.reminder_id=r.id AND t.generation=r.generation JOIN scheduler_fires f ON f.scope=t.scope AND f.timer_id=t.timer_id WHERE r.scope=?1 AND r.id=?2 AND r.generation=?3 AND f.id=?4 AND json_extract(r.record,'$.state')='Active' AND NOT EXISTS(SELECT 1 FROM scheduler_cancelled_fires x WHERE x.scope=f.scope AND x.fire_id=f.id))",rusqlite::params![self.scope,self.id,self.generation,self.fire],|r|r.get(0)).map_err(Error::from)
    }
}
impl Reminders {
    pub fn condition(&self, pending: &PendingReminder) -> FireCondition {
        FireCondition {
            scope: self.scheduler.scope.clone(),
            id: pending.reminder.id.clone(),
            generation: pending.reminder.generation,
            fire: pending.fire.id.clone(),
        }
    }
    pub async fn metadata(&self, pending: &PendingReminder) -> Result<Option<serde_json::Value>> {
        let condition = self.condition(pending);
        self.scheduler.db.read(ReadLane::Background, move |c| {
            let row:Option<String>=c.query_row("SELECT body FROM scheduler_reminder_metadata WHERE scope=?1 AND id=?2 AND generation=?3",rusqlite::params![condition.scope,condition.id,condition.generation],|r|r.get(0)).optional()?;
            row.map(|v|decode(&v)).transpose()
        }).await
    }
    /// Acknowledges scheduler admission only, never channel delivery or user acknowledgement.
    pub async fn acknowledge_queued(&self, fire: String) -> Result<bool> {
        self.scheduler.acknowledge(fire).await
    }
}

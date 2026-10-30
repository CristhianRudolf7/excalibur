//! Durable UTC timers and a scoped delivery outbox. Consumers must honor fire IDs
//! as idempotency keys; acknowledging delivery cannot make an external effect atomic.
use excalibur_store::{Database, DatabaseKind, Error, Priority, ReadLane, Result, rusqlite};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Timing {
    At(i64),
    Every { first: i64, seconds: u32 },
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MissedPolicy {
    RunOnceNow,
    Skip,
    RunAll,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Schedule {
    pub id: String,
    pub scope: String,
    pub timing: Timing,
    pub missed: MissedPolicy,
    pub payload: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fire {
    pub id: String,
    pub schedule: String,
    pub scheduled_at: i64,
    pub payload: serde_json::Value,
}
#[derive(Clone)]
pub struct Scheduler {
    db: Database,
    scope: String,
}
fn invalid() -> Error {
    Error::Invalid("nonempty bounded schedule IDs, scope, payload and positive interval required")
}
fn json_error(_: serde_json::Error) -> Error {
    Error::Invalid("invalid persisted schedule data")
}
impl Scheduler {
    /// The caller obtains this scope from authenticated authority, never event content.
    pub async fn open(db: Database, scope: String) -> Result<Self> {
        if scope.is_empty() || scope.len() > 512 {
            return Err(invalid());
        }
        if db.kind() != DatabaseKind::Main {
            return Err(Error::PrincipalOnly);
        }
        let schema = include_str!("schema.sql");
        let checksum = format!("{:x}", Sha256::digest(schema.as_bytes()));
        db.write(Priority::Journal, move |c| {
            c.execute_batch("CREATE TABLE IF NOT EXISTS scheduler_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL)")?;
            let known: Option<String> = c.query_row("SELECT checksum FROM scheduler_schema WHERE version=1", [], |r| r.get(0)).optional()?;
            if known.as_ref().is_some_and(|stored| stored != &checksum) {
                return Err(Error::Invalid("scheduler schema checksum mismatch"));
            }
            c.execute_batch(schema)?;
            c.execute("INSERT OR IGNORE INTO scheduler_schema(version,checksum) VALUES(1,?1)", [checksum])?;
            Ok(())
        }).await?;
        Ok(Self { db, scope })
    }
    /// Creating an existing ID is idempotent only for the exact same definition.
    /// Replacing a timer needs a new ID so pending deliveries retain their meaning.
    pub async fn create(&self, schedule: Schedule) -> Result<()> {
        if schedule.scope != self.scope || schedule.id.is_empty() || schedule.id.len() > 256 {
            return Err(invalid());
        }
        let first = match schedule.timing {
            Timing::At(at) => at,
            Timing::Every { first, seconds } if seconds > 0 => first,
            Timing::Every { .. } => return Err(invalid()),
        };
        if first < 0 {
            return Err(invalid());
        }
        let definition = serde_json::to_string(&schedule).map_err(json_error)?;
        if definition.len() > 65536 {
            return Err(invalid());
        }
        self.db.write(Priority::Journal, move |c| {
            c.execute("INSERT OR IGNORE INTO scheduler_timers(scope,id,definition,next_at) VALUES(?1,?2,?3,?4)", rusqlite::params![schedule.scope,schedule.id,definition,first])?;
            let stored: String = c.query_row("SELECT definition FROM scheduler_timers WHERE scope=?1 AND id=?2",rusqlite::params![schedule.scope,schedule.id],|r|r.get(0))?;
            if stored != definition { return Err(Error::Invalid("schedule ID already has another definition")); }
            Ok(())
        }).await
    }
    /// Atomically advances timers and records pending fires. No external effects.
    /// `recovering` applies missed policies; ordinary ticks deliver every due fire.
    /// Bounded pages leave run_all backlog durable for the next tick.
    pub async fn tick(&self, now: i64, recovering: bool, limit: u32) -> Result<usize> {
        if now < 0 || limit == 0 || limit > 1000 {
            return Err(Error::Invalid(
                "nonnegative time and tick limit 1..1000 required",
            ));
        }
        let scope = self.scope.clone();
        self.db.write(Priority::Journal, move |c| {
            let mut query = c.prepare("SELECT id,definition,next_at FROM scheduler_timers WHERE scope=?1 AND next_at<=?2 ORDER BY next_at,id LIMIT ?3")?;
            let timers = query.query_map(rusqlite::params![scope,now,limit],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            drop(query);
            let mut count = 0;
            for (id, definition, mut due) in timers {
                let schedule: Schedule = serde_json::from_str(&definition).map_err(json_error)?;
                let interval = match schedule.timing { Timing::At(_) => None, Timing::Every { seconds, .. } if seconds>0 => Some(i64::from(seconds)), _=>return Err(invalid()) };
                // Skip only genuinely missed times: a fire exactly at startup is due.
                let skip = recovering && schedule.missed == MissedPolicy::Skip && due < now
                    && interval.is_none_or(|seconds| (now - due) % seconds != 0);
                let coalesce = recovering && schedule.missed != MissedPolicy::RunAll;
                if coalesce && let Some(seconds) = interval {
                        let latest = due + ((now - due) / seconds) * seconds;
                        if !skip { due = latest; }
                }
                let payload = serde_json::to_string(&schedule.payload).map_err(json_error)?;
                let mut next;
                loop {
                    if !skip {
                        // JSON encoding is unambiguous even when IDs contain separators.
                        let fire_id = serde_json::to_string(&(scope.as_str(),id.as_str(),due)).map_err(json_error)?;
                        count += c.execute("INSERT OR IGNORE INTO scheduler_fires(scope,id,timer_id,scheduled_at,payload) VALUES(?1,?2,?3,?4,?5)",rusqlite::params![scope,fire_id,id,due,payload])?;
                    }
                    next = interval.and_then(|seconds| due.checked_add(seconds));
                    if skip {
                        next = interval.and_then(|seconds| (due + ((now-due)/seconds)*seconds).checked_add(seconds));
                    }
                    if coalesce || count >= limit as usize || next.is_none_or(|at| at > now) { break; }
                    due = next.unwrap();
                }
                c.execute("UPDATE scheduler_timers SET next_at=?3 WHERE scope=?1 AND id=?2",rusqlite::params![scope,id,next])?;
                if count >= limit as usize { break; }
            }
            Ok(count)
        }).await
    }
    pub async fn pending(&self, limit: u32) -> Result<Vec<Fire>> {
        if limit == 0 || limit > 1000 {
            return Err(Error::Invalid("delivery limit 1..1000 required"));
        }
        let scope = self.scope.clone();
        self.db.read(ReadLane::Background, move |c| {
            let mut q=c.prepare("SELECT id,timer_id,scheduled_at,payload FROM scheduler_fires WHERE scope=?1 AND delivered=0 ORDER BY scheduled_at,id LIMIT ?2")?;
            let rows=q.query_map(rusqlite::params![scope,limit],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            rows.into_iter().map(|(id,schedule,scheduled_at,payload)|Ok(Fire{id,schedule,scheduled_at,payload:serde_json::from_str(&payload).map_err(json_error)?})).collect()
        }).await
    }
    pub async fn acknowledge(&self, fire_id: String) -> Result<bool> {
        let scope = self.scope.clone();
        self.db.write(Priority::Journal,move|c|Ok(c.execute("UPDATE scheduler_fires SET delivered=1 WHERE scope=?1 AND id=?2 AND delivered=0",rusqlite::params![scope,fire_id])?==1)).await
    }
}

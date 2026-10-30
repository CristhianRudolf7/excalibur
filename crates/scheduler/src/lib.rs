//! Durable UTC timers and a scoped delivery outbox. Consumers must honor fire IDs
//! as idempotency keys; acknowledging delivery cannot make an external effect atomic.
pub mod calendar;

use excalibur_store::{Database, DatabaseKind, Error, Priority, ReadLane, Result, rusqlite};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Timing {
    At(i64),
    Every {
        first: i64,
        seconds: u32,
    },
    LocalAt {
        local: String,
        timezone: String,
    },
    Cron {
        expression: String,
        timezone: String,
        not_before: i64,
    },
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
        db.write(Priority::Journal, move |c| {
            c.execute_batch("CREATE TABLE IF NOT EXISTS scheduler_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL)")?;
            for (version,schema) in [(1,include_str!("schema.sql")),(2,include_str!("schema2.sql"))] {
                let checksum=format!("{:x}",Sha256::digest(schema.as_bytes()));
                let known:Option<String>=c.query_row("SELECT checksum FROM scheduler_schema WHERE version=?1",[version],|r|r.get(0)).optional()?;
                if known.as_ref().is_some_and(|stored|stored!=&checksum){return Err(Error::Invalid("scheduler schema checksum mismatch"));}
                c.execute_batch(schema)?;
                c.execute("INSERT OR IGNORE INTO scheduler_schema(version,checksum) VALUES(?1,?2)",rusqlite::params![version,checksum])?;
            }
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
        let first = first_at(&schedule.timing)?;
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
        let rows = self.db.read(ReadLane::Background, move |c| {
            let mut q = c.prepare("SELECT id,definition,next_at FROM scheduler_timers WHERE scope=?1 AND next_at<=?2 ORDER BY next_at,id LIMIT ?3")?;
            Ok(q.query_map(rusqlite::params![scope,now,limit],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)
        }).await?;
        // Calendar search does not retain the writer transaction or a read lease.
        let mut plans = Vec::new();
        let mut remaining = limit as usize;
        for (id, definition, due) in rows {
            let schedule: Schedule = serde_json::from_str(&definition).map_err(json_error)?;
            let plan = plan_fires(
                &schedule.timing,
                schedule.missed,
                due,
                now,
                recovering,
                remaining,
            )?;
            remaining -= plan.0.len();
            plans.push((id, definition, due, schedule.payload, plan));
            if remaining == 0 {
                break;
            }
        }
        let scope = self.scope.clone();
        self.db.write(Priority::Journal, move |c| {
            let mut inserted = 0;
            for (id, definition, due, payload, (fires, next)) in plans {
                let current: Option<(String,Option<i64>)> = c.query_row("SELECT definition,next_at FROM scheduler_timers WHERE scope=?1 AND id=?2",rusqlite::params![scope,id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                if current != Some((definition,Some(due))) { continue; }
                let payload = serde_json::to_string(&payload).map_err(json_error)?;
                for at in fires {
                    let key = serde_json::to_string(&(scope.as_str(),id.as_str(),at)).map_err(json_error)?;
                    inserted += c.execute("INSERT OR IGNORE INTO scheduler_fires(scope,id,timer_id,scheduled_at,payload) VALUES(?1,?2,?3,?4,?5)",rusqlite::params![scope,key,id,at,payload])?;
                }
                c.execute("UPDATE scheduler_timers SET next_at=?3 WHERE scope=?1 AND id=?2",rusqlite::params![scope,id,next])?;
            }
            Ok(inserted)
        }).await
    }
    /// Recalculate an active civil timer and atomically persist a user-visible
    /// change notice. Completed one-shot timers are never resurrected. Retrying
    /// one operation key returns its original notice, even after the clock moves.
    pub async fn change_timezone(
        &self,
        key: String,
        id: String,
        timezone: String,
        now: i64,
    ) -> Result<TimezoneChange> {
        if key.is_empty()
            || key.len() > 256
            || id.is_empty()
            || id.len() > 256
            || timezone.len() > 256
            || now < 0
        {
            return Err(invalid());
        }
        calendar::validate_timezone(&timezone)?;
        let (scope, lookup_key) = (self.scope.clone(), key.clone());
        let previous = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT note FROM scheduler_timezone_changes WHERE scope=?1 AND change_key=?2",
                    rusqlite::params![scope, lookup_key],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
            })
            .await?;
        if let Some(note) = previous {
            return replay_change(&note, &id, &timezone);
        }
        let (scope, lookup_id) = (self.scope.clone(), id.clone());
        let (definition, old_next) = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT definition,next_at FROM scheduler_timers WHERE scope=?1 AND id=?2",
                    rusqlite::params![scope, lookup_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
                )?)
            })
            .await?;
        let old_next = old_next.ok_or(Error::Invalid(
            "only active civil timers can change time zone",
        ))?;
        if old_next <= now {
            return Err(Error::Invalid("record due fires before changing time zone"));
        }
        let mut schedule: Schedule = serde_json::from_str(&definition).map_err(json_error)?;
        let (old_timezone, next) = match &mut schedule.timing {
            Timing::LocalAt {
                local,
                timezone: old,
            } => {
                let previous = std::mem::replace(old, timezone.clone());
                (previous, calendar::local_to_utc(local, &timezone)?)
            }
            Timing::Cron {
                expression,
                timezone: old,
                not_before,
            } => {
                let previous = std::mem::replace(old, timezone.clone());
                (
                    previous,
                    calendar::Cron::parse(expression)?.next_after(
                        now.max(not_before.checked_sub(1).ok_or_else(invalid)?),
                        &timezone,
                    )?,
                )
            }
            _ => {
                return Err(Error::Invalid(
                    "UTC timers do not change with user time zone",
                ));
            }
        };
        if next < 0 {
            return Err(invalid());
        }
        let updated = serde_json::to_string(&schedule).map_err(json_error)?;
        let note = TimezoneChange {
            key,
            schedule: id,
            old_timezone,
            timezone,
            previous_next: old_next,
            next,
            changed_at: now,
        };
        let scope = self.scope.clone();
        self.db.write(Priority::Journal,move|c|{
            let existing:Option<String>=c.query_row("SELECT note FROM scheduler_timezone_changes WHERE scope=?1 AND change_key=?2",rusqlite::params![scope,note.key],|r|r.get(0)).optional()?;
            if let Some(existing)=existing{return replay_change(&existing,&note.schedule,&note.timezone);}
            let current:(String,Option<i64>)=c.query_row("SELECT definition,next_at FROM scheduler_timers WHERE scope=?1 AND id=?2",rusqlite::params![scope,note.schedule],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if current!=(definition.clone(),Some(old_next)){return Err(Error::Invalid("timer changed during zone recalculation; retry with fresh state"));}
            c.execute("UPDATE scheduler_timers SET definition=?3,next_at=?4 WHERE scope=?1 AND id=?2",rusqlite::params![scope,note.schedule,updated,next])?;
            c.execute("INSERT INTO scheduler_timezone_changes(scope,change_key,note,old_definition,new_definition) VALUES(?1,?2,?3,?4,?5)",rusqlite::params![scope,note.key,serde_json::to_string(&note).map_err(json_error)?,definition,updated])?;
            Ok(note)
        }).await
    }
    pub async fn timezone_changes(&self, limit: u32) -> Result<Vec<TimezoneChange>> {
        if limit == 0 || limit > 1000 {
            return Err(invalid());
        }
        let scope = self.scope.clone();
        self.db.read(ReadLane::Background,move|c|{
            let mut q=c.prepare("SELECT note FROM scheduler_timezone_changes WHERE scope=?1 AND delivered=0 ORDER BY change_key LIMIT ?2")?;
            let notes=q.query_map(rusqlite::params![scope,limit],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
            notes.into_iter().map(|s|serde_json::from_str(&s).map_err(json_error)).collect()
        }).await
    }
    pub async fn acknowledge_timezone_change(&self, key: String) -> Result<bool> {
        let scope = self.scope.clone();
        self.db.write(Priority::Journal,move|c|Ok(c.execute("UPDATE scheduler_timezone_changes SET delivered=1 WHERE scope=?1 AND change_key=?2 AND delivered=0",rusqlite::params![scope,key])?==1)).await
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

fn first_at(timing: &Timing) -> Result<i64> {
    match timing {
        Timing::At(at) => Ok(*at),
        Timing::Every { first, seconds } if *seconds > 0 => Ok(*first),
        Timing::Every { .. } => Err(invalid()),
        Timing::LocalAt { local, timezone } => calendar::local_to_utc(local, timezone),
        Timing::Cron {
            expression,
            timezone,
            not_before,
        } => {
            if *not_before < 0 {
                return Err(invalid());
            }
            calendar::Cron::parse(expression)?
                .next_after(not_before.checked_sub(1).ok_or_else(invalid)?, timezone)
        }
    }
}
fn next_at(timing: &Timing, after: i64) -> Result<Option<i64>> {
    match timing {
        Timing::At(_) | Timing::LocalAt { .. } => Ok(None),
        Timing::Every { seconds, .. } if *seconds > 0 => Ok(after.checked_add(i64::from(*seconds))),
        Timing::Every { .. } => Err(invalid()),
        Timing::Cron {
            expression,
            timezone,
            ..
        } => calendar::Cron::parse(expression)?
            .next_after(after, timezone)
            .map(Some),
    }
}
fn latest_due(timing: &Timing, first: i64, now: i64) -> Result<i64> {
    match timing {
        Timing::At(_) | Timing::LocalAt { .. } => Ok(first),
        Timing::Every { seconds, .. } if *seconds > 0 => {
            Ok(first + ((now - first) / i64::from(*seconds)) * i64::from(*seconds))
        }
        Timing::Every { .. } => Err(invalid()),
        Timing::Cron {
            expression,
            timezone,
            ..
        } => calendar::Cron::parse(expression)?.latest_at(now, timezone),
    }
}
fn plan_fires(
    timing: &Timing,
    policy: MissedPolicy,
    first: i64,
    now: i64,
    recovery: bool,
    limit: usize,
) -> Result<(Vec<i64>, Option<i64>)> {
    let mut due = first;
    if recovery && policy != MissedPolicy::RunAll {
        due = latest_due(timing, first, now)?;
        let fires = if policy == MissedPolicy::Skip && due < now {
            Vec::new()
        } else {
            vec![due]
        };
        return Ok((fires, next_at(timing, due)?));
    }
    let mut fires = Vec::new();
    loop {
        fires.push(due);
        let next = next_at(timing, due)?;
        if fires.len() >= limit || next.is_none_or(|at| at > now) {
            return Ok((fires, next));
        }
        due = next.ok_or_else(invalid)?;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimezoneChange {
    pub key: String,
    pub schedule: String,
    pub old_timezone: String,
    pub timezone: String,
    pub previous_next: i64,
    pub next: i64,
    pub changed_at: i64,
}
fn replay_change(note: &str, id: &str, zone: &str) -> Result<TimezoneChange> {
    let note: TimezoneChange = serde_json::from_str(note).map_err(json_error)?;
    if note.schedule != id || note.timezone != zone {
        return Err(Error::Invalid(
            "time zone operation key reused with different arguments",
        ));
    }
    Ok(note)
}

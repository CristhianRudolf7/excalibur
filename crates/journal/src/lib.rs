//! Confirmed append-only execution events on the principal SQLite writer.
use excalibur_store::{
    Database, Error as StoreError, Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use excalibur_types::{JobId, RunId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock, Weak},
};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("journal storage: {0}")]
    Storage(#[from] StoreError),
    #[error("journal serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("execution does not exist")]
    Missing,
    #[error("invalid journal argument: {0}")]
    Invalid(&'static str),
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub sequence: u64,
    pub kind: String,
    pub key: String,
    pub payload: Value,
}
#[derive(Clone)]
pub struct Journal {
    db: Database,
}
type RunLocks = Mutex<BTreeMap<String, Weak<tokio::sync::Mutex<()>>>>;
static LOCKS: OnceLock<RunLocks> = OnceLock::new();
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
impl Journal {
    pub async fn new(db: Database) -> Result<Self> {
        // Own transactional, checksummed extension of the foundational storage schema.
        let sql = include_str!("schema.sql");
        let checksum = digest(sql.as_bytes());
        db.write(Priority::Journal, move |c| {
            c.execute_batch(sql)?;
            let known: Option<String> = c
                .query_row(
                    "SELECT checksum FROM durable_schema WHERE version=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            match known {
                Some(value) if value != checksum => {
                    return Err(StoreError::Configuration(
                        "durable schema checksum mismatch".into(),
                    ));
                }
                Some(_) => {}
                None => {
                    c.execute("INSERT INTO durable_schema VALUES(1,?1)", [checksum])?;
                }
            }
            Ok(())
        })
        .await?;
        Ok(Self { db })
    }
    pub fn database(&self) -> &Database {
        &self.db
    }
    /// Shared across engine instances; the Database already enforces per-process file ownership.
    pub async fn lock_run(&self, run: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let key = format!("{}:{run}", self.db.path().display());
        let lock = {
            let mut locks = LOCKS.get_or_init(Default::default).lock().unwrap();
            locks.retain(|_, v| v.strong_count() > 0);
            if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
    /// An operation key creates at most one run; a different seed with the same key is rejected.
    pub async fn create_run(&self, operation: &str, seed: Value) -> Result<String> {
        if operation.trim().is_empty() || operation.len() > 512 {
            return Err(Error::Invalid("operation key required"));
        }
        let text = serde_json::to_string(&seed)?;
        if text.len() > 8 * 1024 * 1024 {
            return Err(Error::Invalid("run seed exceeds 8 MiB"));
        }
        let seed_hash = digest(text.as_bytes());
        let operation = operation.to_owned();
        let run = RunId::new().to_string();
        Ok(self.db.write(Priority::Journal,move|c|{
            let known:Option<(String,String)>=c.query_row("SELECT run_id,seed_hash FROM durable_operations WHERE operation_id=?1",[&operation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((run,hash))=known { if hash!=seed_hash{return Err(StoreError::Invalid("operation key reused with different run seed"));}return Ok(run); }
            c.execute("INSERT INTO runs(id,state) VALUES(?1,'running')",[&run])?;
            c.execute("INSERT INTO durable_operations VALUES(?1,?2,?3)",rusqlite::params![operation,run,seed_hash])?;
            c.execute("INSERT INTO journal(id,run_id,sequence,kind,payload,state,idempotency_key) VALUES(?1,?2,1,'run_started',?3,'confirmed','run_started')",rusqlite::params![JobId::new().to_string(),run,text])?;
            Ok(run)
        }).await?)
    }
    /// A confirmed event key is immutable. Retransmission must carry exactly the same payload.
    pub async fn append(&self, run: &str, kind: &str, key: &str, payload: Value) -> Result<u64> {
        if kind.is_empty() || key.is_empty() || kind.len() > 80 || key.len() > 1024 {
            return Err(Error::Invalid("event kind/key invalid"));
        }
        let text = serde_json::to_string(&payload)?;
        if text.len() > 16 * 1024 * 1024 {
            return Err(Error::Invalid("event exceeds 16 MiB"));
        }
        let run = run.to_owned();
        let kind = kind.to_owned();
        let key = key.to_owned();
        if text.len() > 64 * 1024 {
            let blobs = self.db.blobs();
            let bytes = text.clone().into_bytes();
            let prepared = tokio::task::spawn_blocking(move || blobs.prepare(&bytes))
                .await
                .map_err(|_| Error::Invalid("blob preparation panicked"))??;
            Ok(self
                .db
                .write_with_blob(prepared, Priority::Journal, move |c, hash| {
                    append_row(c, &run, &kind, &key, None, Some(hash.as_str()))
                })
                .await?)
        } else {
            Ok(self
                .db
                .write(Priority::Journal, move |c| {
                    append_row(c, &run, &kind, &key, Some(&text), None)
                })
                .await?)
        }
    }
    pub async fn set_state(
        &self,
        run: &str,
        state: &str,
        reason: Option<String>,
        key: &str,
    ) -> Result<()> {
        if state.len() > 80 || reason.as_ref().is_some_and(|s| s.len() > 65536) {
            return Err(Error::Invalid("state/reason too large"));
        }
        let run = run.to_string();
        let state = state.to_string();
        let key = key.to_string();
        let text = serde_json::to_string(&serde_json::json!({"state":state,"reason":reason}))?;
        self.db
            .write(Priority::Journal, move |c| {
                append_row(c, &run, "state_change", &key, Some(&text), None)?;
                c.execute(
                    "UPDATE runs SET state=?2,reason=?3,updated_at=unixepoch() WHERE id=?1",
                    rusqlite::params![run, state, reason],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub async fn entries(&self, run: &str, after: u64, limit: u32) -> Result<Vec<Event>> {
        if !(1..=1000).contains(&limit) {
            return Err(Error::Invalid("journal page limit"));
        }
        let run = run.to_string();
        let rows=self.db.read(ReadLane::Interactive,move|c|{
            let mut statement=c.prepare("SELECT sequence,kind,idempotency_key,payload,payload_blob FROM journal WHERE run_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")?;
            Ok(statement.query_map(rusqlite::params![run,after,limit],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)
        }).await?;
        let mut out = Vec::new();
        for (sequence, kind, key, inline, blob) in rows {
            let bytes = if let Some(text) = inline {
                text.into_bytes()
            } else if let Some(hash) = blob {
                self.db.read_blob(hash.parse()?, 16 * 1024 * 1024).await?
            } else {
                return Err(Error::Invalid("event payload missing"));
            };
            out.push(Event {
                sequence,
                kind,
                key,
                payload: serde_json::from_slice(&bytes)?,
            });
        }
        Ok(out)
    }
    pub async fn latest_checkpoint(&self, run: &str) -> Result<Option<u64>> {
        let run = run.to_string();
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT max(sequence) FROM journal WHERE run_id=?1 AND kind='checkpoint'",
                    [run],
                    |r| r.get(0),
                )?)
            })
            .await?)
    }
    pub async fn replay(&self, run: &str, after: u64) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        let mut cursor = after;
        loop {
            let page = self.entries(run, cursor, 1000).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().unwrap().sequence;
            events.extend(page);
        }
        Ok(events)
    }
}
fn append_row(
    c: &rusqlite::Connection,
    run: &str,
    kind: &str,
    key: &str,
    text: Option<&str>,
    blob: Option<&str>,
) -> excalibur_store::Result<u64> {
    let known:Option<(u64,String,Option<String>,Option<String>)>=c.query_row("SELECT sequence,kind,payload,payload_blob FROM journal WHERE run_id=?1 AND idempotency_key=?2",rusqlite::params![run,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    if let Some((seq, k, t, b)) = known {
        if k != kind || t.as_deref() != text || b.as_deref() != blob {
            return Err(StoreError::Invalid("journal event key conflict"));
        }
        return Ok(seq);
    }
    let seq: u64 = c.query_row(
        "SELECT coalesce(max(sequence),0)+1 FROM journal WHERE run_id=?1",
        [run],
        |r| r.get(0),
    )?;
    c.execute("INSERT INTO journal(id,run_id,sequence,kind,payload,payload_blob,state,idempotency_key) VALUES(?1,?2,?3,?4,?5,?6,'confirmed',?7)",rusqlite::params![JobId::new().to_string(),run,seq,kind,text,blob,key])?;
    Ok(seq)
}

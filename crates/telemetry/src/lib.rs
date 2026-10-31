//! Durable, redacted backend telemetry and pinned interoperable trial artifacts.
pub mod atif;
mod auxiliary;
mod config;
mod context_projection;
mod event;
mod export;
mod metrics;
pub mod otel;
pub mod resources;
pub use config::{RunMetadata, TelemetryConfig};
pub use event::{EventEnvelope, EventKind, FileEditRecord};
use excalibur_journal::{Event, Journal};
use excalibur_store::{
    Database, DatabaseKind, Priority, ReadLane, StorageConfig,
    rusqlite::{self, OptionalExtension},
};
use excalibur_vault::Redactor;
pub use export::{ExportManifest, JsonlSink, decode_jsonl_prefix};
pub use metrics::{ModelTotals, TaskMetrics, TokenTotals};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("telemetry storage failed: {0}")]
    Store(#[from] excalibur_store::Error),
    #[error("telemetry journal failed: {0}")]
    Journal(#[from] excalibur_journal::Error),
    #[error("telemetry serialization failed")]
    Json(#[from] serde_json::Error),
    #[error("telemetry file operation failed")]
    Io(#[from] std::io::Error),
    #[error("invalid telemetry: {0}")]
    Invalid(&'static str),
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<Inner>,
}
struct Inner {
    directory: PathBuf,
    artifact_root: PathBuf,
    db: Database,
    metadata: RunMetadata,
    redactor: Redactor,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    resources: resources::ResourceConfig,
    export_otel: bool,
    export_serial: tokio::sync::Mutex<()>,
    notify: Arc<tokio::sync::Notify>,
}
impl std::fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telemetry")
            .field("directory", &self.inner.directory)
            .finish_non_exhaustive()
    }
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn sanitize(redactor: &Redactor, value: &Value) -> Value {
    fn visit(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if matches!(
                        key.to_ascii_lowercase().as_str(),
                        "api_key"
                            | "apikey"
                            | "access_token"
                            | "refresh_token"
                            | "authorization"
                            | "password"
                            | "private_key"
                            | "client_secret"
                    ) && !value.is_null()
                    {
                        *value = json!("[REDACTED]");
                    } else {
                        visit(value);
                    }
                }
            }
            Value::Array(values) => values.iter_mut().for_each(visit),
            _ => {}
        }
    }
    let mut value = redactor.redact_json(value);
    visit(&mut value);
    value
}
fn journal_source_hash(event: &Event) -> Result<String> {
    Ok(if event.kind == "context_prepared" {
        context_projection::payload_hash(&event.payload)?
    } else {
        digest(&serde_json::to_vec(&event.payload)?)
    })
}
fn offset_hash(kind: &str, key: &str, source_hash: &str) -> String {
    digest(format!("{kind}\0{key}\0{source_hash}").as_bytes())
}
/// Identity hash stored in `telemetry_offsets.payload_hash` for a journal-sourced event.
/// A purge that redacts a journal payload re-seals the offset with this value so that a
/// later `reconcile` of the redacted journal stays idempotent instead of conflicting.
pub fn journal_offset_hash(event: &Event) -> Result<String> {
    Ok(offset_hash(
        &event.kind,
        &event.key,
        &journal_source_hash(event)?,
    ))
}
impl Telemetry {
    pub async fn open(config: TelemetryConfig) -> Result<Self> {
        let mut directories = std::fs::DirBuilder::new();
        directories.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directories.mode(0o700);
        }
        directories.create(&config.directory)?;
        let directory = config.directory.canonicalize()?;
        let db = Database::open(
            directory.join("telemetry.db"),
            DatabaseKind::Telemetry,
            StorageConfig::default(),
        )?;
        let sql = include_str!("schema.sql");
        let checksum = digest(sql.as_bytes());
        db.write(Priority::Journal, move |c| {
            c.execute_batch(sql)?;
            let old: Option<String> = c
                .query_row(
                    "SELECT checksum FROM telemetry_schema WHERE version=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if old.as_ref().is_some_and(|old| old != &checksum) {
                return Err(excalibur_store::Error::Invalid(
                    "telemetry extension checksum mismatch",
                ));
            }
            c.execute(
                "INSERT OR IGNORE INTO telemetry_schema VALUES(1,?1)",
                [checksum],
            )?;
            let expired = include_str!("schema2.sql");
            let expired_checksum = digest(expired.as_bytes());
            let old: Option<String> = c
                .query_row(
                    "SELECT checksum FROM telemetry_schema WHERE version=2",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            match old {
                Some(old) if old != expired_checksum => {
                    return Err(excalibur_store::Error::Invalid(
                        "telemetry extension checksum mismatch",
                    ));
                }
                Some(_) => {}
                None => {
                    c.execute_batch(expired)?;
                    c.execute(
                        "INSERT INTO telemetry_schema VALUES(2,?1)",
                        [expired_checksum],
                    )?;
                }
            }
            Ok(())
        })
        .await?;
        Ok(Self {
            inner: Arc::new(Inner {
                artifact_root: config.artifact_root.unwrap_or_else(|| directory.clone()),
                directory,
                db,
                metadata: config.metadata,
                redactor: config.redactor,
                clock: config.clock,
                resources: config.resources,
                export_otel: config.export_otel,
                export_serial: tokio::sync::Mutex::new(()),
                notify: Arc::new(tokio::sync::Notify::new()),
            }),
        })
    }
    pub fn database(&self) -> &Database {
        &self.inner.db
    }
    pub fn directory(&self) -> &Path {
        &self.inner.directory
    }
    pub async fn register_run(&self, run: &str, metadata: RunMetadata) -> Result<()> {
        self.identifier(run)?;
        let mut value = sanitize(&self.inner.redactor, &serde_json::to_value(&metadata)?);
        let hash = digest(&serde_json::to_vec(&value["configuration"])?);
        value["configuration_hash"] = json!(hash);
        let run = run.to_owned();
        let text = serde_json::to_string(&value)?;
        self.inner
            .db
            .write(Priority::Journal, move |c| {
                let old: Option<String> = c
                    .query_row(
                        "SELECT metadata FROM telemetry_runs WHERE run_id=?1",
                        [&run],
                        |r| r.get(0),
                    )
                    .optional()?;
                if old.as_ref().is_some_and(|old| old != &text) {
                    return Err(excalibur_store::Error::Invalid(
                        "telemetry run metadata is immutable",
                    ));
                }
                c.execute(
                    "INSERT OR IGNORE INTO telemetry_runs(run_id,metadata) VALUES(?1,?2)",
                    rusqlite::params![run, text],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    async fn ensure_run(&self, run: &str) -> Result<()> {
        self.identifier(run)?;
        let id = run.to_owned();
        let exists = self
            .inner
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM telemetry_runs WHERE run_id=?1)",
                    [id],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await?;
        if !exists {
            self.register_run(run, self.inner.metadata.clone()).await?;
        }
        let run = run.to_owned();
        self.inner
            .db
            .write(Priority::Normal, move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO telemetry_settings(name,value) VALUES('primary_run',?1)",
                    [run],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    fn identifier(&self, run: &str) -> Result<()> {
        if matches!(run, "." | "..")
            || run.is_empty()
            || run.len() > 256
            || !run
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.'))
            || self.inner.redactor.contains_secret(run.as_bytes())
        {
            return Err(Error::Invalid("run identifier is invalid or sensitive"));
        }
        Ok(())
    }
    pub async fn bind_primary_run(&self, run: &str) -> Result<()> {
        self.ensure_run(run).await?;
        let run_copy = run.to_owned();
        self.inner
            .db
            .write(Priority::Journal, move |c| {
                let old: Option<String> = c
                    .query_row(
                        "SELECT value FROM telemetry_settings WHERE name='primary_run'",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                if old.as_ref().is_some_and(|old| old != &run_copy) {
                    return Err(excalibur_store::Error::Invalid(
                        "primary telemetry run is already bound",
                    ));
                }
                c.execute(
                    "INSERT OR IGNORE INTO telemetry_settings(name,value) VALUES('primary_run',?1)",
                    [run_copy],
                )?;
                Ok(())
            })
            .await?;
        self.export_run(run).await?;
        Ok(())
    }
    pub async fn record_event(&self, run: &str, event: &Event) -> Result<EventEnvelope> {
        self.record_at(run, event, (self.inner.clock)(), false)
            .await
    }
    pub async fn record_recovered_event(&self, run: &str, event: &Event) -> Result<EventEnvelope> {
        self.record_at(run, event, (self.inner.clock)(), true).await
    }
    async fn record_at(
        &self,
        run: &str,
        event: &Event,
        stamp: i64,
        recovered: bool,
    ) -> Result<EventEnvelope> {
        self.ensure_run(run).await?;
        if event.sequence == 0
            || event.kind.is_empty()
            || event.kind.len() > 80
            || event.key.len() > 1024
        {
            return Err(Error::Invalid("invalid durable event identity"));
        }
        if self.inner.redactor.contains_secret(event.kind.as_bytes())
            || self.inner.redactor.contains_secret(event.key.as_bytes())
        {
            return Err(Error::Invalid(
                "durable event identity contains sensitive data",
            ));
        }
        // Only the principal journal owns recoverable ContextSession snapshots.
        // Keep source identity over the ORIGINAL payload, including omitted data.
        let source_hash = journal_source_hash(event)?;
        let payload = if event.kind == "context_prepared" {
            sanitize(
                &self.inner.redactor,
                &context_projection::receipt(run, event, &source_hash),
            )
        } else {
            sanitize(&self.inner.redactor, &event.payload)
        };
        let envelope = self
            .insert(
                run,
                "journal",
                event.sequence,
                &event.kind,
                &event.key,
                payload,
                stamp,
                recovered,
                Some(source_hash),
            )
            .await?;
        self.export_run(run).await?;
        Ok(envelope)
    }
    #[allow(clippy::too_many_arguments)]
    async fn insert(
        &self,
        run: &str,
        source: &str,
        sequence: u64,
        kind: &str,
        key: &str,
        payload: Value,
        stamp: i64,
        recovered: bool,
        source_hash: Option<String>,
    ) -> Result<EventEnvelope> {
        let (run, source, kind, key) = (
            run.to_owned(),
            source.to_owned(),
            kind.to_owned(),
            key.to_owned(),
        );
        let text = serde_json::to_string(&payload)?;
        let hash = offset_hash(
            &kind,
            &key,
            &source_hash.unwrap_or_else(|| digest(text.as_bytes())),
        );
        let cursor=self.inner.db.write(Priority::Journal,move|c| {
            let sequence=if sequence==0 {c.query_row("SELECT COALESCE(MAX(sequence),0)+1 FROM telemetry_offsets WHERE run_id=?1 AND source=?2",rusqlite::params![run,source],|r|r.get::<_,u64>(0))?}else{sequence};
            let key=if matches!(source.as_str(),"resource"|"external") {format!("{source}:{sequence}")}else{key};
            let id=digest(format!("{run}\0{source}\0{sequence}").as_bytes());
            let old:Option<(u64,String)>=c.query_row("SELECT cursor,payload_hash FROM telemetry_offsets WHERE run_id=?1 AND source=?2 AND sequence=?3",rusqlite::params![run,source,sequence],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((cursor,old))=old {if old!=hash{return Err(excalibur_store::Error::Invalid("durable telemetry event identity reused with changed content"));}return Ok(cursor);}
            c.execute("INSERT INTO events(id,run_id,kind,payload,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?5)",rusqlite::params![id,run,kind,text,stamp/1000])?;
            c.execute("INSERT INTO telemetry_offsets(run_id,source,sequence,event_id,event_key,timestamp_ms,recovered,payload_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",rusqlite::params![run,source,sequence,id,key,stamp,recovered,hash])?;Ok(c.last_insert_rowid() as u64)
        }).await?;
        self.inner.notify.notify_waiters();
        self.events_after(cursor.saturating_sub(1), 1)
            .await?
            .into_iter()
            .next()
            .ok_or(Error::Invalid("inserted telemetry event missing"))
    }
    pub async fn events_after(&self, cursor: u64, limit: usize) -> Result<Vec<EventEnvelope>> {
        if limit == 0 || limit > 4096 {
            return Err(Error::Invalid("event page size must be 1..4096"));
        }
        self.query_events(cursor, limit, None).await
    }
    async fn query_events(
        &self,
        cursor: u64,
        limit: usize,
        run: Option<String>,
    ) -> Result<Vec<EventEnvelope>> {
        let mut events=self.inner.db.read(ReadLane::Interactive,move|c|{let mut statement=c.prepare("SELECT o.cursor,o.run_id,o.sequence,o.source,e.kind,o.event_key,o.timestamp_ms,o.recovered,e.payload FROM telemetry_offsets o JOIN events e ON e.id=o.event_id WHERE o.cursor>?1 AND (?3 IS NULL OR o.run_id=?3) ORDER BY o.cursor LIMIT ?2")?;let rows=statement.query_map(rusqlite::params![cursor,limit as u64,run],|r|{let kind:String=r.get(4)?;let text:String=r.get(8)?;let payload:Value=serde_json::from_str(&text).map_err(|_|rusqlite::Error::InvalidQuery)?;Ok(EventEnvelope {schema_version:"excalibur.telemetry.v1".into(),cursor:r.get(0)?,run_id:r.get(1)?,sequence:r.get(2)?,source:r.get(3)?,kind:if kind=="resource_sample"{EventKind::ResourceSample}else{EventKind::for_journal(&kind)},journal_kind:kind,key:r.get(5)?,timestamp_unix_ms:r.get(6)?,recovered:r.get(7)?,file_edits:event::file_edits(&payload),payload})})?;Ok(rows.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        for event in &mut events {
            event.payload = sanitize(&self.inner.redactor, &event.payload);
            event.file_edits = event::file_edits(&event.payload);
        }
        Ok(events)
    }
    pub fn subscribe(&self, cursor: u64) -> EventSubscription {
        EventSubscription {
            telemetry: self.clone(),
            cursor,
            pending: VecDeque::new(),
        }
    }
    /// Record journal events missing from telemetry. Events below a retention expiry
    /// watermark (`telemetry_expired`) were deliberately removed and are not re-ingested.
    pub async fn reconcile(&self, journal: &Journal, run: &str) -> Result<u64> {
        let events = journal.replay(run, 0).await?;
        let probe = run.to_owned();
        let through: u64 = self
            .inner
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT coalesce(max(through_sequence),0) FROM telemetry_expired WHERE run_id=?1 AND source='journal'",
                    [probe],
                    |r| r.get(0),
                )?)
            })
            .await?;
        let mut recorded = 0;
        for event in events.iter().filter(|e| e.sequence > through) {
            self.record_recovered_event(run, event).await?;
            recorded += 1;
        }
        Ok(recorded)
    }
    pub(crate) async fn run_data(
        &self,
        run: &str,
    ) -> Result<(RunMetadata, Option<Value>, bool, Vec<EventEnvelope>)> {
        let filter_run = run.to_owned();
        let run = run.to_owned();
        let (metadata, snapshot, finalized) = self
            .inner
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT metadata,snapshot,finalized FROM telemetry_runs WHERE run_id=?1",
                    [run],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, bool>(2)?,
                        ))
                    },
                )?)
            })
            .await?;
        let mut cursor = 0;
        let mut events = Vec::new();
        loop {
            let page = self
                .query_events(cursor, 1024, Some(filter_run.clone()))
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().unwrap().cursor;
            events.extend(page);
        }
        Ok((
            serde_json::from_str(&metadata)?,
            snapshot
                .map(|text| serde_json::from_str(&text))
                .transpose()?,
            finalized,
            events,
        ))
    }
    pub async fn metrics(&self, run: &str) -> Result<TaskMetrics> {
        self.ensure_run(run).await?;
        let (metadata, snapshot, _, mut events) = self.run_data(run).await?;
        events.retain(|event| event.run_id == run);
        events.sort_by_key(|event| (event.source.clone(), event.sequence));
        let resources = events
            .iter()
            .filter(|e| e.source == "resource")
            .map(|e| serde_json::from_value(e.payload.clone()))
            .collect::<std::result::Result<Vec<resources::ResourceSample>, _>>()?;
        Ok(metrics::aggregate(
            run,
            &metadata,
            &events,
            snapshot.as_ref(),
            resources::ResourceSummary::from_samples(&resources),
        ))
    }
    pub async fn finalize<S: Serialize + ?Sized>(
        &self,
        run: &str,
        snapshot: &S,
    ) -> Result<TaskMetrics> {
        self.ensure_run(run).await?;
        let mut snapshot = sanitize(&self.inner.redactor, &serde_json::to_value(snapshot)?);
        if let Some(object) = snapshot.as_object_mut() {
            object.insert("_telemetry_ended_at_ms".into(), json!((self.inner.clock)()));
        }
        let text = serde_json::to_string(&snapshot)?;
        let run_copy = run.to_owned();
        let at = (self.inner.clock)();
        self.inner.db.write(Priority::Journal,move|c|{c.execute("UPDATE telemetry_runs SET snapshot=?2,finalized=1,ended_at_ms=?3 WHERE run_id=?1",rusqlite::params![run_copy,text,at])?;Ok(())}).await?;
        self.export_run(run).await?;
        self.metrics(run).await
    }
    /// Trusted host/benchmark-adapter entry point. Does not execute or read any verifier.
    pub async fn record_verifier(
        &self,
        run: &str,
        reward: Option<f64>,
        output: Value,
    ) -> Result<EventEnvelope> {
        if reward.is_some_and(|n| !n.is_finite()) {
            return Err(Error::Invalid("verifier reward must be finite"));
        }
        self.ensure_run(run).await?;
        let payload = self
            .inner
            .redactor
            .redact_json(&json!({"reward":reward,"output":output,"provenance":"host_adapter"}));
        let result = self
            .insert(
                run,
                "external",
                0,
                "verifier_result",
                "verifier",
                payload,
                (self.inner.clock)(),
                false,
                None,
            )
            .await?;
        self.export_run(run).await?;
        Ok(result)
    }
    pub async fn sample_resources(&self, run: &str) -> Result<resources::ResourceSample> {
        self.ensure_run(run).await?;
        let config = self.inner.resources.clone();
        let at = (self.inner.clock)();
        let sample = tokio::task::spawn_blocking(move || resources::sample(&config, at))
            .await
            .map_err(|_| Error::Invalid("resource probe interrupted"))?;
        self.insert(
            run,
            "resource",
            0,
            "resource_sample",
            "resource",
            serde_json::to_value(&sample)?,
            at,
            false,
            None,
        )
        .await?;
        self.export_run(run).await?;
        Ok(sample)
    }
    pub async fn start_sampler(&self, run: &str) -> Result<resources::ResourceSampler> {
        self.sample_resources(run).await?;
        let telemetry = self.clone();
        let run = run.to_owned();
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut timer = tokio::time::interval_at(
                tokio::time::Instant::now() + std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(1),
            );
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {_=&mut stopped=>break,_=timer.tick()=>{telemetry.sample_resources(&run).await?;}}
            }
            telemetry.sample_resources(&run).await?;
            Ok(telemetry.metrics(&run).await?.resources)
        });
        Ok(resources::ResourceSampler {
            stop: Some(stop),
            task: Some(task),
        })
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.inner.db.shutdown().await?;
        self.inner.notify.notify_waiters();
        Ok(())
    }
}
pub struct EventSubscription {
    telemetry: Telemetry,
    cursor: u64,
    pending: VecDeque<EventEnvelope>,
}
impl EventSubscription {
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
    pub async fn next(&mut self) -> Result<EventEnvelope> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                self.cursor = event.cursor;
                return Ok(event);
            }
            let notified = self.telemetry.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let events = self.telemetry.events_after(self.cursor, 128).await?;
            if !events.is_empty() {
                self.pending = events.into();
                continue;
            }
            notified.await;
        }
    }
}

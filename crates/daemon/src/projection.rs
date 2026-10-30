use excalibur_engine::{DurableSink, Engine};
use excalibur_journal::Event;
use excalibur_telemetry::Telemetry;
use excalibur_types::runtime::BoxFuture;
use std::sync::{Arc, Mutex, Weak};
/// Reuses the embedded event projection and finalizes per-run artifacts from
/// the same confirmed snapshot, without retaining an engine ownership cycle.
pub struct DaemonProjection {
    delegate: excalibur_eval::ExecutionProjection,
    telemetry: Arc<Telemetry>,
    db: excalibur_store::Database,
    engine: Mutex<Weak<Engine>>,
}
impl DaemonProjection {
    pub fn new(telemetry: Arc<Telemetry>, tasks: excalibur_tasks::TaskStore) -> Self {
        Self {
            db: tasks.database().clone(),
            delegate: excalibur_eval::ExecutionProjection::new(telemetry.clone(), tasks),
            telemetry,
            engine: Default::default(),
        }
    }
    pub fn attach(&self, engine: &Arc<Engine>) {
        *self.engine.lock().unwrap_or_else(|p| p.into_inner()) = Arc::downgrade(engine);
    }
    async fn progress(&self, run: &str, through: u64) -> Result<(), String> {
        loop {
            let run = run.to_owned();
            let count = self.db.write(excalibur_store::Priority::Journal, move |conn| {
                use excalibur_store::rusqlite::OptionalExtension;
                let ready: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='master_events')", [], |r| r.get(0))?;
                if !ready { return Ok(0); }
                let agent = conn.query_row("SELECT id,json_extract(body,'$.origin_conversation'),json_extract(body,'$.scope') FROM orchestration_agents WHERE run_id=?1", [&run], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,String>(2)?))).optional()?;
                let Some((agent, conversation, scope)) = agent else { return Ok(0); };
                let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1)", [&conversation], |r| r.get(0))?;
                if !exists { return Ok(0); }
                conn.execute_batch("CREATE TABLE IF NOT EXISTS daemon_progress_receipts(run_id TEXT NOT NULL,sequence INTEGER NOT NULL,cursor INTEGER NOT NULL REFERENCES master_events(sequence),PRIMARY KEY(run_id,sequence))")?;
                let after: u64 = conn.query_row("SELECT COALESCE(MAX(sequence),0) FROM daemon_progress_receipts WHERE run_id=?1", [&run], |r|r.get(0))?;
                let mut q = conn.prepare("SELECT sequence,kind FROM journal WHERE run_id=?1 AND sequence>?2 AND sequence<=?3 AND state='confirmed' ORDER BY sequence LIMIT 256")?;
                let rows = q.query_map(excalibur_store::rusqlite::params![run,after,through], |r| Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?)))?.collect::<Result<Vec<_>,_>>()?;
                for (sequence, kind) in &rows {
                    let payload = serde_json::json!({"run_id":run,"agent_id":agent,"scope":scope,"sequence":sequence,"kind":kind});
                    conn.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'run_progress',?2)", excalibur_store::rusqlite::params![conversation,payload.to_string()])?;
                    conn.execute("INSERT INTO daemon_progress_receipts VALUES(?1,?2,?3)",excalibur_store::rusqlite::params![run,sequence,conn.last_insert_rowid()])?;
                }
                Ok(rows.len())
            }).await.map_err(|_| "progress projection unavailable")?;
            if count < 256 {
                return Ok(());
            }
        }
    }
}
impl DurableSink for DaemonProjection {
    fn record<'a>(&'a self, run: &'a str, event: &'a Event) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.delegate.record(run, event).await?;
            self.progress(run, event.sequence).await?;
            if event.kind == "delivery_report" {
                let engine = self
                    .engine
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .upgrade()
                    .ok_or("projection engine unavailable")?;
                let snapshot = engine
                    .load(run)
                    .await
                    .map_err(|_| "confirmed snapshot unavailable")?;
                self.telemetry
                    .finalize(run, &snapshot)
                    .await
                    .map_err(|_| "trajectory finalization unavailable")?;
            }
            Ok(())
        })
    }
}

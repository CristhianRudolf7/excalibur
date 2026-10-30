//! Conservative detection only; a stalled revision is never automatically replayed.
use crate::*;

pub(crate) async fn initialize(db: &Database) -> Result<()> {
    let sql = include_str!("run_watchdog.sql");
    let checksum = digest(sql.as_bytes());
    db.write(Priority::Journal, move |c| {
        c.execute_batch(sql)?;
        let old: Option<String> = c
            .query_row(
                "SELECT checksum FROM orchestration_run_watchdog_schema WHERE version=1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if old.is_some_and(|old| old != checksum) {
            return Err(excalibur_store::Error::Configuration(
                "run watchdog schema checksum mismatch".into(),
            ));
        }
        c.execute(
            "INSERT OR IGNORE INTO orchestration_run_watchdog_schema VALUES(1,?1)",
            [checksum],
        )?;
        Ok(())
    })
    .await?;
    Ok(())
}
impl Orchestrator {
    /// Host clock input, in UTC seconds. Only actual running revisions can be paused.
    /// Each scan is bounded; registered unresolved tools and all waiting states are excluded.
    pub async fn watchdog_runs(&self, at: i64) -> Result<Vec<String>> {
        let candidates = self.inner.db.read(ReadLane::Background, move |c| {
            let mut q = c.prepare("SELECT a.id,a.run_id,MAX(j.sequence),MAX(CASE WHEN j.kind IN ('step_heartbeat','model_response','tool_result','step_completed') OR (j.kind='state_change' AND json_extract(j.payload,'$.state')='running') THEN j.created_at END) FROM orchestration_agents a JOIN runs r ON r.id=a.run_id JOIN journal j ON j.run_id=r.id WHERE r.state='running' AND a.id>(SELECT agent FROM orchestration_run_watchdog_cursor WHERE id=1) GROUP BY a.id,a.run_id HAVING ?1-MAX(CASE WHEN j.kind IN ('step_heartbeat','model_response','tool_result','step_completed') OR (j.kind='state_change' AND json_extract(j.payload,'$.state')='running') THEN j.created_at END)>=600 ORDER BY a.id LIMIT 128")?;
            Ok(q.query_map([at], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u64>(2)?,r.get::<_,i64>(3)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)
        }).await?;
        let next = candidates
            .last()
            .map(|row| row.0.clone())
            .unwrap_or_default();
        self.inner
            .db
            .write(Priority::Background, move |c| {
                c.execute(
                    "UPDATE orchestration_run_watchdog_cursor SET agent=?1 WHERE id=1",
                    [next],
                )?;
                Ok(())
            })
            .await?;
        let mut paused = vec![];
        for (agent, run, sequence, _) in candidates {
            let state = self.inner.engine.load(&run).await?;
            if state.status != Status::Running
                || state.last_sequence != sequence
                || state
                    .calls
                    .values()
                    .any(|call| call.prepared.is_some() && call.result.is_none())
            {
                continue;
            }
            // This host intent precedes CAS. A model-supplied event/key cannot create authority.
            let r = run.clone();
            let a = agent.clone();
            let admitted=self.inner.db.write(Priority::Journal,move|c|{
                let actual:u64=c.query_row("SELECT COALESCE(MAX(sequence),0) FROM journal WHERE run_id=?1",[&r],|row|row.get(0))?;
                let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM journal WHERE run_id=?1 AND idempotency_key=?2)",rusqlite::params![r,format!("watchdog-stalled:{sequence}")],|row|row.get(0))?;
                if actual!=sequence || exists {return Ok(false);}
                c.execute("INSERT OR IGNORE INTO orchestration_run_watchdog(run_id,sequence,agent_id,observed_at) VALUES(?1,?2,?3,?4)",rusqlite::params![r,sequence,a,at])?;
                Ok(true)
            }).await?;
            if admitted
                && self
                    .inner
                    .engine
                    .pause_stalled_revision(&run, sequence)
                    .await?
            {
                paused.push(agent);
            }
        }
        self.publish_watchdog_notices().await?;
        Ok(paused)
    }
    async fn publish_watchdog_notices(&self) -> Result<()> {
        let pending=self.inner.db.read(ReadLane::Background,|c|{
            let mut q=c.prepare("SELECT w.run_id,w.sequence,w.agent_id,w.observed_at FROM orchestration_run_watchdog w JOIN journal j ON j.run_id=w.run_id AND j.sequence=w.sequence+1 AND j.kind='state_change' AND j.idempotency_key='watchdog-stalled:'||w.sequence WHERE w.notified=0 AND json_extract(j.payload,'$.state')='paused' AND json_extract(j.payload,'$.reason')='watchdog_stalled' ORDER BY w.observed_at,w.run_id LIMIT 128")?;
            Ok(q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)
        }).await?;
        for (run, sequence, agent, at) in pending {
            self.notify(&format!("watchdog:{run}:{sequence}"),&agent,"urgent","Execution paused after ten minutes without durable progress. Inspect and reconcile before resuming; no effect was retried.",at).await?;
            self.inner.db.write(Priority::Journal,move|c|{c.execute("UPDATE orchestration_run_watchdog SET notified=1 WHERE run_id=?1 AND sequence=?2",rusqlite::params![run,sequence])?;Ok(())}).await?;
        }
        Ok(())
    }
}

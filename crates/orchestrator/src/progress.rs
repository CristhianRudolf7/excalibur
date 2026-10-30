//! Durable observation of real plan revisions, independent from narrative reports.
use crate::*;
impl Orchestrator {
    pub(crate) async fn observe_plan_progress(&self, agent: &Agent, step: u64) -> Result<()> {
        let (id, task, scope) = (agent.id.clone(), agent.task.clone(), agent.scope.clone());
        self.inner.db.write(Priority::Interactive,move|c| {
   let plans={let mut q=c.prepare("SELECT p.id,p.version,p.state,p.updated_at FROM plans p WHERE p.owner_task_id=?1 AND COALESCE(p.scope_id,'general')=?2 ORDER BY p.id")?;q.query_map(rusqlite::params![task,scope],|r|Ok(json!([r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?])))?.collect::<std::result::Result<Vec<_>,_>>()?};
   let items={let mut q=c.prepare("SELECT i.id,i.version,i.state,i.updated_at FROM plan_items i LEFT JOIN plans p ON p.id=i.plan_id WHERE (i.task_id=?1 OR p.owner_task_id=?1) AND COALESCE(i.scope_id,p.scope_id,'general')=?2 ORDER BY i.id")?;q.query_map(rusqlite::params![task,scope],|r|Ok(json!([r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?])))?.collect::<std::result::Result<Vec<_>,_>>()?};
   if plans.is_empty()&&items.is_empty(){return Ok(());}
   let at=plans.iter().chain(items.iter()).filter_map(|row|row[3].as_i64()).max().unwrap_or_else(now);
   let fingerprint=digest(json!([plans,items]).to_string().as_bytes());
   c.execute("INSERT INTO orchestration_plan_progress VALUES(?1,?2,?3,?4) ON CONFLICT(agent_id) DO UPDATE SET fingerprint=excluded.fingerprint,step=excluded.step,observed_at=excluded.observed_at WHERE fingerprint!=excluded.fingerprint",rusqlite::params![id,fingerprint,step,at])?;Ok(())
  }).await?;
        Ok(())
    }
}

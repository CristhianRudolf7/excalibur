use crate::*;
use excalibur_store::rusqlite::params;
impl TaskStore {
    /// A bounded DB-only status projection. It never invokes a model.
    pub async fn snapshot(&self, task_id: &str) -> Result<Value> {
        let id = task_id.to_owned();
        let principal = self.policy.principal_id.clone();
        let policy = self.policy.clone();
        let result=self.db.read(ReadLane::Interactive,move|c|{
            let scope:String=c.query_row("SELECT COALESCE(scope_id,'general') FROM tasks WHERE id=?1 AND principal_id IS ?2",params![id,principal],|r|r.get(0))?;
            if policy.allowed_scopes.as_ref().is_some_and(|s|!s.contains(&scope)){return Err(excalibur_store::Error::Invalid("task scope is not authorized"));}
            let task=ops::load(c,"task",&scope,&id,principal.as_deref()).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;
            let(root,total,done):(Option<String>,u64,u64)=c.query_row("SELECT t.root_run_id,(SELECT count(*) FROM plan_items i WHERE i.task_id=t.id OR i.plan_id IN(SELECT id FROM plans WHERE owner_task_id=t.id)),(SELECT count(*) FROM plan_items i WHERE i.state='done' AND(i.task_id=t.id OR i.plan_id IN(SELECT id FROM plans WHERE owner_task_id=t.id))) FROM tasks t WHERE t.id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            let mut runs=Vec::new();if let Some(root)=&root{let mut q=c.prepare("WITH RECURSIVE tree(id,depth) AS(SELECT ?1,0 UNION ALL SELECT l.run_id,t.depth+1 FROM task_run_links l JOIN tree t ON l.parent_run_id=t.id) SELECT r.id,r.task_id,r.state,r.reason,r.agent_id,t.depth FROM tree t JOIN runs r ON r.id=t.id ORDER BY t.depth,r.id LIMIT 21")?;runs=q.query_map([root],|r|Ok(json!({"run_id":r.get::<_,String>(0)?,"task_id":r.get::<_,Option<String>>(1)?,"state":r.get::<_,String>(2)?,"reason":r.get::<_,Option<String>>(3)?,"agent_id":r.get::<_,Option<String>>(4)?,"depth":r.get::<_,u64>(5)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;}
            let more=runs.len()>20;runs.truncate(20);let blockers=runs.iter().filter(|r|r["state"]=="blocked"||r["state"]=="paused").cloned().collect::<Vec<_>>();
            let report=if let Some(root)=root{c.query_row("SELECT payload,payload_blob FROM journal WHERE run_id=?1 AND kind='delivery_report' ORDER BY sequence DESC LIMIT 1",[root],|r|Ok((r.get::<_,Option<String>>(0)?,r.get::<_,Option<String>>(1)?))).optional()?.map(|(text,blob)|text.and_then(|s|serde_json::from_str::<Value>(&s).ok()).unwrap_or(json!({"blob":blob})))}else{None};
            Ok(json!({"task":task,"tree":runs,"tree_has_more":more,"progress":{"done":done,"total":total},"blockers":blockers,"latest_report":report}))
        }).await?;
        Ok(self.redactor.redact_json(&result))
    }
}

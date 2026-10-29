use crate::*;
use excalibur_journal::Event;
use excalibur_store::rusqlite::{Connection, params};
impl TaskStore {
    pub async fn promote(&self, task: &str, input: PromotionInput) -> Result<TaskRecord> {
        let _guard = self.mutation_lock.write().await;
        let (task, policy) = (task.to_owned(), self.policy.clone());
        let value = self
            .db
            .write(Priority::Normal, move |c| {
                promote_tx(c, &task, &input, &policy)
                    .map_err(|e| excalibur_store::Error::Configuration(e.to_string()))
            })
            .await?;
        task_record(value)
    }
    pub async fn promote_for_run(
        &self,
        run: &str,
        input: PromotionInput,
    ) -> Result<Option<TaskRecord>> {
        let Some(task) = self.task_for_run(run).await? else {
            return Ok(None);
        };
        self.promote(&task.id, input).await.map(Some)
    }
    pub async fn observe_event(&self, run: &str, event: &Event) -> Result<()> {
        if !matches!(
            event.kind.as_str(),
            "step_completed" | "tool_result" | "directive_applied" | "state_change"
        ) {
            return Ok(());
        }
        let Some(task) = self.task_for_run(run).await? else {
            return Ok(());
        };
        let paths = if event.kind == "tool_result"
            && event.payload.pointer("/result/success") == Some(&Value::Bool(true))
        {
            event
                .payload
                .pointer("/result/output/files")
                .and_then(Value::as_array)
                .map(|files| {
                    files
                        .iter()
                        .filter_map(|f| f["path"].as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let _guard = if event.kind == "state_change" {
            None
        } else {
            Some(self.mutation_lock.write().await)
        };
        let hash = digest(&serde_json::to_vec(event)?);
        let (id, seq) = (run.to_owned(), event.sequence);
        let old = self
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT payload_hash FROM task_observed_events WHERE run_id=?1 AND sequence=?2",
                    params![id, seq],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
            })
            .await?;
        if let Some(old) = old {
            if old != hash {
                return Err(fail("task event replay changed payload"));
            }
            return Ok(());
        }
        if !paths.is_empty() {
            self.invalidate_paths_locked(&task.scope_id, &paths).await?;
        }
        let (run, event, policy) = (run.to_owned(), event.clone(), self.policy.clone());
        self.db.write(Priority::Journal,move|c|{
            let hash=digest(&serde_json::to_vec(&event).map_err(|_|excalibur_store::Error::Invalid("event serialization failed"))?);
            let old:Option<String>=c.query_row("SELECT payload_hash FROM task_observed_events WHERE run_id=?1 AND sequence=?2",params![run,event.sequence],|r|r.get(0)).optional()?;
            if let Some(old)=old{if old!=hash{return Err(excalibur_store::Error::Invalid("task event replay changed payload"));}return Ok(())}
            if event.kind=="state_change" && event.payload["state"]=="paused"{
                if let Some(id)=event.payload["reason"].as_str().and_then(|r|r.strip_prefix("control:").or_else(||r.strip_prefix("parent:"))){
                    let applied:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM task_control_ops o JOIN task_control_members m ON m.operation_id=o.id WHERE o.id=?1 AND m.run_id=?2 AND o.kind='pause' AND o.state='applied')",params![id,run],|r|r.get(0))?;
                    if applied{c.execute("UPDATE task_control_members SET applied=1 WHERE operation_id=?1 AND run_id=?2",params![id,run])?;c.execute("UPDATE runs SET paused_by=?2 WHERE id=?1",params![run,id])?;}
                    else {
                        // The host may have resumed a deferred pause after admission but before its CAS.
                        let resumed:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM task_control_ops WHERE id=?1 AND kind='pause' AND state='resumed')",[id],|r|r.get(0))?;
                        let latest:u64=c.query_row("SELECT MAX(sequence) FROM journal WHERE run_id=?1",[&run],|r|r.get(0))?;
                        if resumed && latest==event.sequence && crate::control::state(c,&run).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?=="paused" {
                            crate::control::transition(c,&run,"ready","resume",&format!("control:{id}:{run}:deferred-resume"),None).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;
                            crate::control::append(c,&run,"directive_received",&format!("directive:deferred-resume:{id}:{run}"),&json!({"id":format!("resume:{id}:{run}"),"text":"Resume from the durable journal and current micro plan."})).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;
                        }
                    }
                }else if !event.key.starts_with("control:"){c.execute("UPDATE runs SET paused_by=NULL WHERE id=?1",[&run])?;}
            }
            if event.kind=="state_change" && task.root_run_id.as_deref()==Some(&run){
                let current=event.payload["state"].as_str().unwrap_or("unknown");
                let previous:Option<String>=c.query_row("SELECT json_extract(payload,'$.state') FROM journal WHERE run_id=?1 AND kind='state_change' AND sequence<?2 ORDER BY sequence DESC LIMIT 1",params![run,event.sequence],|r|r.get(0)).optional()?;
                let semantic=|state:&str|if matches!(state,"ready"|"running"|"waiting"|"verifying"|"recovering"){"in_progress".to_owned()}else{state.to_owned()};
                if previous.as_deref().map(semantic)!=Some(semantic(current)){crate::planning::dependency_changed(c,&task.id,&policy,"dependency task changed state").map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;}
                if current=="blocked"{crate::planning::request_replan_tx(c,&task.id,"task encountered a blocking condition",vec![task.id.clone()],&policy).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;}
            }
            match event.kind.as_str(){
                "state_change"=>{if task.root_run_id.as_deref()==Some(&run) && event.payload["state"].as_str().is_some_and(crate::control::terminal){let mut q=c.prepare("SELECT id FROM plans WHERE owner_task_id=?1 AND kind='micro' AND state<>'archived'")?;let ids=q.query_map([&task.id],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;for id in ids{crate::planning::save_plan(c,&id).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;c.execute("UPDATE plans SET state='archived',version=version+1 WHERE id=?1",[&id])?;crate::planning::save_plan(c,&id).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;}}}
                "directive_applied" if task.root_run_id.as_deref()==Some(&run)=>{
                    let revision:u64=c.query_row("SELECT count(*) FROM journal WHERE run_id=?1 AND kind='directive_applied' AND sequence<=?2",params![run,event.sequence],|r|r.get(0))?;
                    c.execute("UPDATE runs SET instructions_rev_seen=?2 WHERE id=?1",params![run,revision])?;
                    c.execute("UPDATE criteria SET instruction_revision=?2 WHERE requirement_id IN(SELECT requirement_id FROM task_requirements WHERE task_id=?1)",params![task.id,revision])?;
                    c.execute("UPDATE evidence SET stale=1 WHERE check_id IN(SELECT k.id FROM checks k JOIN criteria c ON c.id=k.criterion_id JOIN task_requirements tr ON tr.requirement_id=c.requirement_id WHERE tr.task_id=?1) AND instruction_revision<>?2",params![task.id,revision])?;
                    if let Some(id)=event.payload["id"].as_str(){c.execute("UPDATE task_directives SET state='applied' WHERE id=?1",[id])?;}
                    if !event.payload["id"].as_str().is_some_and(|id|id.starts_with("replan:")){crate::planning::request_replan_tx(c,&task.id,"applicable instruction changed",vec![task.id.clone()],&policy).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;}

                }
                "step_completed"|"tool_result"=>{
                    let steps:u64=c.query_row("SELECT count(*) FROM journal WHERE run_id=?1 AND kind='step_completed' AND sequence<=?2",params![run,event.sequence],|r|r.get(0))?;
                    let project=registered_project(c,&task,&paths,policy.principal_id.as_deref()).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;
                    let milestones:u64=c.query_row("SELECT count(*) FROM milestones WHERE scope_id=?1",[&task.scope_id],|r|r.get(0))?;
                    let input=PromotionInput{touched_project:project,steps,estimated_seconds:task.estimate_seconds,milestone_count:milestones,user_requests_initiative:false,forced_level:None};
                    promote_tx(c,&task.id,&input,&policy).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;
                }
                _=>{}
            }
            c.execute("INSERT INTO task_observed_events VALUES(?1,?2,?3)",params![run,event.sequence,hash])?;Ok(())
        }).await?;
        Ok(())
    }
}
fn promote_tx(
    c: &Connection,
    task: &str,
    input: &PromotionInput,
    policy: &TaskPolicy,
) -> Result<Value> {
    let scope: String = c.query_row(
        "SELECT COALESCE(scope_id,'general') FROM tasks WHERE id=?1 AND principal_id IS ?2",
        params![task, policy.principal_id],
        |r| r.get(0),
    )?;
    if policy
        .allowed_scopes
        .as_ref()
        .is_some_and(|s| !s.contains(&scope))
    {
        return Err(fail("task scope is not authorized"));
    }
    let old = ops::load(c, "task", &scope, task, policy.principal_id.as_deref())?;
    let mut level = old["level"].as_str().unwrap_or("N1").to_owned();
    let forced = old["forced_level"].as_u64() == Some(1);
    if let Some(force) = input.forced_level {
        level = force.label().into();
    } else if !forced {
        if level == "N1" && (input.touched_project.is_some() || input.steps > 15) {
            level = "N2".into();
        }
        if level == "N2"
            && (input.estimated_seconds.is_some_and(|n| n > 86400)
                || input.milestone_count > 1
                || input.user_requests_initiative)
        {
            level = "N3".into();
        }
    }
    let project = if let Some(project) = &input.touched_project {
        if !policy.allow_projects && old["project_id"].as_str() != Some(project.as_str()) {
            return Err(fail("project binding is disabled by this task policy"));
        }
        if policy
            .allowed_scopes
            .as_ref()
            .is_some_and(|scopes| !scopes.contains(project))
        {
            return Err(fail("project scope is not authorized"));
        }
        let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1 AND state='active' AND principal_id IS ?2)",params![project,policy.principal_id],|r|r.get(0))?;
        if !exists {
            return Err(fail("touched project is not registered"));
        }
        Some(project.clone())
    } else {
        old["project_id"].as_str().map(str::to_owned)
    };
    let changed = old["level"].as_str() != Some(&level)
        || input.forced_level.is_some()
        || old["project_id"].as_str() != project.as_deref()
        || input
            .estimated_seconds
            .is_some_and(|n| old["estimate_seconds"].as_u64() != Some(n));
    if changed {
        c.execute("UPDATE tasks SET level=?2,project_id=?3,forced_level=?4,estimate_seconds=COALESCE(?5,estimate_seconds),version=version+1,updated_at=unixepoch() WHERE id=?1",params![task,level,project,forced||input.forced_level.is_some(),input.estimated_seconds])?;
        if matches!(level.as_str(), "N2" | "N3") {
            crate::planning::attach_macro_item(
                c,
                task,
                project.as_deref().unwrap_or(&scope),
                policy.principal_id.as_deref(),
            )?;
        }
        ops::save_version(c, "task", &scope, task, policy.principal_id.as_deref())?;
    }
    ops::load(c, "task", &scope, task, policy.principal_id.as_deref())
}
fn registered_project(
    c: &Connection,
    task: &TaskRecord,
    paths: &[String],
    principal: Option<&str>,
) -> Result<Option<String>> {
    let mut q = c.prepare(
        "SELECT id,roots FROM projects WHERE state='active' AND principal_id IS ?1 ORDER BY id",
    )?;
    let projects = q
        .query_map([principal], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (id, roots) in projects {
        let roots: Vec<String> = serde_json::from_str(&roots)?;
        for path in paths {
            let p = std::path::Path::new(path);
            let full = if p.is_absolute() {
                p.to_path_buf()
            } else {
                let Some(workspace) = task.metadata["workspace"].as_str() else {
                    continue;
                };
                std::path::Path::new(workspace).join(p)
            };
            if roots.iter().any(|root| full.starts_with(root)) {
                return Ok(Some(id));
            }
        }
    }
    Ok(None)
}
impl TaskStore {
    pub async fn admission_state(&self, run: &str) -> Result<excalibur_engine::AdmissionState> {
        let Some(task) = self.task_for_run(run).await? else {
            return Ok(excalibur_engine::AdmissionState::Ready);
        };
        self.check_scope(&task.scope_id)?;
        let run = run.to_owned();
        self.db.read(ReadLane::Interactive,move|c|{
            let deferred:Option<String>=c.query_row("SELECT o.id FROM task_control_members m JOIN task_control_ops o ON o.id=m.operation_id WHERE m.run_id=?1 AND m.applied=0 AND o.kind='pause' AND o.state='applied' ORDER BY o.created_at DESC LIMIT 1",[&run],|r|r.get(0)).optional()?;
            if let Some(id)=deferred{return Ok(excalibur_engine::AdmissionState::Paused(format!("control:{id}")))}
            let mut q=c.prepare("WITH RECURSIVE ancestors(id,depth) AS(SELECT parent_run_id,1 FROM task_run_links WHERE run_id=?1 AND parent_run_id IS NOT NULL UNION ALL SELECT l.parent_run_id,a.depth+1 FROM task_run_links l JOIN ancestors a ON l.run_id=a.id WHERE l.parent_run_id IS NOT NULL) SELECT id FROM ancestors ORDER BY depth")?;
            let ids=q.query_map([run],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
            for parent in ids{let state=crate::control::state(c,&parent).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;if crate::control::terminal(&state){return Ok(excalibur_engine::AdmissionState::Cancelled(format!("parent {parent} is terminal")))}
if state=="paused"{let owner:Option<String>=c.query_row("SELECT paused_by FROM runs WHERE id=?1",[&parent],|r|r.get(0))?;return Ok(excalibur_engine::AdmissionState::Paused(format!("parent:{}",owner.unwrap_or(parent))))}
if state=="blocked"{return Ok(excalibur_engine::AdmissionState::Blocked(format!("parent {parent} is blocked")))}}
            Ok(excalibur_engine::AdmissionState::Ready)
        }).await.map_err(Error::from)
    }
}

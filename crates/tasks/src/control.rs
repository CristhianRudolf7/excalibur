use crate::*;
use excalibur_store::rusqlite::{Connection, params};
pub(crate) fn terminal(state: &str) -> bool {
    matches!(
        state,
        "completed" | "completed_with_pending" | "cancelled" | "failed"
    )
}
pub(crate) fn state(c: &Connection, run: &str) -> Result<String> {
    let row:Option<String>=c.query_row("SELECT json_extract(payload,'$.state') FROM journal WHERE run_id=?1 AND kind='state_change' AND state='confirmed' ORDER BY sequence DESC LIMIT 1",[run],|r|r.get(0)).optional()?;
    if let Some(state) = row {
        return Ok(state);
    }
    let seed:Option<String>=c.query_row("SELECT payload FROM journal WHERE run_id=?1 AND kind='run_started' ORDER BY sequence LIMIT 1",[run],|r|r.get(0)).optional()?;
    if let Some(seed) = seed {
        let value: Value = serde_json::from_str(&seed)?;
        return Ok(if value["initial_state"] == "created" {
            "created"
        } else {
            "running"
        }
        .into());
    }
    Ok(c.query_row("SELECT state FROM runs WHERE id=?1", [run], |r| r.get(0))?)
}
pub(crate) fn append(
    c: &Connection,
    run: &str,
    kind: &str,
    key: &str,
    payload: &Value,
) -> Result<u64> {
    let text = payload.to_string();
    if let Some((sequence, old_kind, old_text)) = c
        .query_row(
            "SELECT sequence,kind,payload FROM journal WHERE run_id=?1 AND idempotency_key=?2",
            params![run, key],
            |r| {
                Ok((
                    r.get::<_, u64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
    {
        if old_kind != kind || old_text != text {
            return Err(fail("journal key conflict"));
        }
        return Ok(sequence);
    }
    let sequence: u64 = c.query_row(
        "SELECT COALESCE(MAX(sequence),0)+1 FROM journal WHERE run_id=?1",
        [run],
        |r| r.get(0),
    )?;
    c.execute("INSERT INTO journal(id,run_id,sequence,kind,payload,state,idempotency_key) VALUES(?1,?2,?3,?4,?5,'confirmed',?6)",params![new_id("job"),run,sequence,kind,text,key])?;
    let wake_target = if kind == "directive_received" && payload["wake"].as_bool() != Some(false) {
        match state(c, run)?.as_str() {
            "waiting" => Some("running"),
            "blocked" => Some("ready"),
            _ => None,
        }
    } else {
        None
    };
    if let Some(target) = wake_target {
        append(
            c,
            run,
            "wake_event",
            &format!("{key}:wake"),
            &json!({"event":"directive","evidence":key}),
        )?;
        transition(
            c,
            run,
            target,
            "directive",
            &format!("{key}:wake-state"),
            None,
        )?;
    }
    Ok(sequence)
}
pub(crate) fn transition(
    c: &Connection,
    run: &str,
    target: &str,
    reason: &str,
    key: &str,
    paused_by: Option<&str>,
) -> Result<bool> {
    if terminal(&state(c, run)?) {
        return Ok(false);
    }
    append(
        c,
        run,
        "state_change",
        key,
        &json!({"state":target,"reason":reason}),
    )?;
    c.execute(
        "UPDATE runs SET state=?2,reason=?3,paused_by=?4,updated_at=unixepoch() WHERE id=?1",
        params![run, target, reason, paused_by],
    )?;
    Ok(true)
}
impl TaskStore {
    pub async fn bind_root(&self, task: &str, run: &str) -> Result<()> {
        self.bind(task, run, None, false, "initial execution").await
    }
    pub async fn reopen(&self, task: &str, run: &str, reason: &str) -> Result<()> {
        self.bind(task, run, None, true, reason).await
    }
    pub async fn bind_child(&self, task: &str, run: &str, parent: &str) -> Result<()> {
        self.bind(task, run, Some(parent), false, "child execution")
            .await
    }
    async fn bind(
        &self,
        task: &str,
        run: &str,
        parent: Option<&str>,
        reopen: bool,
        reason: &str,
    ) -> Result<()> {
        let _guard = self.mutation_lock.write().await;
        let (task, run, parent, reason, policy) = (
            task.to_owned(),
            run.to_owned(),
            parent.map(str::to_owned),
            self.redactor.redact_text(reason),
            self.policy.clone(),
        );
        self.db.write(Priority::Journal,move|c|{let scope:String=c.query_row("SELECT COALESCE(scope_id,'general') FROM tasks WHERE id=?1 AND principal_id IS ?2",params![task,policy.principal_id],|r|r.get(0))?;if policy.allowed_scopes.as_ref().is_some_and(|s|!s.contains(&scope)){return Err(excalibur_store::Error::Invalid("task scope is not authorized"));}let previous:Option<String>=c.query_row("SELECT root_run_id FROM tasks WHERE id=?1",[&task],|r|r.get(0))?;if parent.is_none(){if previous.as_deref()==Some(&run){return Ok(())}
if let Some(previous)=previous{let current=state(c,&previous).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;if !reopen||!terminal(&current){return Err(excalibur_store::Error::Invalid("task already has a root; explicit reopen requires terminal previous root"));}}else if reopen{return Err(excalibur_store::Error::Invalid("task has no previous execution to reopen"));}}
        let run_scope:Option<Option<String>>=c.query_row("SELECT scope_id FROM runs WHERE id=?1 AND principal_id IS ?2",params![run,policy.principal_id],|r|r.get(0)).optional()?;let Some(run_scope)=run_scope else{return Err(excalibur_store::Error::Invalid("run does not exist for task actor"));};if run_scope.as_ref().is_some_and(|s|s!=&scope){return Err(excalibur_store::Error::Invalid("run belongs to another scope"));}
        let owner:Option<(String,Option<String>)>=c.query_row("SELECT task_id,parent_run_id FROM task_run_links WHERE run_id=?1",[&run],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;if owner.as_ref().is_some_and(|(id,known_parent)|id!=&task||known_parent!=&parent){return Err(excalibur_store::Error::Invalid("run already has a different task or parent binding"));}
        if let Some(parent)=&parent{let known:Option<String>=c.query_row("SELECT l.task_id FROM task_run_links l JOIN tasks t ON t.id=l.task_id WHERE l.run_id=?1 AND t.scope_id=?2 AND t.principal_id IS ?3",params![parent,scope,policy.principal_id],|r|r.get(0)).optional()?;if known.is_none(){return Err(excalibur_store::Error::Invalid("parent run has no task binding"));}
if parent==&run{return Err(excalibur_store::Error::Invalid("run cannot parent itself"));}let parent_state=state(c,parent).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;if terminal(&parent_state)||parent_state=="paused"{return Err(excalibur_store::Error::Invalid("cannot bind a new child to a paused or terminal parent"));}let cycle:bool=c.query_row("WITH RECURSIVE nodes(id) AS(SELECT ?1 UNION SELECT run_id FROM task_run_links l JOIN nodes n ON l.parent_run_id=n.id) SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?2)",params![run,parent],|r|r.get(0))?;if cycle{return Err(excalibur_store::Error::Invalid("run parent would create cycle"));}}
        c.execute("INSERT INTO task_run_links VALUES(?1,?2,?3) ON CONFLICT(run_id) DO NOTHING",params![run,task,parent])?;c.execute("UPDATE runs SET task_id=?2,scope_id=?3 WHERE id=?1",params![run,task,scope])?;
        if parent.is_none(){if reopen{reset_instruction_epoch(c,&task,&run,&scope,policy.principal_id.as_deref()).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;}let ordinal:u64=c.query_row("SELECT COALESCE(MAX(ordinal),0)+1 FROM task_root_history WHERE task_id=?1",[&task],|r|r.get(0))?;c.execute("INSERT INTO task_root_history VALUES(?1,?2,?3,?4,?5)",params![task,run,ordinal,reason,excalibur_store::unix_seconds()])?;c.execute("UPDATE tasks SET root_run_id=?2,version=version+1,updated_at=unixepoch() WHERE id=?1",params![task,run])?;ops::save_version(c,"task",&scope,&task,policy.principal_id.as_deref()).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;}
        Ok(())}).await?;
        Ok(())
    }
    pub async fn pause_subtree(&self, task: &str, key: &str) -> Result<ControlReceipt> {
        let receipt = self.control(task, key, "pause", false, "user").await?;
        self.control_reports(&receipt).await?;
        Ok(receipt)
    }
    pub async fn cancel_subtree(
        &self,
        task: &str,
        key: &str,
        confirmed: bool,
    ) -> Result<ControlReceipt> {
        let receipt = self.control(task, key, "cancel", confirmed, "user").await?;
        self.control_reports(&receipt).await?;
        Ok(receipt)
    }
    async fn control(
        &self,
        task: &str,
        key: &str,
        kind: &str,
        confirmed: bool,
        reason: &str,
    ) -> Result<ControlReceipt> {
        let _guard = self.mutation_lock.write().await;
        let (task, key, kind, reason, policy) = (
            task.to_owned(),
            key.to_owned(),
            kind.to_owned(),
            self.redactor.redact_text(reason),
            self.policy.clone(),
        );
        self.db.write(Priority::Journal,move|c|{
        authorize_task(c,&task,&policy).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;
        if let Some((id,old_task,old_kind,applied))=c.query_row("SELECT id,task_id,kind,state FROM task_control_ops WHERE operation_key=?1",[&key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?))).optional()?{if old_task!=task||old_kind!=kind{return Err(excalibur_store::Error::Invalid("control operation key conflict"));}
if matches!(applied.as_str(),"applied"|"resumed"){return control_receipt(c,&id).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))}
if kind=="cancel"&&(confirmed||policy.autonomy==3){apply_control(c,&id).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;}return control_receipt(c,&id).map_err(|e|excalibur_store::Error::Configuration(e.to_string()));}
        let(scope,root):(String,Option<String>)=c.query_row("SELECT COALESCE(scope_id,'general'),root_run_id FROM tasks WHERE id=?1 AND principal_id IS ?2",params![task,policy.principal_id],|r|Ok((r.get(0)?,r.get(1)?)))?;if policy.allowed_scopes.as_ref().is_some_and(|s|!s.contains(&scope)){return Err(excalibur_store::Error::Invalid("task scope is not authorized"));}let root=root.ok_or(excalibur_store::Error::Invalid("task has no root execution"))?;let id=new_id("ctl");let approved=kind!="cancel"||confirmed||policy.autonomy==3;c.execute("INSERT INTO task_control_ops VALUES(?1,?2,?3,?4,?5,'pending',?6,?7)",params![id,key,task,kind,approved,reason,excalibur_store::unix_seconds()])?;
        let mut query=c.prepare("WITH RECURSIVE nodes(id,depth) AS(SELECT ?1,0 UNION ALL SELECT l.run_id,n.depth+1 FROM task_run_links l JOIN nodes n ON l.parent_run_id=n.id) SELECT id,depth FROM nodes ORDER BY depth DESC,id")?;let rows=query.query_map([root],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;for(run,depth)in rows{let state=state(c,&run).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;if !terminal(&state)&&(kind!="pause"||state!="paused"){c.execute("INSERT INTO task_control_members VALUES(?1,?2,?3,?4,0)",params![id,run,state,depth])?;}}
        if approved{apply_control(c,&id).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;}control_receipt(c,&id).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))
    }).await.map_err(Error::from)
    }
    async fn control_reports(&self, receipt: &ControlReceipt) -> Result<()> {
        if !receipt.applied {
            return Ok(());
        }
        let journal = excalibur_journal::Journal::new(self.db.clone()).await?;
        for run in &receipt.runs {
            let (lookup, pause_key, cancel_key) = (
                run.clone(),
                format!("control:{}:{run}:paused", receipt.id),
                format!("control:{}:{run}:cancelled", receipt.id),
            );
            let seq=self.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT COALESCE(MAX(sequence),0) FROM journal WHERE run_id=?1 AND kind='state_change' AND idempotency_key IN(?2,?3)",params![lookup,pause_key,cancel_key],|r|r.get::<_,u64>(0))?)).await?;
            if seq == 0 {
                continue;
            }
            let key = format!("delivery:control:{}:{seq}", receipt.id);
            let (lookup, keylookup) = (run.clone(), key.clone());
            let known=self.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM journal WHERE run_id=?1 AND idempotency_key=?2)",params![lookup,keylookup],|r|r.get::<_,bool>(0))?)).await?;
            if known {
                continue;
            }
            let report = excalibur_engine::delivery_report_at(&journal, run, seq)
                .await
                .map_err(|e| fail(e.to_string()))?;
            let payload = self.redactor.redact_json(&serde_json::to_value(report)?);
            journal
                .append(run, "delivery_report", &key, payload)
                .await?;
        }
        Ok(())
    }
    pub async fn resume_subtree(&self, pause_id: &str, key: &str) -> Result<ControlReceipt> {
        let _guard = self.mutation_lock.write().await;
        let (pause_id, key) = (pause_id.to_owned(), key.to_owned());
        let policy = self.policy.clone();
        self.db.write(Priority::Journal,move|c|{let mut receipt=control_receipt(c,&pause_id).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;authorize_task(c,&receipt.task_id,&policy).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;if receipt.kind!="pause"||!receipt.applied{return Err(excalibur_store::Error::Invalid("resume requires an applied pause operation"));}let mut members=c.prepare("SELECT run_id FROM task_control_members WHERE operation_id=?1 AND applied=1 ORDER BY depth,run_id")?;let runs=members.query_map([&pause_id],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;c.execute("UPDATE task_control_ops SET state='resumed' WHERE id=?1",[&pause_id])?;c.execute("UPDATE task_control_members SET applied=2 WHERE operation_id=?1 AND applied=0",[&pause_id])?;receipt.runs.clear();receipt.deferred_runs.clear();for run in runs{let paused_by:Option<String>=c.query_row("SELECT paused_by FROM runs WHERE id=?1",[&run],|r|r.get(0))?;if state(c,&run).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?=="paused"&&paused_by.as_deref()==Some(&pause_id){transition(c,&run,"ready","resume",&format!("control:{key}:{run}:resume"),None).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;append(c,&run,"directive_received",&format!("directive:{key}:{run}:received"),&json!({"id":format!("resume:{key}:{run}"),"text":"Resume from the durable journal and current micro plan."})).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;receipt.runs.push(run);}}
        receipt.kind="resume".into();Ok(receipt)
    }).await.map_err(Error::from)
    }
}
/// Revision numbers are local to an execution. Rebinding a terminal task creates a
/// new criterion version so an old exemption at the same numeric revision cannot revive.
fn reset_instruction_epoch(
    c: &Connection,
    task: &str,
    run: &str,
    scope: &str,
    principal: Option<&str>,
) -> Result<()> {
    let revision: u64 = c.query_row(
        "SELECT count(*) FROM journal WHERE run_id=?1 AND kind='directive_applied' AND state='confirmed'",
        [run], |r| r.get(0),
    )?;
    let mut query = c.prepare(
        "SELECT cr.id FROM criteria cr JOIN task_requirements tr ON tr.requirement_id=cr.requirement_id WHERE tr.task_id=?1 AND cr.scope_id=?2 AND cr.principal_id IS ?3 ORDER BY cr.id",
    )?;
    let criteria = query
        .query_map(params![task, scope, principal], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for id in criteria {
        ops::save_version(c, "criterion", scope, &id, principal)?;
        c.execute(
            "UPDATE criteria SET version=version+1,instruction_revision=?2,updated_at=unixepoch() WHERE id=?1",
            params![id,revision],
        )?;
        c.execute("UPDATE evidence SET stale=1 WHERE check_id IN(SELECT id FROM checks WHERE criterion_id=?1)",[&id])?;
        c.execute(
            "INSERT INTO task_criterion_state(criterion_id,state,reason,latest_evidence_id,instruction_revision) VALUES(?1,'unverified','task reopened in a new instruction epoch',NULL,?2) ON CONFLICT(criterion_id) DO UPDATE SET state=excluded.state,reason=excluded.reason,latest_evidence_id=NULL,instruction_revision=excluded.instruction_revision",
            params![id,revision],
        )?;
        ops::save_version(c, "criterion", scope, &id, principal)?;
    }
    c.execute(
        "UPDATE runs SET instructions_rev_seen=?2 WHERE id=?1",
        params![run, revision],
    )?;
    Ok(())
}

fn apply_control(c: &Connection, id: &str) -> Result<()> {
    let (kind, task): (String, String) = c.query_row(
        "SELECT kind,task_id FROM task_control_ops WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let root: Option<String> =
        c.query_row("SELECT root_run_id FROM tasks WHERE id=?1", [task], |r| {
            r.get(0)
        })?;
    if kind == "cancel" {
        // Confirmation covers the current subtree, including children bound while it was pending.
        if let Some(root) = &root {
            let mut q=c.prepare("WITH RECURSIVE tree(id,depth) AS(SELECT ?1,0 UNION ALL SELECT l.run_id,t.depth+1 FROM task_run_links l JOIN tree t ON l.parent_run_id=t.id) SELECT id,depth FROM tree")?;
            let children = q
                .query_map([root], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (run, depth) in children {
                let current = state(c, &run)?;
                if !terminal(&current) {
                    c.execute(
                        "INSERT OR IGNORE INTO task_control_members VALUES(?1,?2,?3,?4,0)",
                        params![id, run, current, depth],
                    )?;
                }
            }
        }
    }
    let mut q=c.prepare("SELECT run_id FROM task_control_members WHERE operation_id=?1 AND applied=0 ORDER BY depth DESC,run_id")?;
    let runs = q
        .query_map([id], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for run in runs {
        if kind == "pause" && state(c, &run)? == "created" {
            continue;
        }
        let reason = if root.as_deref() == Some(&run) {
            "user"
        } else {
            "parent"
        };
        let target = if kind == "pause" {
            "paused"
        } else {
            "cancelled"
        };
        if transition(
            c,
            &run,
            target,
            reason,
            &format!("control:{id}:{run}:{target}"),
            (kind == "pause").then_some(id),
        )? {
            c.execute(
                "UPDATE task_control_members SET applied=1 WHERE operation_id=?1 AND run_id=?2",
                params![id, run],
            )?;
            c.execute("DELETE FROM leases WHERE holder_id=?1", [run])?;
        }
    }
    c.execute(
        "UPDATE task_control_ops SET state='applied',confirmed=1 WHERE id=?1",
        [id],
    )?;
    Ok(())
}
fn control_receipt(c: &Connection, id: &str) -> Result<ControlReceipt> {
    let (kind, task, applied): (String, String, String) = c.query_row(
        "SELECT kind,task_id,state FROM task_control_ops WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let mut q = c.prepare(
        "SELECT run_id FROM task_control_members WHERE operation_id=?1 ORDER BY depth DESC,run_id",
    )?;
    let runs = q
        .query_map([id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut deferred=c.prepare("SELECT run_id FROM task_control_members WHERE operation_id=?1 AND prior_state='created' AND applied=0")?;
    let deferred_runs = if kind == "pause" && applied == "applied" {
        deferred
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    Ok(ControlReceipt {
        id: id.into(),
        kind: kind.clone(),
        task_id: task,
        runs,
        confirmation_required: kind == "cancel" && applied == "pending",
        applied: matches!(applied.as_str(), "applied" | "resumed"),
        deferred_runs,
    })
}
impl TaskStore {
    pub async fn reserve_child_budget(
        &self,
        parent: &str,
        allocation_id: &str,
        child: &str,
        budget: excalibur_engine::Budget,
    ) -> Result<excalibur_engine::Budget> {
        let parent_task = self
            .task_for_run(parent)
            .await?
            .ok_or_else(|| fail("parent has no task binding"))?;
        self.check_scope(&parent_task.scope_id)?;
        let (parent_id, child_id) = (parent.to_owned(), child.to_owned());
        let exact=self.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM task_run_links WHERE run_id=?1 AND parent_run_id=?2)",params![child_id,parent_id],|r|r.get::<_,bool>(0))?)).await?;
        if !exact {
            return Err(fail("child is not uniquely bound to this parent"));
        }
        let journal = excalibur_journal::Journal::new(self.db.clone()).await?;
        excalibur_engine::reserve_child_budget(&journal, parent, allocation_id, child, budget)
            .await
            .map_err(|e| fail(e.to_string()))
    }
}

pub(crate) fn authorize_task(c: &Connection, task: &str, policy: &TaskPolicy) -> Result<String> {
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
    Ok(scope)
}

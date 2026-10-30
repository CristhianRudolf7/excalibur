use crate::*;
use excalibur_engine::Status;
impl Orchestrator {
    pub async fn control(&self, agent: &str, action: Control) -> Result<ControlReport> {
        self.control_keyed(&AgentId::new().to_string(), agent, action)
            .await
    }
    /// Commands serialize through one actor gate. The persisted intent is replayable after a crash.
    pub async fn control_keyed(
        &self,
        operation: &str,
        target: &str,
        action: Control,
    ) -> Result<ControlReport> {
        if operation.is_empty() || operation.len() > 256 {
            return Err(invalid("control operation required"));
        }
        if let Control::Redirect { text } = &action
            && (text.trim().is_empty() || text.len() > 64 * 1024)
        {
            return Err(invalid("redirect requires a nonempty literal up to 64 KiB"));
        }
        let _guard = self.inner.gate.lock().await;
        let target_agent = self.agent(target).await?;
        if target_agent.depth == 0 {
            return Err(invalid("control a run rather than the master"));
        }
        if matches!(action, Control::Cancel { confirmed: false }) {
            return Err(invalid("cancellation requires explicit confirmation"));
        }
        self.validate_durable(&serde_json::to_value(&action)?)?;
        let action_json = serde_json::to_string(&action)?;
        let op = operation.to_owned();
        let target_owned = target.to_owned();
        let body = action_json.clone();
        let known=self.inner.db.write(Priority::Interactive,move|c|{
            let row:Option<(String,String,Option<String>)>=c.query_row("SELECT target,action,report FROM orchestration_controls WHERE operation=?1",[&op],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if let Some((t,a,r))=row{if t!=target_owned||a!=body{return Err(excalibur_store::Error::Invalid("control operation reused"));}return Ok(r);}
            c.execute("INSERT INTO orchestration_controls VALUES(?1,?2,?3,?3,'pending',NULL)",rusqlite::params![op,target_owned,body])?;Ok(None)
        }).await?;
        if let Some(report) = known {
            return Ok(serde_json::from_str(&report)?);
        }
        let all = self.list_agents().await?;
        let mut tree = vec![target_agent];
        let mut i = 0;
        while i < tree.len() {
            let parent = tree[i].id.clone();
            tree.extend(
                all.iter()
                    .filter(|a| a.parent_id.as_deref() == Some(parent.as_str()))
                    .cloned(),
            );
            i += 1;
        }
        let redirect_branch = tree.clone();
        if matches!(action, Control::Interrupt | Control::Redirect { .. }) {
            tree.truncate(1);
        }
        if matches!(action, Control::Cancel { .. }) {
            tree.reverse();
        }
        let mut report = ControlReport {
            operation: operation.into(),
            ..Default::default()
        };
        let root_pause = self.paused_by(target).await?;
        let cascade_owner = root_pause
            .as_ref()
            .filter(|owner| !matches!(owner.as_str(), "resource" | "shutdown"))
            .cloned()
            .unwrap_or_else(|| operation.to_owned());
        for a in tree {
            let Some(run) = &a.run_id else { continue };
            let before = self.inner.engine.load(run).await?;
            if terminal(&before.status) {
                if matches!(action, Control::Cancel { .. }) {
                    self.finalize_agent_stop(&a).await?;
                }
                report.unchanged.push(a.id);
                continue;
            }
            match &action {
                Control::Pause | Control::Interrupt => {
                    if before.status == Status::Paused {
                        let owner = self.paused_by(&a.id).await?;
                        // Repeated human pause/interrupt retains its cascade
                        // ownership. Independently paused descendants keep theirs.
                        if a.id == target
                            && owner
                                .as_deref()
                                .is_none_or(|owner| matches!(owner, "resource" | "shutdown"))
                        {
                            self.set_pause_owner(&a.id, Some(operation.to_owned()), false)
                                .await?;
                            self.inner.engine.pause_with_reason(run, "user").await?;
                        }
                        report.unchanged.push(a.id);
                        continue;
                    }
                    // Persist ownership before the state transition, so retry preserves cascade ownership.
                    self.set_pause_owner(&a.id, Some(cascade_owner.clone()), false)
                        .await?;
                    if let Err(error) = self
                        .inner
                        .engine
                        .pause_with_reason(run, if a.id == target { "user" } else { "parent" })
                        .await
                    {
                        if terminal(&self.inner.engine.load(run).await?.status) {
                            self.set_pause_owner(&a.id, None, false).await?;
                            report.unchanged.push(a.id);
                            continue;
                        }
                        return Err(error.into());
                    }
                    report.affected.push(a.id.clone());
                    if before
                        .calls
                        .values()
                        .any(|c| c.prepared.is_some() && c.result.is_none())
                    {
                        report.pending.push(a.id.clone());
                    }
                }
                Control::Resume => {
                    let owned = self.paused_by(&a.id).await?;
                    if before.status == Status::Paused
                        && owned.as_deref() != Some("resource")
                        && (a.id == target || owned.is_some() && owned == root_pause)
                    {
                        self.inner.engine.resume(run).await?;
                        self.set_pause_owner(&a.id, None, false).await?;
                        report.affected.push(a.id);
                    } else {
                        report.unchanged.push(a.id);
                    }
                }
                Control::Cancel { .. } => {
                    if let Err(error) = self.inner.engine.cancel(run).await
                        && !terminal(&self.inner.engine.load(run).await?.status)
                    {
                        return Err(error.into());
                    }
                    self.set_pause_owner(&a.id, None, false).await?;
                    self.finalize_agent_stop(&a).await?;
                    report.affected.push(a.id);
                }
                Control::Redirect { text } => {
                    if text.trim().is_empty() {
                        return Err(invalid("redirect requires literal instruction"));
                    }
                    self.persist_redirect_instruction(operation, &a, text, &redirect_branch)
                        .await?;
                    if before.status != Status::Paused {
                        self.inner.engine.pause(run).await?;
                    }
                    self.clear_agent_controls(&a.id).await?;
                    self.inner.engine.directive(run, operation, text).await?;
                    self.inner.engine.resume(run).await?;
                    self.set_pause_owner(&a.id, None, false).await?;
                    report.affected.push(a.id);
                }
            }
        }
        let encoded = serde_json::to_string(&report)?;
        let op = operation.to_owned();
        self.inner.db.write(Priority::Interactive,move|c|{c.execute("UPDATE orchestration_controls SET state='completed',report=?2 WHERE operation=?1",rusqlite::params![op,encoded])?;Ok(())}).await?;
        self.inner.wake.notify_waiters();
        Ok(report)
    }
    async fn persist_redirect_instruction(
        &self,
        operation: &str,
        target: &Agent,
        text: &str,
        branch: &[Agent],
    ) -> Result<()> {
        use excalibur_journal::instructions::{
            InstructionOrigin, InstructionOriginKind, InstructionScope, InstructionStore,
        };
        let run = target
            .run_id
            .as_ref()
            .ok_or_else(|| invalid("redirect target has no run"))?;
        if target.task.is_empty() {
            return Err(invalid("redirect requires a task-bound execution"));
        }
        let registry = InstructionStore::open(self.inner.db.clone(), Default::default()).await?;
        let origin = InstructionOrigin {
            kind: InstructionOriginKind::Host,
            reference: format!("orchestration-task-redirect:{}", target.task),
        };
        // The expected revision belongs to the durable control intent. Reusing
        // the operation after a cut therefore repeats exactly the registry CAS,
        // rather than reading an already incremented revision and incrementing again.
        let (op, run, reference, task) = (
            operation.to_owned(),
            run.clone(),
            origin.reference.clone(),
            target.task.clone(),
        );
        let(principal,instruction,expected)=self.inner.db.write(Priority::Interactive,move|conn|{
            let principal:Option<String>=conn.query_row("SELECT principal_id FROM runs WHERE id=?1",[&run],|row|row.get(0))?;
            let body:String=conn.query_row("SELECT body FROM orchestration_controls WHERE operation=?1",[&op],|row|row.get(0))?;
            let mut body:Value=serde_json::from_str(&body).map_err(|_|excalibur_store::Error::Invalid("invalid control intent"))?;
            if body["registry_prepared"]==true{
                let id=body["registry_instruction_id"].as_str().map(str::to_owned);
                let expected=body["registry_expected_revision"].as_u64();
                if id.is_some()!=expected.is_some(){return Err(excalibur_store::Error::Invalid("invalid redirect instruction intent"));}
                return Ok((principal,id,expected));
            }
            let old:Option<(String,u64)>=conn.query_row("SELECT i.id,i.revision FROM instructions i JOIN context_instruction_scopes s ON s.instruction_id=i.id WHERE i.principal_id IS ?1 AND s.kind='task' AND s.scope_key=?2 AND json_extract(CASE WHEN json_valid(i.origin) THEN i.origin ELSE '{}' END,'$.reference')=?3 AND i.state='active' ORDER BY i.revision DESC LIMIT 1",rusqlite::params![principal,task,reference],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
            let(id,expected)=old.map_or((None,None),|(id,revision)|(Some(id),Some(revision)));
            body["registry_prepared"]=json!(true);body["registry_instruction_id"]=json!(id);body["registry_expected_revision"]=json!(expected);
            conn.execute("UPDATE orchestration_controls SET body=?2 WHERE operation=?1",rusqlite::params![op,body.to_string()])?;
            Ok((principal,id,expected))
        }).await?;
        let registry_operation = format!("redirect:{}", digest(operation.as_bytes()));
        let record = if let (Some(id), Some(expected)) = (instruction, expected) {
            registry
                .revise(&registry_operation, principal, &id, expected, origin, text)
                .await?
        } else {
            registry
                .register(
                    &registry_operation,
                    principal,
                    InstructionScope::Task(target.task.clone()),
                    origin,
                    text,
                )
                .await?
        };
        // Existing immutable run bindings select the target task. Descendants
        // with independent tasks receive only this branch's instruction by the
        // registry's principal-checked delegation API.
        for child in branch.iter().filter(|agent| agent.id != target.id) {
            let Some(child_run) = &child.run_id else {
                continue;
            };
            if terminal(&self.inner.engine.load(child_run).await?.status) {
                continue;
            }
            let parent = branch
                .iter()
                .find(|agent| Some(agent.id.as_str()) == child.parent_id.as_deref())
                .ok_or_else(|| invalid("redirect branch parent missing"))?;
            let parent_run = parent
                .run_id
                .as_ref()
                .ok_or_else(|| invalid("redirect branch parent has no run"))?;
            registry
                .delegate(parent_run, child_run, std::slice::from_ref(&record.id))
                .await?;
            self.send_message(&format!("redirect:{}:{}",digest(operation.as_bytes()),child.id),&parent.id,&child.id,"directive",json!({"instruction_id":record.id,"instruction_revision":record.revision,"literal":record.literal_text,"action":"replan"}),"interrupt").await?;
        }
        Ok(())
    }
    pub(crate) async fn paused_by(&self, id: &str) -> Result<Option<String>> {
        let id = id.to_owned();
        Ok(self
            .inner
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT paused_by FROM orchestration_agents WHERE id=?1",
                    [id],
                    |r| r.get::<_, Option<String>>(0),
                )?)
            })
            .await?)
    }
    pub(crate) async fn set_pause_owner(
        &self,
        id: &str,
        owner: Option<String>,
        resource: bool,
    ) -> Result<()> {
        let id = id.to_owned();
        self.inner
            .db
            .write(Priority::Interactive, move |c| {
                c.execute(
                    "UPDATE orchestration_agents SET paused_by=?2,resource_paused=?3 WHERE id=?1",
                    rusqlite::params![id, owner, resource],
                )?;
                c.execute("UPDATE runs SET paused_by=?2 WHERE id=(SELECT run_id FROM orchestration_agents WHERE id=?1)",rusqlite::params![id,owner])?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    pub(crate) async fn clear_pause_owner_if(&self, id: &str, owner: &str) -> Result<()> {
        let (id, owner) = (id.to_owned(), owner.to_owned());
        self.inner.db.write(Priority::Interactive,move|c|{let changed=c.execute("UPDATE orchestration_agents SET paused_by=NULL,resource_paused=0 WHERE id=?1 AND paused_by=?2",rusqlite::params![id,owner])?;if changed>0{c.execute("UPDATE runs SET paused_by=NULL WHERE id=(SELECT run_id FROM orchestration_agents WHERE id=?1) AND paused_by=?2",rusqlite::params![id,owner])?;}Ok(())}).await?;
        Ok(())
    }
    pub async fn recover_controls(&self) -> Result<usize> {
        let pending=self.inner.db.read(ReadLane::Interactive,|c|{let mut q=c.prepare("SELECT operation,target,action FROM orchestration_controls WHERE state='pending' ORDER BY rowid")?;Ok(q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        let n = pending.len();
        for (op, target, action) in pending {
            self.control_keyed(&op, &target, serde_json::from_str(&action)?)
                .await?;
        }
        Ok(n)
    }
    /// Tool-only interruption leaves the execution itself running.
    pub async fn interrupt_tool(&self, agent: &str) -> Result<bool> {
        let a = self.agent(agent).await?;
        let run = a
            .run_id
            .ok_or_else(|| invalid("master has no tool session"))?;
        self.authority()
            .ok_or_else(|| invalid("host runtime is not bound"))?
            .interrupt_tool(&run)
            .await
            .map_err(Error::Authority)
    }
}

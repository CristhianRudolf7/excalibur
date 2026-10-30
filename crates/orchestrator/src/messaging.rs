use crate::*;
use excalibur_engine::{Status, WakeEvent};
use excalibur_types::MessageId;
impl Orchestrator {
    pub async fn send_message(
        &self,
        client_id: &str,
        sender: &str,
        recipient: &str,
        kind: &str,
        payload: Value,
        priority: &str,
    ) -> Result<AgentMessage> {
        if client_id.is_empty()
            || client_id.len() > 256
            || !matches!(
                kind,
                "directive" | "question" | "answer" | "report" | "completion" | "blocked"
            )
            || !matches!(priority, "normal" | "interrupt")
        {
            return Err(invalid("mail identity, kind or priority invalid"));
        }
        let from = self.agent(sender).await?;
        let to = self.agent(recipient).await?;
        if from.parent_id.as_deref() != Some(recipient) && to.parent_id.as_deref() != Some(sender) {
            return Err(invalid(
                "agents communicate only with their parent or children",
            ));
        }
        if matches!(kind, "directive" | "answer") && to.parent_id.as_deref() != Some(sender) {
            return Err(invalid("downward message required"));
        }
        if matches!(kind, "question" | "report" | "completion" | "blocked")
            && from.parent_id.as_deref() != Some(recipient)
        {
            return Err(invalid("upward message required"));
        }
        self.validate_durable(&payload)?;
        let encoded = serde_json::to_string(&payload)?;
        if encoded.len() > 64 * 1024 {
            return Err(invalid("mail exceeds 64 KiB"));
        }
        let step_agent = if matches!(kind, "report" | "question" | "completion" | "blocked") {
            &from
        } else {
            &to
        };
        let step = if let Some(run) = &step_agent.run_id {
            self.inner.engine.load(run).await?.step
        } else {
            0
        };
        let message = AgentMessage {
            id: MessageId::new().to_string(),
            client_id: client_id.into(),
            sender: sender.into(),
            recipient: recipient.into(),
            kind: kind.into(),
            payload,
            priority: priority.into(),
            created: now(),
            step,
            state: "pending".into(),
        };
        let control = from
            .run_id
            .as_ref()
            .and_then(|run| {
                client_id
                    .strip_prefix(&format!("{run}:"))
                    .map(|key| (run.clone(), key.to_owned()))
            })
            .filter(|_| {
                kind == "blocked" || kind == "question" && message.payload["blocking"] != false
            })
            .map(|(run, key)| {
                (
                    run,
                    key,
                    if kind == "blocked" { "blocked" } else { "user" }.to_owned(),
                    message
                        .payload
                        .get("reason")
                        .or_else(|| message.payload.get("question"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| message.payload.to_string()),
                )
            });
        let m = message.clone();
        let result=self.inner.db.write(Priority::Journal,move|c|{
            let old:Option<(String,String,String,String,String)>=c.query_row("SELECT m.id,m.recipient,m.kind,m.payload,COALESCE(o.priority,m.priority) FROM orchestration_messages m LEFT JOIN orchestration_message_originals o ON o.id=m.id WHERE m.sender=?1 AND m.client_id=?2",rusqlite::params![m.sender,m.client_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
            if let Some((id,recipient,kind,payload,priority))=old{if recipient!=m.recipient||kind!=m.kind||payload!=encoded||priority!=m.priority{return Err(excalibur_store::Error::Invalid("mail operation reused"));}return Ok(id);}
            c.execute("INSERT INTO orchestration_messages VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'pending')",rusqlite::params![m.id,m.client_id,m.sender,m.recipient,m.kind,encoded,m.priority,m.created,m.step])?;c.execute("INSERT INTO orchestration_message_originals VALUES(?1,?2)",rusqlite::params![m.id,m.priority])?;
            if let Some((run,key,kind,reason))=control{c.execute("INSERT INTO orchestration_agent_controls VALUES(?1,?2,?3,?4,?5,?6)",rusqlite::params![m.id,m.sender,run,key,kind,reason])?;}
            Ok(m.id)
        }).await?;
        self.inner.wake.notify_waiters();
        let persisted = self.message(&result).await?;
        if to.depth == 0 && matches!(kind, "question" | "blocked" | "report") {
            self.notify(
                &format!("mail:{}", persisted.id),
                &from.id,
                if kind == "report" {
                    "informational"
                } else {
                    "urgent"
                },
                &serde_json::to_string(&persisted.payload)?,
                persisted.created,
            )
            .await?;
        }
        Ok(persisted)
    }
    pub async fn message(&self, id: &str) -> Result<AgentMessage> {
        let id = id.to_owned();
        let m=self.inner.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT id,client_id,sender,recipient,kind,payload,priority,created,step,state FROM orchestration_messages WHERE id=?1",[id],mail_row).optional()?)).await?.ok_or(Error::Missing)?;
        decode_mail(m)
    }
    pub async fn mailbox(&self, recipient: &str) -> Result<Vec<AgentMessage>> {
        self.agent(recipient).await?;
        let recipient = recipient.to_owned();
        let rows=self.inner.db.read(ReadLane::Interactive,move|c|{let mut q=c.prepare("SELECT id,client_id,sender,recipient,kind,payload,priority,created,step,state FROM orchestration_messages WHERE recipient=?1 AND state!='acked' ORDER BY created,id")?;Ok(q.query_map([recipient],mail_row)?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        rows.into_iter().map(decode_mail).collect()
    }
    pub async fn ack_message(&self, recipient: &str, id: &str) -> Result<()> {
        let recipient = recipient.to_owned();
        let id = id.to_owned();
        let changed = self
            .inner
            .db
            .write(Priority::Journal, move |c| {
                Ok(c.execute(
                    "UPDATE orchestration_messages SET state='acked' WHERE id=?1 AND recipient=?2",
                    rusqlite::params![id, recipient],
                )?)
            })
            .await?;
        if changed == 0 {
            return Err(Error::Missing);
        }
        Ok(())
    }
    pub(crate) async fn latest_report(&self, agent: &str) -> Result<Option<Value>> {
        let agent = agent.to_owned();
        let row=self.inner.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT payload,step,created FROM orchestration_messages WHERE sender=?1 AND kind IN ('report','completion','blocked') ORDER BY created DESC,id DESC LIMIT 1",[agent],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,i64>(2)?))).optional()?)).await?;
        row.map(|(text,step,created)|Ok(json!({"payload":serde_json::from_str::<Value>(&text)?,"step":step,"created":created}))).transpose()
    }
    /// Deliver into the durable engine context. A directive is acknowledged only
    /// after a subsequent model request, never merely because it was enqueued.
    pub async fn deliver_mailbox(&self, recipient: &str) -> Result<usize> {
        let agent = self.agent(recipient).await?;
        let Some(run) = &agent.run_id else {
            return Ok(0);
        };
        let mut delivered = 0;
        for message in self.mailbox(recipient).await? {
            let state = self.inner.engine.load(run).await?;
            if message.state == "delivered" {
                let events = self.inner.engine.journal().replay(run, 0).await?;
                let applied = events
                    .iter()
                    .find(|e| {
                        matches!(
                            e.kind.as_str(),
                            "directive_applied" | "agent_message_received"
                        ) && e.payload["id"] == message.id
                    })
                    .map(|e| e.sequence);
                if applied.is_some_and(|seq| {
                    events
                        .iter()
                        .any(|e| e.kind == "model_request" && e.sequence > seq)
                }) {
                    self.ack_message(recipient, &message.id).await?;
                }
                continue;
            }
            if terminal(&state.status) {
                continue;
            }
            if state.status == Status::Paused && message.priority == "normal" {
                continue;
            }
            let mut induced = false;
            let delivery_guard = self.inner.gate.lock().await;
            if message.priority == "interrupt"
                && state.status != Status::Paused
                && self.inner.engine.pause_if_active(run, "directive").await?
            {
                self.set_pause_owner(recipient, Some(message.id.clone()), false)
                    .await?;
                induced = true;
            }
            self.clear_child_wait(recipient).await?;
            self.clear_controls_for_message(recipient, &message).await?;
            let text = serde_json::to_string(
                &json!({"mail_id":message.id,"kind":message.kind,"from":message.sender,"content":message.payload}),
            )?;
            if message.kind == "directive" {
                self.inner.engine.directive(run, &message.id, &text).await?;
            } else {
                self.inner
                    .engine
                    .receive_agent_message(
                        run,
                        &message.id,
                        &message.kind,
                        &message.sender,
                        message.payload.clone(),
                    )
                    .await?;
            }
            let id = message.id.clone();
            self.inner.db.write(Priority::Journal,move|c|{c.execute("UPDATE orchestration_messages SET state='delivered' WHERE id=?1 AND state='pending'",[id])?;Ok(())}).await?;
            drop(delivery_guard);
            if induced && self.paused_by(recipient).await?.as_deref() == Some(message.id.as_str()) {
                if self.inner.engine.resume_if_reason(run, "directive").await? {
                    self.clear_pause_owner_if(recipient, &message.id).await?;
                }
            } else if state.status == Status::Waiting
                && self.inner.engine.load(run).await?.status == Status::Waiting
            {
                self.inner
                    .engine
                    .wake(run, WakeEvent::ChildSignal, &message.id)
                    .await?;
            }
            delivered += 1;
        }
        Ok(delivered)
    }
    /// Host calls after persisting the instruction/requirement revision. Scope
    /// changes reach every descendant without relying on an agent forwarding it.
    pub async fn propagate_directive(
        &self,
        origin: &str,
        impact: DirectiveImpact,
    ) -> Result<Vec<AgentMessage>> {
        if impact.text.is_empty() || impact.instruction_revision == 0 {
            return Err(invalid("literal revision required"));
        }
        let source = self.agent(origin).await?;
        let all = self.list_agents().await?;
        let mut affected = Vec::new();
        for a in &all {
            if a.depth == 0 || !is_descendant(a, &source.id, &all) {
                continue;
            }
            if !impact.scopes.is_empty()
                && !impact.scopes.contains(&a.scope)
                && !task_branch_affected(a, &all, &impact.tasks)
            {
                continue;
            }
            if impact.scopes.is_empty()
                && !impact.tasks.is_empty()
                && !task_branch_affected(a, &all, &impact.tasks)
            {
                continue;
            }
            let state = if let Some(run) = &a.run_id {
                Some(self.inner.engine.load(run).await?)
            } else {
                None
            };
            if state.as_ref().is_some_and(|s| terminal(&s.status)) {
                continue;
            }
            let mut intersects = false;
            let mut external = false;
            if let Some(state) = &state {
                for call in state.calls.values().filter(|c| c.result.is_none()) {
                    if let Some(prepared) = &call.prepared {
                        intersects |= prepared.resources.iter().any(|resource| {
                            impact.resources.iter().any(|r| {
                                resource.key == *r
                                    || resource
                                        .key
                                        .strip_prefix(r)
                                        .is_some_and(|tail| tail.starts_with('/'))
                                    || r.strip_prefix(&resource.key)
                                        .is_some_and(|tail| tail.starts_with('/'))
                            })
                        });
                        external |= prepared
                            .resources
                            .iter()
                            .any(|resource| resource.key.starts_with("external:"));
                    }
                }
            }
            let priority =
                if impact.stop || impact.prohibits_current_action || intersects || external {
                    "interrupt"
                } else {
                    "normal"
                };
            // The nearest parent is the authoritative sender on each branch.
            let sender = a
                .parent_id
                .as_deref()
                .ok_or_else(|| invalid("worker parent missing"))?;
            affected.push(self.send_message(&format!("rev:{}:{}:{}:{}",impact.instruction_revision,source.id,a.id,digest(impact.text.as_bytes())),sender,&a.id,"directive",json!({"instruction_revision":impact.instruction_revision,"literal":impact.text,"action":if impact.stop{"stop"}else{"replan"}}),priority).await?);
        }
        Ok(affected)
    }
    pub async fn enforce_directive_deadlines(&self, at: i64) -> Result<Vec<String>> {
        let mut escalated = Vec::new();
        for a in self.list_agents().await? {
            let Some(run) = &a.run_id else { continue };
            let state = self.inner.engine.load(run).await?;
            if state.status != Status::Running {
                continue;
            }
            for m in self
                .mailbox(&a.id)
                .await?
                .into_iter()
                .filter(|m| m.kind == "directive")
            {
                let long_tool = state
                    .calls
                    .values()
                    .any(|c| c.prepared.is_some() && c.result.is_none());
                if state.step.saturating_sub(m.step) >= 2 || at - m.created >= 120 && !long_tool {
                    let id = m.id.clone();
                    self.inner.db.write(Priority::Journal,move|c|{c.execute("UPDATE orchestration_messages SET priority='interrupt',state='pending' WHERE id=?1 AND state!='acked'",[id])?;Ok(())}).await?;
                    self.deliver_mailbox(&a.id).await?;
                    escalated.push(a.id.clone());
                    break;
                }
            }
        }
        Ok(escalated)
    }
}
fn is_descendant(a: &Agent, ancestor: &str, all: &[Agent]) -> bool {
    let mut parent = a.parent_id.as_deref();
    for _ in 0..3 {
        match parent {
            Some(id) if id == ancestor => return true,
            Some(id) => {
                parent = all
                    .iter()
                    .find(|a| a.id == id)
                    .and_then(|a| a.parent_id.as_deref())
            }
            None => return false,
        }
    }
    false
}
type MailRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    u64,
    String,
);
fn mail_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MailRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
    ))
}
fn decode_mail(m: MailRow) -> Result<AgentMessage> {
    Ok(AgentMessage {
        id: m.0,
        client_id: m.1,
        sender: m.2,
        recipient: m.3,
        kind: m.4,
        payload: serde_json::from_str(&m.5)?,
        priority: m.6,
        created: m.7,
        step: m.8,
        state: m.9,
    })
}

fn task_branch_affected(
    agent: &Agent,
    all: &[Agent],
    tasks: &std::collections::BTreeSet<String>,
) -> bool {
    if tasks.contains(&agent.task) {
        return true;
    }
    let mut parent = agent.parent_id.as_deref();
    for _ in 0..3 {
        if let Some(id) = parent
            && let Some(a) = all.iter().find(|a| a.id == id)
        {
            if tasks.contains(&a.task) {
                return true;
            }
            parent = a.parent_id.as_deref();
        } else {
            break;
        }
    }
    false
}

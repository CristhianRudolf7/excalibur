//! Receipt-guarded waits requested by model-callable communication tools.
use crate::*;
impl Orchestrator {
    /// Called at the next step boundary. Unconfirmed tool effects must first be
    /// reconciled; a mailbox side effect alone cannot strand an unreconciled intent.
    pub async fn apply_agent_control(&self, agent: &str) -> Result<bool> {
        let _control = self.inner.gate.lock().await;
        let a = self.agent(agent).await?;
        let Some(run) = a.run_id else {
            return Ok(false);
        };
        let id = a.id.clone();
        let pending=self.inner.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT a.kind,a.reason FROM orchestration_agent_controls a WHERE a.agent_id=?1 AND EXISTS(SELECT 1 FROM journal j WHERE j.run_id=a.run_id AND j.kind='tool_result' AND j.state='confirmed' AND j.idempotency_key=a.tool_key||':result' AND json_extract(j.payload,'$.result.success')=1) ORDER BY CASE a.kind WHEN 'blocked' THEN 0 ELSE 1 END,a.rowid LIMIT 1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?)).await?;
        let Some((kind, reason)) = pending else {
            return Ok(false);
        };
        if self.inner.engine.load(&run).await?.status != Status::Running {
            return Ok(false);
        }
        let applied = if kind == "user" {
            self.inner
                .engine
                .wait(&run, excalibur_engine::WaitReason::User)
                .await
        } else {
            self.inner.engine.block(&run, &reason).await
        };
        if let Err(error) = applied {
            if self.inner.engine.load(&run).await?.status != Status::Running {
                return Ok(false);
            }
            return Err(error.into());
        }
        Ok(true)
    }
    /// Called under the same short actor guard as delivery and wait admission.
    pub(crate) async fn clear_controls_for_message(
        &self,
        agent: &str,
        message: &AgentMessage,
    ) -> Result<()> {
        if message.kind == "directive" {
            return self.clear_agent_controls(agent).await;
        }
        if message.kind != "answer" {
            return Ok(());
        }
        let (agent, question) = (
            agent.to_owned(),
            message
                .payload
                .get("question_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
        self.inner.db.write(Priority::Journal,move|c|{
            if let Some(question)=question {c.execute("DELETE FROM orchestration_agent_controls WHERE agent_id=?1 AND kind='user' AND message_id=?2",rusqlite::params![agent,question])?;c.execute("UPDATE orchestration_question_deadlines SET state='answered' WHERE agent_id=?1 AND message_id=?2",rusqlite::params![agent,question])?;}
            else {c.execute("DELETE FROM orchestration_agent_controls WHERE message_id=(SELECT message_id FROM orchestration_question_deadlines WHERE agent_id=?1 AND state='open' ORDER BY created,message_id LIMIT 1)",[&agent])?;c.execute("UPDATE orchestration_question_deadlines SET state='answered' WHERE message_id=(SELECT message_id FROM orchestration_question_deadlines WHERE agent_id=?1 AND state='open' ORDER BY created,message_id LIMIT 1)",[agent])?;}
            Ok(())
        }).await?;
        Ok(())
    }
    /// Deterministic timeout service; optional defaults require an actual idle
    /// user wait, and mandatory questions never acquire an automatic answer.
    pub async fn service_question_deadlines(&self, at: i64) -> Result<Vec<String>> {
        let pending=self.inner.db.read(ReadLane::Interactive,|c| {
            let mut q=c.prepare("SELECT message_id,agent_id,blocking,default_value,created,next_reminder FROM orchestration_question_deadlines WHERE state='open' ORDER BY created,message_id")?;
            Ok(q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,bool>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)
        }).await?;
        let mut applied = Vec::new();
        for (id, agent, blocking, default, created, reminder) in pending {
            let a = self.agent(&agent).await?;
            let Some(run) = &a.run_id else { continue };
            let state = self.inner.engine.load(run).await?;
            if terminal(&state.status) {
                let id = id.clone();
                self.inner.db.write(Priority::Journal,move|c|{c.execute("UPDATE orchestration_question_deadlines SET state='closed' WHERE message_id=?1",[id])?;Ok(())}).await?;
                continue;
            }
            if blocking && at >= reminder {
                let question = self.message(&id).await?;
                self.notify(&format!("question-reminder:{id}:{}",(at-created)/86400),&agent,"urgent",&json!({"kind":"question_reminder","question_id":id,"question":question.payload["question"],"blocking":true}).to_string(),at).await?;
                let next = created + ((at - created) / 86400 + 1) * 86400;
                let update = id.clone();
                self.inner.db.write(Priority::Journal,move|c|{c.execute("UPDATE orchestration_question_deadlines SET next_reminder=?2 WHERE message_id=?1",rusqlite::params![update,next])?;Ok(())}).await?;
                applied.push(id);
            } else if !blocking
                && at - created >= 900
                && state.status == Status::Waiting
                && state.reason.as_deref() == Some("user")
                && state.calls.values().all(|call| call.result.is_some())
                && state.request.is_none_or(|_| state.response.is_some())
                && let Some(default) = default
            {
                let value: Value = serde_json::from_str(&default)?;
                let parent = a
                    .parent_id
                    .as_ref()
                    .ok_or_else(|| invalid("question parent missing"))?;
                self.inner.engine.journal().append(run,"assumption_recorded",&format!("question-default:{id}"),json!({"question_id":id,"value":value,"reason":"optional question unanswered after fifteen minutes while idle"})).await?;
                self.send_message(
                    &format!("question-default:{id}"),
                    parent,
                    &agent,
                    "answer",
                    json!({"question_id":id,"text":value,"assumption":true}),
                    "normal",
                )
                .await?;
                self.deliver_mailbox(&agent).await?;
                self.notify(
                    &format!("question-default-notice:{id}"),
                    &agent,
                    "normal",
                    &json!({"kind":"assumption","question_id":id,"value":value}).to_string(),
                    at,
                )
                .await?;
                applied.push(id);
            }
        }
        Ok(applied)
    }
    pub(crate) async fn clear_agent_controls(&self, agent: &str) -> Result<()> {
        let agent = agent.to_owned();
        self.inner
            .db
            .write(Priority::Journal, move |c| {
                c.execute("UPDATE orchestration_question_deadlines SET state='superseded' WHERE agent_id=?1 AND state='open'",[&agent])?;
                c.execute(
                    "DELETE FROM orchestration_agent_controls WHERE agent_id=?1",
                    [agent],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}

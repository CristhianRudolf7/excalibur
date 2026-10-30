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
            if let Some(question)=question {c.execute("DELETE FROM orchestration_agent_controls WHERE agent_id=?1 AND kind='user' AND message_id=?2",rusqlite::params![agent,question])?;}
            else {c.execute("DELETE FROM orchestration_agent_controls WHERE message_id=(SELECT message_id FROM orchestration_agent_controls WHERE agent_id=?1 AND kind='user' ORDER BY rowid LIMIT 1)",[agent])?;}
            Ok(())
        }).await?;
        Ok(())
    }
    pub(crate) async fn clear_agent_controls(&self, agent: &str) -> Result<()> {
        let agent = agent.to_owned();
        self.inner
            .db
            .write(Priority::Journal, move |c| {
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

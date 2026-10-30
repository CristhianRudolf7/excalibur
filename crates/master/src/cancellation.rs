//! Human cancellation authorization is separate from model clarification answers.
use crate::{Error, Master, Result};
use excalibur_store::{Database, Priority, ReadLane, rusqlite::OptionalExtension};
use excalibur_types::MessageId;
use serde_json::{Value, json};

pub(super) async fn migrate(db: &Database) -> Result<()> {
    db.write(Priority::Interactive, |conn| {
        let checksum = format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(include_str!("schema_v3.sql").as_bytes())
        );
        let previous: Option<String> = conn
            .query_row(
                "SELECT checksum FROM master_schema WHERE version=3",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            if previous != checksum {
                return Err(excalibur_store::Error::Configuration(
                    "master schema checksum mismatch".into(),
                ));
            }
        } else {
            conn.execute_batch(include_str!("schema_v3.sql"))?;
            conn.execute(
                "INSERT INTO master_schema(version,checksum) VALUES(3,?1)",
                [checksum],
            )?;
        }
        Ok(())
    })
    .await?;
    Ok(())
}
pub(super) fn decision(text: &str) -> Option<bool> {
    match text.trim().to_lowercase().as_str() {
        "yes" | "sí" | "si" | "confirm" | "confirmo" => Some(true),
        "no" | "deny" | "rechazo" => Some(false),
        _ => None,
    }
}
impl Master {
    /// Read-only host guard for the dedicated confirmation port. Neither a
    /// model flag nor an arbitrary operation key constitutes authorization.
    pub async fn authorizes_cancel(
        &self,
        conversation: &str,
        scope: &str,
        target: &str,
        key: &str,
    ) -> Result<bool> {
        self.authorize_conversation(conversation).await?;
        let Some(id) = key.strip_prefix("cancel-confirmation:") else {
            return Ok(false);
        };
        let (id, conversation, scope, target, principal) = (
            id.to_owned(),
            conversation.to_owned(),
            scope.to_owned(),
            target.to_owned(),
            self.inner.config.principal.clone(),
        );
        let answer:Option<String>=self.inner.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT m.text FROM master_cancel_confirmations x JOIN master_questions q ON q.id=x.question_id JOIN master_client_receipts r ON r.message_id=x.answer_message_id JOIN messages m ON m.id=r.message_id WHERE x.question_id=?1 AND x.conversation_id=?2 AND x.scope_id=?3 AND x.target=?4 AND x.principal_id=?5 AND x.state='accepted' AND m.conversation_id=x.conversation_id AND m.scope_id=x.scope_id AND m.principal_id=x.principal_id AND q.answer=m.text",(&id,&conversation,&scope,&target,&principal),|r|r.get(0)).optional()?)).await?;
        Ok(answer.is_some_and(|text| decision(&text) == Some(true)))
    }

    pub(super) async fn is_cancel_question(&self, id: &str) -> Result<bool> {
        let id = id.to_owned();
        Ok(self
            .inner
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM master_cancel_confirmations WHERE question_id=?1)",
                    [id],
                    |r| r.get(0),
                )?)
            })
            .await?)
    }
    pub(super) async fn create_cancel_question(
        &self,
        conversation: &str,
        source: &str,
        result: &Value,
    ) -> Result<()> {
        let target = result["target"]
            .as_str()
            .ok_or_else(|| Error::Invalid("missing cancellation target".into()))?
            .to_owned();
        let target_scope = result["scope"]
            .as_str()
            .ok_or_else(|| Error::Invalid("missing cancellation scope".into()))?
            .to_owned();
        let conversation = conversation.to_owned();
        let source = source.to_owned();
        let principal = self.inner.config.principal.clone();
        self.inner.db.write(Priority::Interactive,move|c| {
            let scope:Option<String>=c.query_row("SELECT m.scope_id FROM messages m JOIN master_client_receipts r ON r.message_id=m.id JOIN conversations v ON v.id=m.conversation_id WHERE m.id=?1 AND m.conversation_id=?2 AND v.principal_id=?3",(&source,&conversation,&principal),|r|r.get(0)).optional()?;
            let Some(scope)=scope else {return Err(excalibur_store::Error::Invalid("cancellation needs a client source"));};
            if scope!=target_scope {return Err(excalibur_store::Error::Invalid("cancellation target is outside the source scope"));}
            let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM master_cancel_confirmations WHERE source_message_id=?1 OR (conversation_id=?2 AND principal_id=?3 AND scope_id=?4 AND target=?5 AND state='pending'))",(&source,&conversation,&principal,&scope,&target),|r|r.get(0))?;
            if exists {return Ok(());}
            let id=MessageId::new().to_string();let text=format!("Cancel {target} and its descendants? Reply yes or no to this question. No work has been cancelled.");
            c.execute("INSERT INTO master_questions(id,conversation_id,scope_id,text,blocking,created_at) VALUES(?1,?2,?3,?4,1,unixepoch())",(&id,&conversation,&scope,&text))?;
            c.execute("INSERT INTO master_question_sources(question_id,source_message_id) VALUES(?1,?2)",(&id,&source))?;
            c.execute("INSERT INTO master_cancel_confirmations(question_id,conversation_id,principal_id,scope_id,target,source_message_id) VALUES(?1,?2,?3,?4,?5,?6)",(&id,&conversation,&principal,&scope,&target,&source))?;
            c.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'question',?2)",(&conversation,json!({"id":id,"text":text,"scope":scope,"blocking":true,"action":"cancel","target":target}).to_string()))?;
            Ok(())
        }).await?;
        self.inner.wake.notify_waiters();
        Ok(())
    }
    /// Only a durable client receipt can authorize; role=user is insufficient.
    pub(super) async fn route_cancel_source(
        &self,
        conversation: &str,
        source: &str,
    ) -> Result<bool> {
        let conversation = conversation.to_owned();
        let source = source.to_owned();
        let principal = self.inner.config.principal.clone();
        let consumed=self.inner.db.write(Priority::Interactive,move|c| {
            let row:Option<(String,String,String,String,String,u64)>=c.query_row("SELECT i.state,m.text,m.scope_id,i.preanalysis,m.attachments,r.cursor FROM master_inbox i JOIN messages m ON m.id=i.message_id JOIN master_client_receipts r ON r.message_id=m.id JOIN conversations v ON v.id=m.conversation_id WHERE m.id=?1 AND m.conversation_id=?2 AND v.principal_id=?3",(&source,&conversation,&principal),|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
            let Some((state,text,scope,analysis,attachments,cursor))=row else {return Ok(false);};
            if state!="routing" {return Ok(false);}
            let analysis:crate::Preanalysis=serde_json::from_str(&analysis).map_err(|_|excalibur_store::Error::Invalid("invalid intake analysis"))?;
            let mut q=c.prepare("SELECT x.question_id FROM master_cancel_confirmations x JOIN master_questions q ON q.id=x.question_id WHERE x.conversation_id=?1 AND x.principal_id=?2 AND x.scope_id=?3 AND x.state='pending' AND q.answer IS NULL AND EXISTS(SELECT 1 FROM master_events e WHERE e.kind='question' AND json_extract(e.payload,'$.id')=x.question_id AND e.sequence<?4)")?;
            let ids=q.query_map((&conversation,&principal,&scope,cursor),|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let reply_cancel=if let Some(reply)=&analysis.reply_to {c.query_row("SELECT EXISTS(SELECT 1 FROM master_cancel_confirmations WHERE question_id=?1)",[reply],|r|r.get::<_,bool>(0))?}else{false};
            if analysis.reply_to.is_some() && !reply_cancel {return Ok(false);}
            if analysis.command.is_some() && !reply_cancel {return Ok(false);}
            if ids.is_empty() && !reply_cancel {return Ok(false);}
            // A bare yes is safe only if exactly one question of any kind is pending.
            let count:u64=c.query_row("SELECT COUNT(*) FROM master_questions WHERE conversation_id=?1 AND scope_id=?2 AND answer IS NULL",(&conversation,&scope),|r|r.get(0))?;
            let selected=if let Some(reply)=&analysis.reply_to {ids.iter().find(|id|*id==reply)}else if ids.len()==1 && count==1 {ids.first()}else{None};
            let explicit=decision(&text);
            if let (Some(id),Some(yes))=(selected,explicit) && attachments=="[]" {
                c.execute("UPDATE master_questions SET answer=?2 WHERE id=?1 AND answer IS NULL",(id,&text))?;
                c.execute("UPDATE master_cancel_confirmations SET state=?2,answer_message_id=?3 WHERE question_id=?1 AND state='pending'",(id,if yes{"accepted"}else{"rejected"},&source))?;
                c.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'cancellation_answered',?2)",(&conversation,json!({"id":id,"accepted":yes,"source_message_id":source}).to_string()))?;
                if !yes {record_response(c,&conversation,&scope,&principal,"Cancellation declined. No work has been cancelled.")?;}
            } else {
                c.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'cancellation_selection_required',?2)",(&conversation,json!({"source_message_id":source,"question_ids":ids,"reason":"Reply yes or no to one pending cancellation question in this conversation and scope."}).to_string()))?;
            }
            c.execute("UPDATE master_inbox SET state='consumed' WHERE message_id=?1 AND state='routing'",[&source])?;
            Ok(true)
        }).await?;
        if consumed {
            self.drain_cancel_confirmations().await?;
            self.inner.wake.notify_waiters();
        }
        Ok(consumed)
    }
    /// Accepted decisions form a durable outbox. The host uses one orchestrator
    /// operation key on every retry, including after effect-before-receipt crashes.
    pub(super) async fn drain_cancel_confirmations(&self) -> Result<()> {
        let principal = self.inner.config.principal.clone();
        let pending=self.inner.db.read(ReadLane::Interactive,move|c|{
            let mut q=c.prepare("SELECT x.question_id,x.conversation_id,x.scope_id,x.target FROM master_cancel_confirmations x JOIN master_client_receipts r ON r.message_id=x.answer_message_id JOIN messages m ON m.id=r.message_id JOIN conversations v ON v.id=x.conversation_id WHERE x.state='accepted' AND x.principal_id=?1 AND v.principal_id=?1 AND m.conversation_id=x.conversation_id AND m.scope_id=x.scope_id ORDER BY x.question_id LIMIT 128")?;
            Ok(q.query_map([principal],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)
        }).await?;
        for (id, conversation, scope, target) in pending {
            let key = format!("cancel-confirmation:{id}");
            let result = match tokio::time::timeout(
                self.inner.config.fast_tool_timeout,
                self.inner
                    .actions
                    .confirm_cancel(&conversation, &scope, &target, &key),
            )
            .await
            {
                Ok(Ok(result)) => result,
                // Leave the accepted decision durable for the next maintenance tick.
                _ => continue,
            };
            let result = self.inner.redactor.redact_json(&result);
            self.inner.db.write(Priority::Interactive,move|c|{
                if c.execute("UPDATE master_cancel_confirmations SET state='done',result=?2 WHERE question_id=?1 AND state='accepted'",(&id,result.to_string()))?==1 {
                    c.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'cancellation_completed',?2)",(&conversation,json!({"id":id,"target":target,"scope":scope}).to_string()))?;
                    let principal:String=c.query_row("SELECT principal_id FROM master_cancel_confirmations WHERE question_id=?1",[&id],|r|r.get(0))?;
                    record_response(c,&conversation,&scope,&principal,&format!("Cancellation processed for {target}."))?;
                }Ok(())
            }).await?;
        }
        Ok(())
    }
}

fn record_response(
    c: &excalibur_store::rusqlite::Connection,
    conversation: &str,
    scope: &str,
    principal: &str,
    text: &str,
) -> excalibur_store::Result<()> {
    let message = MessageId::new().to_string();
    c.execute("INSERT INTO messages(id,conversation_id,role,text,scope_id,principal_id) VALUES(?1,?2,'assistant',?3,?4,?5)",(&message,conversation,text,scope,principal))?;
    c.execute(
        "INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'assistant_message',?2)",
        (
            conversation,
            json!({"message_id":message,"text":text,"scope":scope}).to_string(),
        ),
    )?;
    Ok(())
}

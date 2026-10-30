use crate::{Error, Master, Result};
use excalibur_store::{Priority, ReadLane, rusqlite::OptionalExtension};
use excalibur_types::MessageId;
use excalibur_vault::Redactor;
use serde::{Deserialize, Serialize};
use serde_json::json;
pub(super) enum QuestionRoute {
    Continue,
    Consumed(Option<String>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingQuestion {
    pub id: String,
    pub conversation: String,
    pub text: String,
    pub blocking: bool,
    pub default_answer: Option<String>,
    pub created_at: i64,
    pub answer: Option<String>,
    pub applied_default: bool,
}
impl Master {
    pub async fn ask(
        &self,
        conversation: &str,
        text: &str,
        blocking: bool,
        default_answer: Option<String>,
        now: i64,
    ) -> Result<String> {
        let scope = self.conversation_scope(conversation).await?;
        self.ask_scoped(conversation, &scope, text, blocking, default_answer, now)
            .await
    }
    pub async fn ask_scoped(
        &self,
        conversation: &str,
        scope: &str,
        text: &str,
        blocking: bool,
        default_answer: Option<String>,
        now: i64,
    ) -> Result<String> {
        self.ask_for_source_inner(
            conversation,
            scope,
            None,
            text,
            blocking,
            default_answer,
            now,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn ask_for_source(
        &self,
        conversation: &str,
        scope: &str,
        source_message_id: &str,
        text: &str,
        blocking: bool,
        default_answer: Option<String>,
        now: i64,
    ) -> Result<String> {
        self.ask_for_source_inner(
            conversation,
            scope,
            Some(source_message_id),
            text,
            blocking,
            default_answer,
            now,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn ask_for_source_inner(
        &self,
        conversation: &str,
        scope: &str,
        source: Option<&str>,
        text: &str,
        blocking: bool,
        default_answer: Option<String>,
        now: i64,
    ) -> Result<String> {
        crate::validate_scope(scope)?;
        if self.inner.redactor.redact_text(scope) != scope {
            return Err(Error::Invalid("secret literal in question scope".into()));
        }
        let source = source.map(str::to_string);
        let scope = scope.to_string();
        self.authorize_conversation(conversation).await?;
        if text.is_empty()
            || text.len() > 8192
            || default_answer.as_ref().is_some_and(|s| s.len() > 8192)
        {
            return Err(Error::Invalid("invalid question size".into()));
        }
        let id = MessageId::new().to_string();
        let saved = id.clone();
        let conversation = conversation.to_string();
        let text = self.inner.redactor.redact_text(text);
        let default_answer = default_answer.map(|s| self.inner.redactor.redact_text(&s));
        self.inner.db.write(Priority::Interactive,move|conn|{let source=source.map(|source|conn.query_row("SELECT client_message_id FROM master_user_sources WHERE id=?1",[&source],|r|r.get::<_,String>(0)).optional().map(|literal|literal.unwrap_or(source))).transpose()?;if let Some(source)=&source {let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1 AND conversation_id=?2 AND scope_id=?3)",(source,&conversation,&scope),|r|r.get(0))?;if !valid{return Err(excalibur_store::Error::Invalid("question source does not belong to this scope"));}let count:u32=conn.query_row("SELECT COUNT(*) FROM master_question_sources WHERE source_message_id=?1",[source],|r|r.get(0))?;if count>=3{return Err(excalibur_store::Error::Invalid("at most three questions per source message"));}}conn.execute("INSERT INTO master_questions(id,conversation_id,scope_id,text,blocking,default_answer,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",(&saved,&conversation,&scope,&text,blocking,&default_answer,now))?;if let Some(source)=&source {conn.execute("INSERT INTO master_question_sources(question_id,source_message_id) VALUES(?1,?2)",(&saved,source))?;}conn.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'question',?2)",(&conversation,json!({"id":saved,"text":text,"blocking":blocking,"default_answer":default_answer}).to_string()))?;Ok(())}).await?;
        Ok(id)
    }
    pub async fn answer_question(&self, conversation: &str, id: &str, answer: &str) -> Result<()> {
        self.authorize_conversation(conversation).await?;
        if self.is_cancel_question(id).await? {
            return Err(Error::Invalid(
                "cancellation requires an authenticated client message".into(),
            ));
        }
        if answer.is_empty() || answer.len() > 8192 {
            return Err(Error::Invalid("invalid answer size".into()));
        }
        let conversation_owned = conversation.to_string();
        let id = id.to_string();
        let answer = self.inner.redactor.redact_text(answer);
        let principal = self.inner.config.principal.clone();
        let redactor = self.inner.redactor.clone();
        let scope = self
            .inner
            .db
            .write(Priority::Interactive, move |conn| {
                apply_answer(
                    conn,
                    AnswerInput {
                        conversation: &conversation_owned,
                        id: &id,
                        answer: &answer,
                        principal: &principal,
                        source: None,
                        redactor: &redactor,
                    },
                )
            })
            .await?;
        self.wake_question_scope(conversation, &scope).await
    }
    pub(super) async fn wake_question_scope(&self, conversation: &str, scope: &str) -> Result<()> {
        if self.conversation_scope(conversation).await? == scope
            && !self
                .inner
                .unavailable
                .load(std::sync::atomic::Ordering::Acquire)
        {
            self.schedule(conversation, true);
        }
        self.inner.wake.notify_waiters();
        Ok(())
    }
    pub(super) async fn release_intake(&self, source: &str) -> Result<()> {
        let source = source.to_string();
        self.inner.db.write(Priority::Interactive,move|conn|{conn.execute("UPDATE master_inbox SET state='pending' WHERE message_id=?1 AND state='routing'",[&source])?;Ok(())}).await?;
        Ok(())
    }
    pub(super) async fn route_question_source(
        &self,
        conversation: &str,
        source: &str,
    ) -> Result<QuestionRoute> {
        let conversation = conversation.to_string();
        let source = source.to_string();
        let principal = self.inner.config.principal.clone();
        let redactor = self.inner.redactor.clone();
        Ok(self.inner.db.write(Priority::Interactive,move|conn| {
            let (state,text,analysis,scope,attachments,cursor):(String,String,String,String,String,u64)=conn.query_row("SELECT i.state,m.text,i.preanalysis,m.scope_id,m.attachments,r.cursor FROM master_inbox i JOIN messages m ON m.id=i.message_id JOIN conversations c ON c.id=m.conversation_id JOIN master_client_receipts r ON r.message_id=m.id WHERE m.id=?1 AND m.conversation_id=?2 AND c.principal_id=?3",(&source,&conversation,&principal),|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)))?;
            if state!="routing" {return Ok(QuestionRoute::Consumed(None));}
            let analysis:crate::Preanalysis=serde_json::from_str(&analysis).map_err(|_|excalibur_store::Error::Invalid("invalid durable intake analysis"))?;
            let mut candidates=Vec::new();let mut unmatched=false;
            if let Some(reply)=&analysis.reply_to {
                let question:Option<(String,String,bool,Option<String>,bool)>=conn.query_row("SELECT q.id,q.scope_id,q.blocking,q.answer,EXISTS(SELECT 1 FROM master_events e WHERE e.conversation_id=q.conversation_id AND e.kind='question' AND json_extract(e.payload,'$.id')=q.id AND e.sequence<?3) FROM master_questions q WHERE q.id=?1 AND q.conversation_id=?2",(reply,&conversation,cursor),|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional()?;
                if let Some((id,question_scope,blocking,None,true))=question {candidates.push((id,question_scope,blocking));}
                else {
                    let normal:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1 AND conversation_id=?2)",(reply,&conversation),|row|row.get(0))?;
                    if normal {return Ok(QuestionRoute::Continue);}unmatched=true;
                }
            } else if analysis.command.is_none() {
                let mut query=conn.prepare("SELECT q.id,q.scope_id,q.blocking FROM master_questions q WHERE q.conversation_id=?1 AND q.scope_id=?2 AND q.answer IS NULL AND EXISTS(SELECT 1 FROM master_events e WHERE e.conversation_id=q.conversation_id AND e.kind='question' AND json_extract(e.payload,'$.id')=q.id AND e.sequence<?3) ORDER BY q.created_at,q.id")?;
                candidates=query.query_map((&conversation,&scope,cursor),|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,bool>(2)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
                if !candidates.iter().any(|candidate|candidate.2) {return Ok(QuestionRoute::Continue);}
            } else {return Ok(QuestionRoute::Continue);}
            let earlier:u64=conn.query_row("SELECT count(*) FROM master_inbox i JOIN messages m ON m.id=i.message_id WHERE i.conversation_id=?1 AND m.scope_id=?2 AND i.message_id!=?3 AND i.state IN ('pending','processing','unavailable','context_blocked','routing') AND i.created_seq<?4",(&conversation,&scope,&source,cursor),|row|row.get(0))?;
            let valid_text=!text.trim().is_empty()&&text.len()<=8192&&attachments=="[]";
            if !unmatched && candidates.len()==1 && valid_text && (analysis.reply_to.is_some() || earlier==0) {
                let (id,_,_)=&candidates[0];
                let scope=apply_answer(conn,AnswerInput{conversation:&conversation,id,answer:&redactor.redact_text(&text),principal:&principal,source:Some(&source),redactor:&redactor})?;
                return Ok(QuestionRoute::Consumed(Some(scope)));
            }
            let kind=if unmatched {"clarification_not_matched"}else{"clarification_selection_required"};
    let payload=redactor.redact_json(&json!({"source_message_id":source,"reply_to":analysis.reply_to,"question_ids":candidates.iter().map(|candidate|&candidate.0).collect::<Vec<_>>(),"reason":if unmatched {"The reply does not identify a pending question in this conversation."}else if !valid_text {"Clarification answers require 1..8192 text bytes without attachments."}else {"Select the pending question explicitly; no answer was guessed."}}));
            conn.execute("UPDATE master_inbox SET state='consumed' WHERE message_id=?1 AND state='routing'",[&source])?;
            conn.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,?2,?3)",(&conversation,kind,payload.to_string()))?;
            Ok(QuestionRoute::Consumed(None))
        }).await?)
    }
    pub(super) async fn route_recovered_questions(&self, conversation: &str) -> Result<()> {
        let conversation_owned = conversation.to_string();
        let sources=self.inner.db.read(ReadLane::Interactive,move|conn|{let mut query=conn.prepare("SELECT i.message_id,r.client_id,r.client_msg_id FROM master_inbox i JOIN messages m ON m.id=i.message_id JOIN master_client_receipts r ON r.message_id=m.id WHERE i.conversation_id=?1 AND i.state='routing' AND m.scope_id=(SELECT COALESCE(active_scope,'general') FROM conversations WHERE id=?1) ORDER BY i.created_seq LIMIT 128")?;Ok(query.query_map([&conversation_owned],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        for (source, client, message) in sources {
            let Ok(_intake_guard) = self
                .intake_mutex(&crate::intake_key(&client, &message))
                .try_lock_owned()
            else {
                continue;
            };
            if self.route_cancel_source(conversation, &source).await? {
                continue;
            }
            if matches!(
                self.route_question_source(conversation, &source).await?,
                QuestionRoute::Continue
            ) {
                let source_owned = source.clone();
                let analysis =
                    self.inner
                        .db
                        .read(ReadLane::Interactive, move |conn| {
                            let encoded: String = conn.query_row(
                                "SELECT preanalysis FROM master_inbox WHERE message_id=?1",
                                [&source_owned],
                                |row| row.get(0),
                            )?;
                            Ok(serde_json::from_str::<crate::Preanalysis>(&encoded)
                                .unwrap_or_default())
                        })
                        .await?;
                if let Some(command) = analysis.command {
                    self.dispatch_command(conversation, &source, &command)
                        .await?;
                } else {
                    self.release_intake(&source).await?;
                }
            }
        }
        Ok(())
    }
    /// Called by the host's bounded maintenance tick. Mandatory/irreversible
    /// questions never apply defaults; reminders are emitted at most once/day.
    pub async fn tick_questions(&self, now: i64) -> Result<Vec<PendingQuestion>> {
        let principal = self.inner.config.principal.clone();
        let busy = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let ready=self.inner.db.write(Priority::Interactive,move|conn|{
            let rows={let mut q=conn.prepare("SELECT q.id,q.conversation_id,q.text,q.blocking,q.default_answer,q.created_at,q.reminded_at,q.scope_id FROM master_questions q JOIN conversations c ON c.id=q.conversation_id WHERE q.answer IS NULL AND c.principal_id=?1")?;q.query_map([&principal],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,bool>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,i64>(5)?,r.get::<_,Option<i64>>(6)?,r.get::<_,String>(7)?)))?.collect::<std::result::Result<Vec<_>,_>>()?};
            let mut ready=Vec::new();for (id,conversation,text,blocking,default_answer,created_at,reminded_at,scope) in rows {
                if !blocking && !busy.contains(&conversation) && now.saturating_sub(created_at)>=900 {if let Some(answer)=&default_answer {conn.execute("UPDATE master_questions SET answer=?2,applied_default=1 WHERE id=?1 AND answer IS NULL",(&id,answer))?;let message_id=MessageId::new().to_string();
                    conn.execute("INSERT INTO messages(id,conversation_id,role,text,scope_id,principal_id) VALUES(?1,?2,'user',?3,?4,?5)",(&message_id,&conversation,format!("Recorded nonblocking assumption after the clarification deadline. Question: {text}\nAssumed answer: {answer}"),&scope,&principal))?;
                    conn.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'question_default_applied',?2)",(&conversation,json!({"id":id,"answer":answer,"assumption":true,"message_id":message_id}).to_string()))?;
                    let cursor=conn.last_insert_rowid();conn.execute("INSERT INTO master_inbox(message_id,conversation_id,preanalysis,created_seq) VALUES(?1,?2,'{}',?3)",(&message_id,&conversation,cursor))?;
                    ready.push(PendingQuestion{id,conversation,text,blocking,default_answer:default_answer.clone(),created_at,answer:Some(answer.clone()),applied_default:true});}}
                else if blocking&&now.saturating_sub(reminded_at.unwrap_or(created_at))>=86400 {conn.execute("UPDATE master_questions SET reminded_at=?2 WHERE id=?1",(&id,now))?;conn.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'question_reminder',?2)",(&conversation,json!({"id":id,"text":text,"blocking":true}).to_string()))?;}
            }Ok(ready)
        }).await?;
        for question in &ready {
            if !self
                .inner
                .unavailable
                .load(std::sync::atomic::Ordering::Acquire)
            {
                self.schedule(&question.conversation, true);
            }
        }
        self.inner.wake.notify_waiters();
        Ok(ready)
    }
    pub async fn questions(&self, conversation: &str) -> Result<Vec<PendingQuestion>> {
        self.authorize_conversation(conversation).await?;
        let conversation = conversation.to_string();
        let mut questions=self.inner.db.read(ReadLane::Interactive,move|conn|{let mut q=conn.prepare("SELECT id,conversation_id,text,blocking,default_answer,created_at,answer,applied_default FROM master_questions WHERE conversation_id=?1 ORDER BY created_at,id")?;Ok(q.query_map([&conversation],|r|Ok(PendingQuestion{id:r.get(0)?,conversation:r.get(1)?,text:r.get(2)?,blocking:r.get(3)?,default_answer:r.get(4)?,created_at:r.get(5)?,answer:r.get(6)?,applied_default:r.get(7)?}))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        for question in &mut questions {
            question.text = self.inner.redactor.redact_text(&question.text);
            question.default_answer = question
                .default_answer
                .as_ref()
                .map(|text| self.inner.redactor.redact_text(text));
            question.answer = question
                .answer
                .as_ref()
                .map(|text| self.inner.redactor.redact_text(text));
        }
        Ok(questions)
    }
}

struct AnswerInput<'a> {
    conversation: &'a str,
    id: &'a str,
    answer: &'a str,
    principal: &'a str,
    source: Option<&'a str>,
    redactor: &'a Redactor,
}
fn apply_answer(
    conn: &excalibur_store::rusqlite::Connection,
    input: AnswerInput<'_>,
) -> excalibur_store::Result<String> {
    let AnswerInput {
        conversation,
        id,
        answer,
        principal,
        source,
        redactor,
    } = input;
    let previous: Option<(Option<String>, String, String)> = conn
        .query_row(
            "SELECT answer,scope_id,text FROM master_questions WHERE id=?1 AND conversation_id=?2",
            (id, conversation),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (scope, question) = match previous {
        Some((None, scope, question)) => (scope, redactor.redact_text(&question)),
        Some((Some(previous), scope, _)) if previous == answer => return Ok(scope),
        _ => {
            return Err(excalibur_store::Error::Invalid(
                "unknown or already answered question",
            ));
        }
    };
    conn.execute(
        "UPDATE master_questions SET answer=?2 WHERE id=?1 AND answer IS NULL",
        (id, answer),
    )?;
    let message_id = MessageId::new().to_string();
    let text = format!("Clarification question: {question}\nUser answer: {answer}");
    conn.execute("INSERT INTO messages(id,conversation_id,role,text,scope_id,principal_id,reply_to) VALUES(?1,?2,'user',?3,?4,?5,?6)",(&message_id,conversation,&text,&scope,principal,source))?;
    conn.execute("INSERT INTO master_answer_literals(message_id,source_message_id,literal_text) VALUES(?1,?2,?3)",(&message_id,source,answer))?;
    let payload=redactor.redact_json(&json!({"id":id,"answer":answer,"message_id":message_id,"source_message_id":source,"scope":scope}));
    conn.execute(
        "INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'question_answered',?2)",
        (conversation, payload.to_string()),
    )?;
    let cursor = conn.last_insert_rowid();
    conn.execute("INSERT INTO master_inbox(message_id,conversation_id,preanalysis,created_seq) VALUES(?1,?2,'{}',?3)",(&message_id,conversation,cursor))?;
    if let Some(source) = source {
        conn.execute(
            "UPDATE messages SET scope_id=?2 WHERE id=?1 AND conversation_id=?3",
            (source, &scope, conversation),
        )?;
        conn.execute("UPDATE master_inbox SET state='consumed' WHERE message_id=?1 AND conversation_id=?2 AND state='routing'",(source,conversation))?;
    }
    Ok(scope)
}

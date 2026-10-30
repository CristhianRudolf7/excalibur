use crate::{Master, Result};
use excalibur_store::{Database, Priority, ReadLane};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientSource {
    pub id: String,
    pub client_message_id: String,
    pub text: String,
    pub scope_id: String,
    pub conversation_id: String,
}
pub(super) async fn migrate(db: &Database) -> Result<()> {
    db.write(Priority::Interactive, |conn| {
        let checksum = format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(include_str!("schema_v2.sql").as_bytes())
        );
        use excalibur_store::rusqlite::OptionalExtension;
        let previous: Option<String> = conn
            .query_row(
                "SELECT checksum FROM master_schema WHERE version=2",
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
            return Ok(());
        }
        conn.execute_batch(include_str!("schema_v2.sql"))?;
        conn.execute(
            "INSERT INTO master_schema(version,checksum) VALUES(2,?1)",
            [checksum],
        )?;
        Ok(())
    })
    .await?;
    Ok(())
}
impl Master {
    pub async fn client_sources(
        &self,
        conversation: &str,
        ids: &[String],
    ) -> Result<Vec<ClientSource>> {
        self.authorize_conversation(conversation).await?;
        let conversation = conversation.to_string();
        let ids = ids.to_vec();
        Ok(self.inner.db.read(ReadLane::Interactive,move|conn| {
            let mut result=Vec::new();let mut query=conn.prepare("SELECT id,client_message_id,text,scope_id,conversation_id FROM master_user_sources WHERE conversation_id=?1 AND id=?2")?;
            for id in ids {let mut rows=query.query((&conversation,&id))?;if let Some(row)=rows.next()? {result.push(ClientSource{id:row.get(0)?,client_message_id:row.get(1)?,text:row.get(2)?,scope_id:row.get(3)?,conversation_id:row.get(4)?});}}Ok(result)
        }).await?)
    }
}
impl Master {
    pub(super) async fn record_outage(&self, conversation: &str) -> Result<()> {
        self.inner
            .unavailable
            .store(true, std::sync::atomic::Ordering::Release);
        let conversation = conversation.to_string();
        self.inner.db.write(Priority::Interactive,move|conn| {
            let changed=conn.execute("UPDATE master_health SET unavailable=1 WHERE id=1 AND unavailable=0",[])?;
            if changed>0 {conn.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'model_unavailable',?2)",(&conversation,r#"{"text":"Received; no model is available. Messages remain queued until service returns."}"#))?;}Ok(())
        }).await?;
        Ok(())
    }
    pub async fn publish_link(
        &self,
        id: &str,
        conversation: &str,
        content: serde_json::Value,
    ) -> Result<u64> {
        self.publish_notice(id, conversation, content, "informational")
            .await
    }
    /// Flush fixed five-second buckets into one literal template per scope.
    pub async fn flush_notices(&self, now: i64) -> Result<usize> {
        self.flush_notices_inner(now,None::<fn(&excalibur_store::rusqlite::Connection,&str)->excalibur_store::Result<bool>>).await
    }
    /// Host guards each pending member inside the grouped publication writer.
    /// Suppressed members remain durable and pending without being acknowledged.
    pub async fn flush_notices_checked(
        &self,
        now: i64,
        check: impl Fn(&excalibur_store::rusqlite::Connection, &str) -> excalibur_store::Result<bool>
        + Send
        + 'static,
    ) -> Result<usize> {
        self.flush_notices_inner(now, Some(check)).await
    }
    async fn flush_notices_inner(
        &self,
        now: i64,
        check: Option<
            impl Fn(&excalibur_store::rusqlite::Connection, &str) -> excalibur_store::Result<bool>
            + Send
            + 'static,
        >,
    ) -> Result<usize> {
        let principal = self.inner.config.principal.clone();
        let inner = self.inner.clone();
        let count=self.inner.db.write(Priority::Interactive,move|conn| {
            // Admission and grouped publication share the same short guard.
            // Due rows for occupied conversations remain untouched.
            let active=inner.active.lock().unwrap_or_else(|e|e.into_inner());
            let groups=crate::notice_flush::groups(conn,&principal,now)?;
            let mut published=0;
            for group in &groups {
                let (conversation,scope,due)=(&group.conversation,&group.scope,&group.due);
                if active.contains_key(conversation) {continue;}
                let mut eligible=Vec::new();
                for member in crate::notice_flush::members(conn,&principal,group)? {
                    let allowed=match crate::notice_guards::origin(conn,&member.0)? {
                        None=>true,
                        Some(origin)=>match &check {None=>false,Some(check)=>check(conn,&origin)?},
                    };
                    if allowed {eligible.push(member);}
                }
                let members=eligible;
                if members.is_empty() {continue;}
                let text=members.iter().map(|(_,content)|crate::templates::notice(&serde_json::from_str(content).unwrap_or_default())).collect::<Vec<_>>().join("\n");
                let id=excalibur_types::MessageId::new().to_string();
                conn.execute("INSERT INTO messages(id,conversation_id,role,text,scope_id,principal_id) VALUES(?1,?2,'assistant',?3,?4,?5)",(&id,conversation,&text,scope,&principal))?;
                conn.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'notification_group',?2)",(conversation,serde_json::json!({"message_id":id,"text":text,"scope":scope,"members":members.iter().map(|(id,_)|id).collect::<Vec<_>>(),"bucket":due}).to_string()))?;
                published+=1;
                for (notice,_) in members {conn.execute("UPDATE master_notice_outbox SET flushed=1 WHERE notice_id=?1",[notice])?;}
                crate::notice_flush::finish(conn,&principal,group)?;
            }Ok(published)
        }).await?;
        self.inner.wake.notify_waiters();
        Ok(count)
    }
}

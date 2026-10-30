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
        let principal = self.inner.config.principal.clone();
        let inner = self.inner.clone();
        let count=self.inner.db.write(Priority::Interactive,move|conn| {
            // Admission and grouped publication share the same short guard.
            // Due rows for occupied conversations remain untouched.
            let active=inner.active.lock().unwrap_or_else(|e|e.into_inner());
            let mut groups={let mut q=conn.prepare("SELECT conversation_id,scope_id,due_at FROM master_notice_outbox WHERE flushed=0 AND due_at<=?1 GROUP BY conversation_id,scope_id,due_at ORDER BY due_at,conversation_id,scope_id LIMIT 128")?;q.query_map([now],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?.collect::<std::result::Result<Vec<_>,_>>()?};
            groups.retain(|(conversation,_,_)|!active.contains_key(conversation));
            for (conversation,scope,due) in &groups {
                let members={let mut q=conn.prepare("SELECT notice_id,content FROM master_notice_outbox WHERE conversation_id=?1 AND scope_id=?2 AND due_at=?3 AND flushed=0 ORDER BY notice_id LIMIT 32")?;q.query_map((conversation,scope,due),|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?};
                let text=members.iter().map(|(_,content)|crate::templates::notice(&serde_json::from_str(content).unwrap_or_default())).collect::<Vec<_>>().join("\n");
                let id=excalibur_types::MessageId::new().to_string();
                conn.execute("INSERT INTO messages(id,conversation_id,role,text,scope_id,principal_id) VALUES(?1,?2,'assistant',?3,?4,?5)",(&id,conversation,&text,scope,&principal))?;
                conn.execute("INSERT INTO master_events(conversation_id,kind,payload) VALUES(?1,'notification_group',?2)",(conversation,serde_json::json!({"message_id":id,"text":text,"scope":scope,"members":members.iter().map(|(id,_)|id).collect::<Vec<_>>(),"bucket":due}).to_string()))?;
                for (notice,_) in members {conn.execute("UPDATE master_notice_outbox SET flushed=1 WHERE notice_id=?1",[notice])?;}
            }Ok(groups.len())
        }).await?;
        self.inner.wake.notify_waiters();
        Ok(count)
    }
}

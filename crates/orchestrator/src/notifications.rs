use crate::*;
use excalibur_types::MessageId;
impl Orchestrator {
    pub async fn set_notification_policy(
        &self,
        conversation: &str,
        policy: NotificationPolicy,
    ) -> Result<()> {
        if policy.quiet_start_minute.is_some_and(|m| m >= 1440)
            || policy.quiet_end_minute.is_some_and(|m| m >= 1440)
            || policy.utc_offset_minutes.unsigned_abs() > 1440
        {
            return Err(invalid("invalid quiet hours"));
        }
        let conversation = conversation.to_owned();
        let text = serde_json::to_string(&policy)?;
        self.inner.db.write(Priority::Interactive,move|c|{c.execute("INSERT INTO orchestration_notify_policy VALUES(?1,?2) ON CONFLICT(conversation) DO UPDATE SET body=excluded.body",rusqlite::params![conversation,text])?;Ok(())}).await?;
        Ok(())
    }
    pub async fn connection_activity(
        &self,
        conversation: &str,
        channel: &str,
        connected: bool,
        at: i64,
    ) -> Result<()> {
        if conversation.is_empty() || channel.is_empty() {
            return Err(invalid("conversation/channel required"));
        }
        let conversation = conversation.to_owned();
        let channel = channel.to_owned();
        self.inner.db.write(Priority::Interactive,move|c|{c.execute("INSERT INTO orchestration_activity VALUES(?1,?2,?3,?4) ON CONFLICT(conversation) DO UPDATE SET channel=excluded.channel,connected=excluded.connected,last_active=excluded.last_active",rusqlite::params![conversation,channel,connected,at])?;Ok(())}).await?;
        Ok(())
    }
    pub async fn notify(
        &self,
        operation: &str,
        agent: &str,
        class: &str,
        content: &str,
        at: i64,
    ) -> Result<Notification> {
        if operation.is_empty()
            || !matches!(class, "urgent" | "normal" | "informational")
            || content.len() > 64 * 1024
        {
            return Err(invalid("notification identity/class/content invalid"));
        }
        self.validate_durable(&json!({"content":content}))?;
        let a = self.agent(agent).await?;
        let n = Notification {
            id: format!("ntf_{}", digest(operation.as_bytes())),
            agent_id: a.id,
            conversation: a.origin_conversation,
            class: class.into(),
            content: content.into(),
            created: at,
            acknowledged: false,
        };
        let n2 = n.clone();
        self.inner.db.write(Priority::Journal,move|c|{let old:Option<(String,String,String)>=c.query_row("SELECT agent_id,class,content FROM orchestration_notifications WHERE id=?1",[&n2.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;if old.is_some_and(|(a,k,text)|a!=n2.agent_id||k!=n2.class||text!=n2.content){return Err(excalibur_store::Error::Invalid("notification operation reused"));}c.execute("INSERT OR IGNORE INTO orchestration_notifications VALUES(?1,?2,?3,?4,?5,?6,0)",rusqlite::params![n2.id,n2.agent_id,n2.conversation,n2.class,n2.content,n2.created])?;Ok(())}).await?;
        self.inner.wake.notify_waiters();
        Ok(n)
    }
    pub async fn notifications(&self, conversation: &str) -> Result<Vec<Notification>> {
        let conversation = conversation.to_owned();
        Ok(self.inner.db.read(ReadLane::Interactive,move|c|{let mut q=c.prepare("SELECT id,agent_id,conversation,class,content,created,acknowledged FROM orchestration_notifications WHERE conversation=?1 ORDER BY created,id")?;Ok(q.query_map([conversation],notification_row)?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?)
    }
    /// Return durable pending channel attempts. An adapter acknowledges only
    /// after its own idempotent channel send; retries retain the notification ID.
    pub async fn pending_deliveries(&self, at: i64) -> Result<Vec<Delivery>> {
        let db = self.inner.db.clone();
        Ok(db.write(Priority::Interactive,move|c|{
            let notifications={let mut q=c.prepare("SELECT id,agent_id,conversation,class,content,created,acknowledged FROM orchestration_notifications WHERE acknowledged=0 ORDER BY created,id")?;q.query_map([],notification_row)?.collect::<std::result::Result<Vec<_>,_>>()?};
            let mut out=vec![];for n in notifications {
                let mut policy:NotificationPolicy=c.query_row("SELECT body FROM orchestration_notify_policy WHERE conversation=?1",[&n.conversation],|r|r.get::<_,String>(0)).optional()?.and_then(|s|serde_json::from_str(&s).ok()).unwrap_or_default();
                let task:Option<(Option<String>,Option<String>)>=c.query_row("SELECT t.level,t.notification_policy FROM orchestration_agents a JOIN tasks t ON t.id=json_extract(a.body,'$.task') WHERE a.id=?1",[&n.agent_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                if let Some(overrides)=task.as_ref().and_then(|(_,policy)|policy.as_ref()).and_then(|text|serde_json::from_str::<Value>(text).ok()).and_then(|value|value.as_object().cloned()) {
                    let mut merged=serde_json::to_value(&policy).map_err(|_|excalibur_store::Error::Invalid("invalid notification policy"))?;
                    if let Some(fields)=merged.as_object_mut(){fields.extend(overrides);}
                    if let Ok(candidate)=serde_json::from_value::<NotificationPolicy>(merged)
                        && candidate.quiet_start_minute.is_none_or(|minute|minute<1440)
                        && candidate.quiet_end_minute.is_none_or(|minute|minute<1440)
                        && candidate.utc_offset_minutes.unsigned_abs()<=1440 {policy=candidate;}
                }
                let progress=policy.progress.unwrap_or_else(||task.as_ref().is_some_and(|(level,_)|level.as_deref()==Some("N3")));
                if n.class=="informational"&&!progress{continue;}
                if n.class=="normal"&&at<n.created+5{continue;}
                let minute=(at.div_euclid(60)+i64::from(policy.utc_offset_minutes)).rem_euclid(1440)as u16;
                let quiet=match(policy.quiet_start_minute,policy.quiet_end_minute){(Some(start),Some(end))if start<end=>minute>=start&&minute<end,(Some(start),Some(end))=>minute>=start||minute<end,_=>false};
                if quiet&&(n.class!="urgent"||!policy.urgent_during_quiet){continue;}
                let origin:Option<(String,bool)>=c.query_row("SELECT channel,connected FROM orchestration_activity WHERE conversation=?1",[&n.conversation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                let mut routes=vec![(format!("conversation:{}",n.conversation),None)];
                if origin.as_ref().is_none_or(|(_,connected)|!*connected)&&n.class!="informational"&&let Some(channel)=policy.preferred_channel{routes.push((channel,None));}
                if n.class=="urgent"{let mut q=c.prepare("SELECT conversation FROM orchestration_activity WHERE conversation!=?1 AND connected=1 AND last_active>=?2 ORDER BY last_active DESC LIMIT 1")?;if let Some(other)=q.query_row(rusqlite::params![n.conversation,at-600],|r|r.get::<_,String>(0)).optional()?{routes.push((format!("conversation:{other}"),Some(other)));}}
                for(channel,cross)in routes{
                    let state:Option<String>=c.query_row("SELECT state FROM orchestration_deliveries WHERE notification_id=?1 AND channel=?2",rusqlite::params![n.id,channel],|r|r.get(0)).optional()?;
                    if state.as_deref()==Some("delivered"){continue;}
                    c.execute("INSERT OR IGNORE INTO orchestration_deliveries VALUES(?1,?2,'pending')",rusqlite::params![n.id,channel])?;out.push(Delivery{notification:n.clone(),channel,cross_conversation:cross});
                }
            }Ok(out)
        }).await?)
    }
    pub async fn confirm_delivery(&self, id: &str, channel: &str) -> Result<()> {
        let (id, channel) = (id.to_owned(), channel.to_owned());
        let n=self.inner.db.write(Priority::Journal,move|c|Ok(c.execute("UPDATE orchestration_deliveries SET state='delivered' WHERE notification_id=?1 AND channel=?2",rusqlite::params![id,channel])?)).await?;
        if n == 0 {
            return Err(Error::Missing);
        }
        Ok(())
    }
    pub async fn acknowledge_notification(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        let n = self
            .inner
            .db
            .write(Priority::Interactive, move |c| {
                Ok(c.execute(
                    "UPDATE orchestration_notifications SET acknowledged=1 WHERE id=?1",
                    [id],
                )?)
            })
            .await?;
        if n == 0 {
            return Err(Error::Missing);
        }
        Ok(())
    }
    pub async fn unique_notification_id(&self) -> String {
        MessageId::new().to_string()
    }
}
fn notification_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Notification> {
    Ok(Notification {
        id: r.get(0)?,
        agent_id: r.get(1)?,
        conversation: r.get(2)?,
        class: r.get(3)?,
        content: r.get(4)?,
        created: r.get(5)?,
        acknowledged: r.get(6)?,
    })
}

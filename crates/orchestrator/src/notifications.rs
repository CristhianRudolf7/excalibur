use crate::*;
use excalibur_types::MessageId;
/// Authenticated host metadata. `source` is an accepted client message ID,
/// never a model-generated notice or a field extracted from its content.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct HostNotificationBinding {
    pub principal: String,
    pub scope: String,
    pub conversation: String,
    pub source: String,
    /// Host-configured routes. This grants no external delivery adapter.
    pub approved_channels: Vec<String>,
}
/// Sealed host-only handle. Not exposed through model tools or deserialization.
#[derive(Clone, Debug)]
pub struct HostNotificationTarget {
    id: String,
    binding: HostNotificationBinding,
}
fn validate_host_source(
    c: &rusqlite::Connection,
    b: &HostNotificationBinding,
) -> excalibur_store::Result<()> {
    let allowed: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM messages m JOIN master_client_receipts r ON r.message_id=m.id JOIN conversations v ON v.id=m.conversation_id JOIN principals p ON p.id=v.principal_id WHERE m.id=?1 AND m.conversation_id=?2 AND m.scope_id=?3 AND m.principal_id=?4 AND v.principal_id=?4)", rusqlite::params![b.source,b.conversation,b.scope,b.principal], |r|r.get(0))?;
    let ceiling: Option<String> = c
        .query_row(
            "SELECT body FROM orchestration_agents WHERE depth=0",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let granted = ceiling
        .and_then(|body| serde_json::from_str::<Agent>(&body).ok())
        .is_some_and(|agent| agent.token.data_scopes.contains(&b.scope));
    if !allowed || !granted {
        return Err(excalibur_store::Error::Invalid(
            "host notification source is not authorized",
        ));
    }
    Ok(())
}
impl Orchestrator {
    /// Bind only host-authenticated metadata and configured routing. An accepted
    /// source grants its original scope; a later conversation switch cannot widen it.
    pub async fn bind_host_notification(
        &self,
        binding: HostNotificationBinding,
    ) -> Result<HostNotificationTarget> {
        if [
            &binding.principal,
            &binding.scope,
            &binding.conversation,
            &binding.source,
        ]
        .iter()
        .any(|v| v.is_empty() || v.len() > 512)
            || binding.approved_channels.len() > 32
            || binding.approved_channels.iter().any(|v| {
                v.is_empty()
                    || v.len() > 512
                    || v.starts_with("conversation:")
                        && v != &format!("conversation:{}", binding.conversation)
            })
        {
            return Err(invalid("invalid host notification binding"));
        }
        let body = serde_json::to_string(&binding)?;
        let id = format!("hnb_{}", digest(body.as_bytes()));
        let (key, b) = (id.clone(), binding.clone());
        self.inner.db.write(Priority::Journal,move|c| {
            validate_host_source(c,&b)?;
            c.execute("INSERT OR IGNORE INTO orchestration_host_notification_bindings(id,body) VALUES(?1,?2)",rusqlite::params![key,body])?;
            Ok(())
        }).await?;
        Ok(HostNotificationTarget { id, binding })
    }
    /// Enumerable durable bindings for host-service recovery. Rebinding validates
    /// the original accepted source again and never derives authority from a hash.
    pub async fn host_notification_bindings(
        &self,
        principal: &str,
    ) -> Result<Vec<HostNotificationBinding>> {
        let principal = principal.to_owned();
        let rows=self.inner.db.read(ReadLane::Interactive,move|c| {
            let mut q=c.prepare("SELECT body FROM orchestration_host_notification_bindings WHERE json_extract(body,'$.principal')=?1 ORDER BY id")?;
            Ok(q.query_map([principal],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?)
        }).await?;
        rows.iter().map(|s| Ok(serde_json::from_str(s)?)).collect()
    }
    /// Receipt-backed authority only; ordinary notification JSON is never read.
    pub async fn host_notification_origin(
        &self,
        id: &str,
    ) -> Result<Option<HostNotificationBinding>> {
        let id = id.to_owned();
        let row=self.inner.db.read(ReadLane::Interactive,move|c| {
            Ok(c.query_row("SELECT b.body FROM orchestration_host_notification_origins o JOIN orchestration_host_notification_bindings b ON b.id=o.binding_id WHERE o.notification_id=?1 AND o.source_kind='host'",[id],|r|r.get::<_,String>(0)).optional()?)
        }).await?;
        row.map(|s| serde_json::from_str(&s).map_err(Error::from))
            .transpose()
    }
    /// Exact-key replay returns the insertion snapshot, including its original
    /// timestamp and acknowledgement. Notification and authority commit together.
    pub async fn notify_host(
        &self,
        target: &HostNotificationTarget,
        key: &str,
        class: &str,
        text: &str,
        preferred: Option<&str>,
        at: i64,
    ) -> Result<Notification> {
        if key.is_empty()
            || key.len() > 256
            || !matches!(class, "urgent" | "normal" | "informational")
            || text.len() > 60 * 1024
            || serde_json::to_string(text)?.len() > 60 * 1024
            || preferred.is_some_and(|v| {
                !target
                    .binding
                    .approved_channels
                    .iter()
                    .any(|route| route == v)
            })
        {
            return Err(invalid("invalid host notice or unapproved route"));
        }
        self.validate_durable(&json!({"content":text}))?;
        let request = serde_json::to_string(&(target.id.clone(), class, text, preferred))?;
        let n = Notification {
            id: format!("hntf_{}", digest(key.as_bytes())),
            agent_id: String::new(),
            conversation: target.binding.conversation.clone(),
            class: class.into(),
            content: text.into(),
            created: at,
            acknowledged: false,
        };
        let snapshot = serde_json::to_string(&n)?;
        let (b, binding, preferred) = (
            target.binding.clone(),
            target.id.clone(),
            preferred.map(str::to_owned),
        );
        let result=self.inner.db.write(Priority::Journal,move|c| {
            validate_host_source(c,&b)?;
            let old:Option<(String,String)>=c.query_row("SELECT request,snapshot FROM orchestration_host_notification_origins WHERE notification_id=?1",[&n.id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((old,response))=old {
                if old!=request {return Err(excalibur_store::Error::Invalid("host notification key reused"));}
                return Ok(response);
            }
            c.execute("INSERT INTO orchestration_notifications VALUES(?1,'',?2,?3,?4,?5,0)",rusqlite::params![n.id,n.conversation,n.class,n.content,n.created])?;
            c.execute("INSERT INTO orchestration_host_notification_origins VALUES(?1,?2,?3,?4,?5,'host')",rusqlite::params![n.id,binding,request,snapshot,preferred])?;
            Ok(snapshot)
        }).await?;
        self.inner.wake.notify_waiters();
        Ok(serde_json::from_str(&result)?)
    }
    pub async fn set_notification_policy(
        &self,
        conversation: &str,
        policy: NotificationPolicy,
    ) -> Result<()> {
        if policy.quiet_start_minute.is_some_and(|m| m >= 1440)
            || policy.quiet_end_minute.is_some_and(|m| m >= 1440)
            || policy.utc_offset_minutes.unsigned_abs() > 1440
            || policy
                .timezone
                .as_ref()
                .is_some_and(|zone| zone.parse::<chrono_tz::Tz>().is_err())
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
                let preferred:Option<String>=c.query_row("SELECT preferred_channel FROM orchestration_host_notification_origins WHERE notification_id=?1",[&n.id],|r|r.get(0)).optional()?.flatten();
                if preferred.is_some(){policy.preferred_channel=preferred;}
                let task:Option<(Option<String>,Option<String>)>=c.query_row("SELECT t.level,t.notification_policy FROM orchestration_agents a JOIN tasks t ON t.id=json_extract(a.body,'$.task') WHERE a.id=?1",[&n.agent_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                if let Some(overrides)=task.as_ref().and_then(|(_,policy)|policy.as_ref()).and_then(|text|serde_json::from_str::<Value>(text).ok()).and_then(|value|value.as_object().cloned()) {
                    let mut merged=serde_json::to_value(&policy).map_err(|_|excalibur_store::Error::Invalid("invalid notification policy"))?;
                    if let Some(fields)=merged.as_object_mut(){fields.extend(overrides);}
                    if let Ok(candidate)=serde_json::from_value::<NotificationPolicy>(merged)
                        && candidate.quiet_start_minute.is_none_or(|minute|minute<1440)
                        && candidate.quiet_end_minute.is_none_or(|minute|minute<1440)
                        && candidate.utc_offset_minutes.unsigned_abs()<=1440
                        && candidate.timezone.as_ref().is_none_or(|zone|zone.parse::<chrono_tz::Tz>().is_ok()) {policy=candidate;}
                }
                let progress=policy.progress.unwrap_or_else(||task.as_ref().is_some_and(|(level,_)|level.as_deref()==Some("N3")));
                if n.class=="informational"&&!progress{continue;}
                if n.class=="normal"&&at<(n.created.div_euclid(5)+1)*5{continue;}
                let minute=if let Some(zone)=&policy.timezone {
                    use chrono::Timelike;
                    let zone=zone.parse::<chrono_tz::Tz>().map_err(|_|excalibur_store::Error::Invalid("invalid notification timezone"))?;
                    let local=chrono::DateTime::from_timestamp(at,0).ok_or(excalibur_store::Error::Invalid("notification timestamp out of range"))?.with_timezone(&zone);
                    (local.hour()*60+local.minute())as u16
                }else{(at.div_euclid(60)+i64::from(policy.utc_offset_minutes)).rem_euclid(1440)as u16};
                let quiet=match(policy.quiet_start_minute,policy.quiet_end_minute){(Some(start),Some(end))if start<end=>minute>=start&&minute<end,(Some(start),Some(end))=>minute>=start||minute<end,_=>false};
                if quiet&&(n.class!="urgent"||!policy.urgent_during_quiet){continue;}
                let origin:Option<(String,bool)>=c.query_row("SELECT channel,connected FROM orchestration_activity WHERE conversation=?1",[&n.conversation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                let mut routes=vec![(format!("conversation:{}",n.conversation),None)];
                if origin.as_ref().is_none_or(|(_,connected)|!*connected)&&n.class!="informational"&&let Some(channel)=policy.preferred_channel{routes.push((channel,None));}
                if n.class=="urgent"{let mut q=c.prepare("SELECT conversation FROM orchestration_activity WHERE conversation!=?1 AND connected=1 AND last_active>=?2 AND (NOT EXISTS(SELECT 1 FROM orchestration_host_notification_origins WHERE notification_id=?3) OR EXISTS(SELECT 1 FROM conversations v JOIN orchestration_host_notification_bindings b ON json_extract(b.body,'$.principal')=v.principal_id JOIN orchestration_host_notification_origins o ON o.binding_id=b.id WHERE o.notification_id=?3 AND v.id=orchestration_activity.conversation)) ORDER BY last_active DESC LIMIT 1")?;if let Some(other)=q.query_row(rusqlite::params![n.conversation,at-600,n.id],|r|r.get::<_,String>(0)).optional()?{routes.push((format!("conversation:{other}"),Some(other)));}}
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

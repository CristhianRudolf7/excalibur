//! Explicit host ticks for generation-safe reminder admission. No background service
//! or model tool is installed by this library.
use excalibur_orchestrator::{HostNotificationBinding, Orchestrator};
use excalibur_scheduler::reminders::{Operation, Reminder, Reminders};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] excalibur_store::Error),
    #[error(transparent)]
    Orchestrator(#[from] excalibur_orchestrator::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
/// A stable principal/scope/conversation namespace. Accepted source messages are
/// delivery provenance per generation, never the namespace of each new request.
#[derive(Clone)]
pub struct Agenda {
    reminders: Reminders,
    orchestrator: Orchestrator,
    binding: HostNotificationBinding,
}
impl Agenda {
    pub async fn open(
        orchestrator: Orchestrator,
        binding: HostNotificationBinding,
    ) -> Result<Self, Error> {
        orchestrator.bind_host_notification(binding.clone()).await?;
        let reminders = Reminders::open(
            orchestrator.database().clone(),
            binding.scope.clone(),
            binding.principal.clone(),
            binding.conversation.clone(),
            None,
        )
        .await?;
        Ok(Self {
            reminders,
            orchestrator,
            binding,
        })
    }
    /// Host supplies an accepted message binding at open; model JSON has no such field.
    pub async fn apply(&self, key: String, operation: Operation) -> Result<Reminder, Error> {
        let target = self
            .orchestrator
            .bind_host_notification(self.binding.clone())
            .await?;
        // Preserve the creator's delivery source across snooze/completion generations.
        let metadata = if matches!(&operation, Operation::Create(_)) {
            Some(serde_json::to_value(&self.binding)?)
        } else {
            None
        };
        Ok(self
            .reminders
            .apply_host_validated(
                key,
                operation,
                metadata,
                move |c| target.validate(c),
                |c, reminder, metadata| {
                    if reminder.state == excalibur_scheduler::reminders::State::Active {
                        let binding: HostNotificationBinding = serde_json::from_value(
                            metadata.cloned().ok_or(excalibur_store::Error::Invalid(
                                "reminder has no host delivery binding",
                            ))?,
                        )
                        .map_err(|_| {
                            excalibur_store::Error::Invalid("invalid reminder host metadata")
                        })?;
                        binding.validate_reminder(c, reminder.preferred_channel.as_deref())?;
                    }
                    Ok(())
                },
            )
            .await?)
    }
    pub async fn list(
        &self,
        key: String,
        after: Option<String>,
        limit: u32,
    ) -> Result<Vec<Reminder>, Error> {
        self.orchestrator
            .bind_host_notification(self.binding.clone())
            .await?;
        Ok(self.reminders.list(key, after, limit).await?)
    }
    /// Persist due fires during recovery/safe mode, but admit no notices. Each
    /// successful receipt means NotificationQueued, not delivered or user-acked.
    pub async fn tick_once(&self, at: i64, limit: u32) -> Result<usize, Error> {
        let state = self.orchestrator.recovery().status(at).await?;
        let blocked = state.recovering || state.safe_mode;
        self.reminders.tick(at, blocked, limit).await?;
        dispatch_pending(&self.reminders, &self.orchestrator, at, blocked, limit).await
    }
}
/// Per-fire binding validation: no namespace representative grants delivery authority.
pub(crate) async fn dispatch_pending(reminders: &Reminders, orchestrator: &Orchestrator, at: i64, blocked: bool, limit: u32) -> Result<usize, Error> {
        if blocked {
            return Ok(0);
        }
        let mut queued = 0;
        let mut after = None;
        loop {
            let page = reminders.pending_after(after, limit).await?;
            if page.is_empty() {
                break;
            }
            after = page
                .last()
                .map(|p| (p.fire.scheduled_at, p.fire.id.clone()));
            for pending in page {
                let Some(metadata) = reminders.metadata(&pending).await? else {
                    continue;
                };
                let Ok(binding) = serde_json::from_value::<HostNotificationBinding>(metadata)
                else {
                    continue;
                };
                // Legacy invalid routes remain observable in list/pending, but
                // cannot poison another binding's dispatch page or gain an ACK.
                if !binding.allows_channel(pending.reminder.preferred_channel.as_deref()) {
                    continue;
                }
                let target = match orchestrator.bind_host_notification(binding).await {
                    Ok(target) => target,
                    Err(excalibur_orchestrator::Error::Storage(
                        excalibur_store::Error::Invalid(
                            "host notification source is not authorized",
                        ),
                    )) => continue,
                    Err(excalibur_orchestrator::Error::Invalid(message))
                        if message == "invalid host notification binding" =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                // Hash the typed scheduler fire identity in a versioned JSON tuple.
                // Caller-provided IDs never become raw notification operation keys.
                let key = notice_key(&pending.fire.id)?;
                if orchestrator
                    .notify_host_guarded(
                        &target,
                        &key,
                        "normal",
                        &pending.reminder.text,
                        pending.reminder.preferred_channel.as_deref(),
                        pending.fire.scheduled_at,
                        reminders.condition(&pending),
                        at,
                    )
                    .await?
                    .is_some()
                {
                    reminders.acknowledge_queued(pending.fire.id).await?;
                    queued += 1;
                    if queued >= limit as usize {
                        return Ok(queued);
                    }
                }
            }
        }
        Ok(queued)
    }

fn notice_key(fire: &str) -> Result<String, serde_json::Error> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "agenda_{:x}",
        Sha256::digest(serde_json::to_vec(&("reminder-notice-v1", fire))?)
    ))
}

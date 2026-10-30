//! Generation-safe reminder admission. `Agenda` is the explicit host-tick API
//! for one authenticated namespace; `AgendaRuntime` is the daemon-owned
//! automatic task that records due fires and admits them through the same
//! guarded host-notification path. No model tool or RPC is installed here.
use excalibur_orchestrator::{HostNotificationBinding, Orchestrator};
use excalibur_scheduler::reminders::{self, Operation, Reminder, Reminders};
use futures::future::BoxFuture;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::{Mutex, Notify, watch};

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

/// Admit recorded fires of one namespace. Authority comes only from the
/// creator binding persisted with each fire's own generation; the handle that
/// enumerates the namespace never grants delivery authority, and a missing or
/// mismatched binding is skipped rather than replaced by a fallback.
pub(crate) async fn dispatch_pending(
    reminders: &Reminders,
    orchestrator: &Orchestrator,
    at: i64,
    blocked: bool,
    limit: u32,
) -> Result<usize, Error> {
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
            let Ok(binding) = serde_json::from_value::<HostNotificationBinding>(metadata) else {
                continue;
            };
            // Metadata must describe exactly this sealed namespace; it cannot
            // redirect a fire to another principal, scope or conversation.
            if reminders::namespace_of(
                &binding.scope,
                &binding.principal,
                &binding.conversation,
                None,
            )
            .ok()
            .as_deref()
                != Some(reminders.namespace())
            {
                continue;
            }
            // Legacy invalid routes remain observable in list/pending, but
            // cannot poison another binding's dispatch page or gain an ACK.
            if !binding.allows_channel(pending.reminder.preferred_channel.as_deref()) {
                continue;
            }
            let target = match orchestrator.bind_host_notification(binding).await {
                Ok(target) => target,
                Err(excalibur_orchestrator::Error::Storage(excalibur_store::Error::Invalid(
                    "host notification source is not authorized",
                ))) => continue,
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

/// Wall clock seen by the automatic runtime, in UTC seconds. Tests inject a
/// manual clock; the daemon uses `SystemClock`.
pub trait AgendaClock: Send + Sync + 'static {
    fn now(&self) -> i64;
    /// Resolve no earlier than `at` on this clock.
    fn sleep_until(&self, at: i64) -> BoxFuture<'static, ()>;
}
pub struct SystemClock;
impl AgendaClock for SystemClock {
    fn now(&self) -> i64 {
        excalibur_store::unix_seconds()
    }
    fn sleep_until(&self, at: i64) -> BoxFuture<'static, ()> {
        let delay = at.saturating_sub(self.now()).max(0) as u64;
        Box::pin(tokio::time::sleep(std::time::Duration::from_secs(delay)))
    }
}
/// Deterministic test clock: sleepers wake when `set`/`advance` reaches them.
pub struct ManualClock {
    now: watch::Sender<i64>,
}
impl ManualClock {
    pub fn new(at: i64) -> Arc<Self> {
        Arc::new(Self {
            now: watch::channel(at).0,
        })
    }
    pub fn set(&self, at: i64) {
        self.now.send_replace(at);
    }
    pub fn advance(&self, seconds: i64) {
        self.now
            .send_modify(|now| *now = now.saturating_add(seconds));
    }
}
impl AgendaClock for ManualClock {
    fn now(&self) -> i64 {
        *self.now.borrow()
    }
    fn sleep_until(&self, at: i64) -> BoxFuture<'static, ()> {
        let mut now = self.now.subscribe();
        Box::pin(async move {
            if now.wait_for(|now| *now >= at).await.is_err() {
                std::future::pending::<()>().await;
            }
        })
    }
}

/// Upper bound between durable re-checks while idle. It covers authority that
/// becomes valid again, lost process-local hints and monotonic-clock suspend.
pub const IDLE_RECHECK_SECONDS: i64 = 60;
/// Re-check interval while startup recovery or safe mode blocks admission, so
/// a manual safe-mode exit releases recorded fires without a restart.
pub const BLOCKED_RECHECK_SECONDS: i64 = 5;
/// A wake this late (suspend, stalled host) is treated as an outage and the
/// missed-fire policy is applied again instead of replaying every interval.
pub const LATE_WAKE_SECONDS: i64 = 60;
const NAMESPACE_PAGE: u32 = 256;
const HANDLE_CACHE: usize = 4096;

/// Outcome of one runtime pass; useful for host diagnostics and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgendaStep {
    pub now: i64,
    /// Fires newly recorded by this pass.
    pub recorded: usize,
    /// Fires admitted as guarded host notifications (NotificationQueued).
    pub queued: usize,
    /// Recovery or safe mode blocked admission; fires were only recorded.
    pub blocked: bool,
    /// Missed-fire policy (17.4) applied instead of ordinary every-fire ticks.
    pub catch_up: bool,
    /// Next wake on the runtime clock: min(next_fire_utc) bounded by re-checks.
    pub wake_at: i64,
}
struct RuntimeState {
    handles: BTreeMap<String, Reminders>,
    /// Starts true: the first pass after process start recomputes the fires
    /// missed while the daemon was down. Stays true until no timer is overdue.
    catching_up: bool,
    deadline: Option<i64>,
}
/// Daemon-owned automatic Agenda: records due reminder fires in every reminder
/// namespace and admits them through `notify_host_guarded`. It holds no
/// binding of its own. One pass is serialized per runtime; concurrent runtimes
/// or explicit `Agenda::tick_once` calls are deduplicated by durable fire IDs,
/// timer compare-and-set and keyed notification receipts.
pub struct AgendaRuntime {
    orchestrator: Orchestrator,
    clock: Arc<dyn AgendaClock>,
    signal: Arc<Notify>,
    state: Mutex<RuntimeState>,
    limit: u32,
}
impl AgendaRuntime {
    pub fn new(orchestrator: Orchestrator, clock: Arc<dyn AgendaClock>) -> Arc<Self> {
        let signal = excalibur_scheduler::mutation_signal(orchestrator.database());
        Arc::new(Self {
            orchestrator,
            clock,
            signal,
            state: Mutex::new(RuntimeState {
                handles: BTreeMap::new(),
                catching_up: true,
                deadline: None,
            }),
            limit: 100,
        })
    }
    /// One durable pass: record due fires per policy, admit them unless
    /// blocked, and compute the next wake from the earliest durable timer.
    pub async fn step(&self) -> Result<AgendaStep, Error> {
        let mut state = self.state.lock().await;
        let now = self.clock.now();
        if state
            .deadline
            .is_some_and(|deadline| now > deadline.saturating_add(LATE_WAKE_SECONDS))
        {
            state.catching_up = true;
        }
        let status = self.orchestrator.recovery().status(now).await?;
        let blocked = status.recovering || status.safe_mode;
        let catch_up = state.catching_up || blocked;
        let db = self.orchestrator.database().clone();
        let (mut recorded, mut queued) = (0usize, 0usize);
        let (mut next, mut backlog) = (None::<i64>, false);
        let mut after = None;
        loop {
            let page = reminders::index(&db, after.clone(), NAMESPACE_PAGE).await?;
            let full = page.len() == NAMESPACE_PAGE as usize;
            after = page.last().map(|entry| entry.namespace.clone());
            for entry in page {
                let due = entry.next_at.is_some_and(|at| at <= now);
                if !due && (blocked || !entry.pending) {
                    next = earliest(next, entry.next_at);
                    continue;
                }
                let Some(handle) = self.handle(&mut state.handles, &entry).await? else {
                    // Without a verifiable creator binding nothing is opened,
                    // ticked or admitted; the durable rows remain for audit.
                    continue;
                };
                if due {
                    recorded += handle.tick(now, catch_up, self.limit).await?;
                }
                let admitted =
                    dispatch_pending(&handle, &self.orchestrator, now, blocked, self.limit).await?;
                backlog |= admitted >= self.limit as usize;
                queued += admitted;
                let following = handle.next_at().await?;
                backlog |= following.is_some_and(|at| at <= now);
                next = earliest(next, following);
            }
            if !full {
                break;
            }
        }
        if !backlog {
            state.catching_up = false;
        }
        let mut wake = now.saturating_add(IDLE_RECHECK_SECONDS);
        if let Some(at) = next {
            wake = wake.min(at.max(now));
        }
        if blocked {
            wake = wake.min(now.saturating_add(BLOCKED_RECHECK_SECONDS));
        }
        if backlog {
            // Continue promptly while making progress; never spin otherwise.
            wake = if recorded + queued > 0 {
                now
            } else {
                now.saturating_add(1)
            };
        } else if wake <= now {
            wake = now.saturating_add(1);
        }
        state.deadline = Some(wake);
        Ok(AgendaStep {
            now,
            recorded,
            queued,
            blocked,
            catch_up,
            wake_at: wake,
        })
    }
    /// Sleep until `wake_at` on the runtime clock or a schedule mutation.
    pub async fn wait(&self, wake_at: i64) {
        tokio::select! {
            _ = self.clock.sleep_until(wake_at) => {}
            _ = self.signal.notified() => {}
        }
    }
    /// Supervised daemon task. Stopping cancels the wait immediately; a pass
    /// interrupted mid-way is idempotent on the next start.
    pub fn spawn(
        self: Arc<Self>,
        runtime: &tokio::runtime::Handle,
        shutdown: Arc<Notify>,
        stop: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<Result<(), String>> {
        crate::services::supervise(runtime, "agenda", shutdown, stop, move || {
            let agenda = self.clone();
            async move {
                let deadline = agenda.state.lock().await.deadline;
                if let Some(at) = deadline {
                    agenda.wait(at).await;
                }
                agenda.step().await.map(|_| ()).map_err(|e| e.to_string())
            }
        })
    }
    async fn handle(
        &self,
        handles: &mut BTreeMap<String, Reminders>,
        entry: &reminders::NamespaceIndex,
    ) -> Result<Option<Reminders>, Error> {
        if let Some(handle) = handles.get(&entry.namespace) {
            return Ok(Some(handle.clone()));
        }
        let Some(binding) = entry
            .binding
            .clone()
            .and_then(|value| serde_json::from_value::<HostNotificationBinding>(value).ok())
        else {
            return Ok(None);
        };
        if reminders::namespace_of(
            &binding.scope,
            &binding.principal,
            &binding.conversation,
            None,
        )
        .ok()
        .as_deref()
            != Some(entry.namespace.as_str())
        {
            return Ok(None);
        }
        let handle = Reminders::open(
            self.orchestrator.database().clone(),
            binding.scope,
            binding.principal,
            binding.conversation,
            None,
        )
        .await?;
        if handle.namespace() != entry.namespace {
            return Ok(None);
        }
        if handles.len() >= HANDLE_CACHE {
            handles.clear();
        }
        handles.insert(entry.namespace.clone(), handle.clone());
        Ok(Some(handle))
    }
}
fn earliest(current: Option<i64>, candidate: Option<i64>) -> Option<i64> {
    match (current, candidate) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

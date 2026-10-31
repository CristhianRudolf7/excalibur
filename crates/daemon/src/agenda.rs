//! Generation-safe reminder admission. `Agenda` is the explicit host-tick API
//! for one authenticated namespace; `AgendaRuntime` is the daemon-owned
//! automatic task that records due fires and admits them through the same
//! guarded host-notification path. The `reminders` master tool and the
//! `agenda.reminders` RPC live in `agenda_tools` and call `Agenda`.
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
/// Writer refusal of reminder text containing a secret literal.
pub const SECRET_TEXT_REJECTED: &str = "secret literal in reminder text; use references";
/// Writer refusal of reminder text the notice writer would never admit.
pub const TEXT_REJECTED: &str = "reminder text rejected by the notice writer";
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
        self.apply_at(key, operation, excalibur_store::unix_seconds())
            .await
    }
    /// As `apply`, with the host clock used by `Operation::ChangeTimezone`.
    pub async fn apply_at(
        &self,
        key: String,
        operation: Operation,
        now: i64,
    ) -> Result<Reminder, Error> {
        let target = self
            .orchestrator
            .bind_host_notification(self.binding.clone())
            .await?;
        let create = matches!(&operation, Operation::Create(_));
        // Preserve the creator's delivery source across snooze/completion generations.
        let metadata = if create {
            Some(serde_json::to_value(&self.binding)?)
        } else {
            None
        };
        let validate = self.validator(create);
        Ok(self
            .reminders
            .apply_host_validated_at(
                key,
                operation,
                metadata,
                now,
                move |c| target.validate(c),
                validate,
            )
            .await?)
    }
    /// Recompute every civil reminder of this namespace in `timezone` and
    /// return the keyed report the user is informed with (21.1).
    pub async fn change_timezone(
        &self,
        key: String,
        timezone: String,
        now: i64,
    ) -> Result<reminders::TimezoneReport, Error> {
        let target = self
            .orchestrator
            .bind_host_notification(self.binding.clone())
            .await?;
        Ok(self
            .reminders
            .change_timezone_all(
                key,
                timezone,
                now,
                move |c| target.validate(c),
                self.validator(false),
            )
            .await?)
    }
    /// Writer-side validation, run after the keyed-replay check: an exact
    /// retry returns its original snapshot even if the redactor learned a new
    /// secret since (AR-10). A new create whose text the notice writer would
    /// refuse forever (for example a secret literal) is rejected rather than
    /// silently redacted, so the stored reminder keeps the user's wording.
    fn validator(
        &self,
        create: bool,
    ) -> impl Fn(
        &excalibur_store::rusqlite::Connection,
        &Reminder,
        Option<&serde_json::Value>,
    ) -> excalibur_store::Result<()>
    + Clone
    + Send
    + 'static {
        let orchestrator = self.orchestrator.clone();
        move |c, reminder, metadata| {
            if create {
                orchestrator
                    .validate_host_notice_text(&reminder.text)
                    .map_err(|error| match error {
                        excalibur_orchestrator::Error::Invalid(message)
                            if message.starts_with("secret literal") =>
                        {
                            excalibur_store::Error::Invalid(SECRET_TEXT_REJECTED)
                        }
                        _ => excalibur_store::Error::Invalid(TEXT_REJECTED),
                    })?;
            }
            if reminder.state == excalibur_scheduler::reminders::State::Active {
                let binding: HostNotificationBinding = serde_json::from_value(
                    metadata.cloned().ok_or(excalibur_store::Error::Invalid(
                        "reminder has no host delivery binding",
                    ))?,
                )
                .map_err(|_| excalibur_store::Error::Invalid("invalid reminder host metadata"))?;
                binding.validate_reminder(c, reminder.preferred_channel.as_deref())?;
            }
            Ok(())
        }
    }
    /// Live durable `next_fire_utc` of an active reminder generation.
    pub async fn next_fire(&self, reminder: &Reminder) -> Result<Option<i64>, Error> {
        if reminder.state != excalibur_scheduler::reminders::State::Active {
            return Ok(None);
        }
        Ok(self
            .reminders
            .timer_next(&reminder.id, reminder.generation)
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
        Ok(
            dispatch_pending(&self.reminders, &self.orchestrator, at, blocked, limit)
                .await?
                .queued,
        )
    }
}

/// Result of admitting one namespace's recorded fires.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dispatch {
    /// Guarded notices queued (NotificationQueued) and acknowledged.
    pub queued: usize,
    /// Calls into the guarded notification writer.
    pub attempted: usize,
    /// Fires terminally retired because the notice can never be admitted.
    pub dead_lettered: usize,
    /// Fires left pending: no verifiable binding, unapproved route, revoked
    /// source or a cancelled generation. They stay durable for a later retry.
    pub undeliverable: usize,
}
/// Deterministic refusals of a notice request; retrying cannot succeed.
fn terminal(error: &excalibur_orchestrator::Error) -> Option<reminders::DeadLetterReason> {
    use excalibur_orchestrator::Error as E;
    use reminders::DeadLetterReason as R;
    match error {
        E::Invalid(message) if message.starts_with("secret literal") => Some(R::ContentRejected),
        E::Invalid(_) | E::Authority(_) | E::Json(_) => Some(R::NoticeRejected),
        E::Storage(excalibur_store::Error::Invalid("host notification key reused")) => {
            Some(R::NoticeRejected)
        }
        _ => None,
    }
}

/// Admit recorded fires of one namespace. Authority comes only from the
/// creator binding persisted with each fire's own generation; the handle that
/// enumerates the namespace never grants delivery authority, and a missing or
/// mismatched binding is skipped rather than replaced by a fallback.
/// A deterministic refusal retires only that fire (dead letter with a reason
/// code) and dispatch continues; only transient storage errors abort, and
/// they abort this namespace alone.
pub(crate) async fn dispatch_pending(
    reminders: &Reminders,
    orchestrator: &Orchestrator,
    at: i64,
    blocked: bool,
    limit: u32,
) -> Result<Dispatch, Error> {
    let mut outcome = Dispatch::default();
    if blocked {
        return Ok(outcome);
    }
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
            let binding = reminders
                .metadata(&pending)
                .await?
                .and_then(|value| serde_json::from_value::<HostNotificationBinding>(value).ok());
            let Some(binding) = binding else {
                outcome.undeliverable += 1;
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
                outcome.undeliverable += 1;
                continue;
            }
            // Legacy invalid routes remain observable in list/pending, but
            // cannot poison another binding's dispatch page or gain an ACK.
            if !binding.allows_channel(pending.reminder.preferred_channel.as_deref()) {
                outcome.undeliverable += 1;
                continue;
            }
            let target = match orchestrator.bind_host_notification(binding).await {
                Ok(target) => target,
                Err(excalibur_orchestrator::Error::Storage(excalibur_store::Error::Invalid(_)))
                | Err(excalibur_orchestrator::Error::Invalid(_)) => {
                    outcome.undeliverable += 1;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            // Hash the typed scheduler fire identity in a versioned JSON tuple.
            // Caller-provided IDs never become raw notification operation keys.
            let key = notice_key(&pending.fire.id)?;
            outcome.attempted += 1;
            match orchestrator
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
                .await
            {
                Ok(Some(_)) => {
                    reminders.acknowledge_queued(pending.fire.id).await?;
                    outcome.queued += 1;
                    if outcome.queued >= limit as usize {
                        return Ok(outcome);
                    }
                }
                Ok(None) => outcome.undeliverable += 1,
                Err(error) => match terminal(&error) {
                    Some(reason) => {
                        if reminders.dead_letter(pending.fire.id, reason, at).await? {
                            outcome.dead_lettered += 1;
                        }
                    }
                    None => return Err(error.into()),
                },
            }
        }
    }
    Ok(outcome)
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
    /// Resolve no earlier than `at` on this clock, or early when the clock
    /// is observed to step backwards. Callers bound real waiting separately.
    fn sleep_until(&self, at: i64) -> BoxFuture<'static, ()>;
}
pub struct SystemClock;
impl AgendaClock for SystemClock {
    fn now(&self) -> i64 {
        excalibur_store::unix_seconds()
    }
    fn sleep_until(&self, at: i64) -> BoxFuture<'static, ()> {
        // A monotonic sleep of at most one re-check interval: a wall clock
        // stepping backwards cannot stretch the wait beyond the bound.
        let delay = at.saturating_sub(self.now()).clamp(0, IDLE_RECHECK_SECONDS) as u64;
        Box::pin(tokio::time::sleep(std::time::Duration::from_secs(delay)))
    }
}
/// Deterministic test clock: sleepers wake when `set`/`advance` reaches them,
/// and also when the clock is set backwards, so callers re-evaluate.
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
        let start = *now.borrow();
        Box::pin(async move {
            if now
                .wait_for(|now| *now >= at || *now < start)
                .await
                .is_err()
            {
                std::future::pending::<()>().await;
            }
        })
    }
}

/// Upper bound between durable re-checks while idle, enforced on a monotonic
/// timer. It covers authority that becomes valid again, lost process-local
/// hints, monotonic clocks paused by suspend and wall clocks stepping back.
pub const IDLE_RECHECK_SECONDS: i64 = 60;
/// Re-check interval while startup recovery or safe mode blocks admission, so
/// a manual safe-mode exit releases recorded fires without a restart.
pub const BLOCKED_RECHECK_SECONDS: i64 = 5;
/// A wake this late (suspend, stalled host) is treated as an outage and the
/// missed-fire policy is applied again instead of replaying every interval.
pub const LATE_WAKE_SECONDS: i64 = 60;
/// Longest per-namespace deferral after transient errors or when nothing in
/// the namespace could be admitted. Restored authority waits at most this.
pub const MAX_NAMESPACE_BACKOFF_SECONDS: i64 = 900;
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
    /// Calls into the guarded notification writer.
    pub attempted: usize,
    /// Fires terminally retired (deterministic refusal or superseded).
    pub dead_lettered: usize,
    /// Namespaces whose processing failed transiently and were deferred.
    pub failed_namespaces: usize,
    /// Namespaces skipped for dispatch because of an active backoff.
    pub deferred_namespaces: usize,
    /// Recovery or safe mode blocked admission; fires were only recorded.
    pub blocked: bool,
    /// Missed-fire policy (17.4) applied instead of ordinary every-fire ticks.
    pub catch_up: bool,
    /// Next wake on the runtime clock: min(next_fire_utc) bounded by re-checks.
    pub wake_at: i64,
}
#[derive(Clone, Copy)]
struct Backoff {
    until: i64,
    failures: u32,
}
struct RuntimeState {
    handles: BTreeMap<String, Reminders>,
    /// Starts true: the first pass after process start recomputes the fires
    /// missed while the daemon was down. Stays true until no timer is overdue.
    catching_up: bool,
    deadline: Option<i64>,
    backoff: BTreeMap<String, Backoff>,
}
#[derive(Default)]
struct NamespacePass {
    recorded: usize,
    dispatch: Dispatch,
    retired: usize,
    dispatched: bool,
    following: Option<i64>,
}
/// Daemon-owned automatic Agenda: records due reminder fires in every reminder
/// namespace and admits them through `notify_host_guarded`. It holds no
/// binding of its own. One pass is serialized per runtime; concurrent runtimes
/// or explicit `Agenda::tick_once` calls are deduplicated by durable fire IDs,
/// timer compare-and-set and keyed notification receipts. Failures are
/// isolated per fire (dead letter) and per namespace (backoff).
pub struct AgendaRuntime {
    orchestrator: Orchestrator,
    clock: Arc<dyn AgendaClock>,
    signal: Arc<Notify>,
    state: Mutex<RuntimeState>,
    passes: std::sync::atomic::AtomicU64,
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
                backoff: BTreeMap::new(),
            }),
            passes: Default::default(),
            limit: 100,
        })
    }
    /// Completed passes, for host diagnostics and tests.
    pub fn passes(&self) -> u64 {
        self.passes.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// One durable pass: record due fires per policy, admit them unless
    /// blocked, and compute the next wake from the earliest durable timer.
    /// Only a failure to read the namespace index fails the whole pass.
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
        let mut step = AgendaStep {
            now,
            recorded: 0,
            queued: 0,
            attempted: 0,
            dead_lettered: 0,
            failed_namespaces: 0,
            deferred_namespaces: 0,
            blocked,
            catch_up,
            wake_at: now,
        };
        let (mut next, mut backlog, mut progress) = (None::<i64>, false, false);
        let mut after = None;
        loop {
            let page = reminders::index(&db, after.clone(), NAMESPACE_PAGE).await?;
            let full = page.len() == NAMESPACE_PAGE as usize;
            after = page.last().map(|entry| entry.namespace.clone());
            for entry in page {
                let due = entry.next_at.is_some_and(|at| at <= now);
                let backoff = state
                    .backoff
                    .get(&entry.namespace)
                    .copied()
                    .filter(|b| b.until > now);
                let dispatch = !blocked && backoff.is_none();
                if let Some(b) = backoff
                    && (due || entry.pending)
                {
                    next = earliest(next, Some(b.until));
                }
                if !due && !(entry.pending && dispatch) {
                    next = earliest(next, entry.next_at);
                    continue;
                }
                let Some(handle) = (match self.handle(&mut state.handles, &entry).await {
                    Ok(handle) => handle,
                    Err(_) => {
                        step.failed_namespaces += 1;
                        Self::defer(&mut state.backoff, &entry.namespace, now, &mut next);
                        continue;
                    }
                }) else {
                    // Without a verifiable creator binding nothing is opened,
                    // ticked or admitted; the durable rows remain for audit.
                    continue;
                };
                match self
                    .namespace_pass(&handle, now, due, catch_up, backoff.is_some(), blocked)
                    .await
                {
                    Ok(pass) => {
                        step.recorded += pass.recorded;
                        step.queued += pass.dispatch.queued;
                        step.attempted += pass.dispatch.attempted;
                        step.dead_lettered += pass.dispatch.dead_lettered + pass.retired;
                        progress |= pass.recorded + pass.dispatch.queued > 0;
                        backlog |= pass.dispatch.queued >= self.limit as usize;
                        backlog |= pass.following.is_some_and(|at| at <= now);
                        next = earliest(next, pass.following);
                        if !blocked && !pass.dispatched {
                            step.deferred_namespaces += 1;
                        }
                        if pass.dispatched {
                            if pass.dispatch.undeliverable > 0 && pass.dispatch.queued == 0 {
                                // Nothing admissible: retry pending-only work
                                // with growing delay instead of every pass.
                                Self::defer(&mut state.backoff, &entry.namespace, now, &mut next);
                            } else {
                                state.backoff.remove(&entry.namespace);
                            }
                        }
                    }
                    Err(_) => {
                        step.failed_namespaces += 1;
                        Self::defer(&mut state.backoff, &entry.namespace, now, &mut next);
                    }
                }
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
            wake = if progress { now } else { now.saturating_add(1) };
        } else if wake <= now {
            wake = now.saturating_add(1);
        }
        state.deadline = Some(wake);
        step.wake_at = wake;
        self.passes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(step)
    }
    async fn namespace_pass(
        &self,
        handle: &Reminders,
        now: i64,
        due: bool,
        catch_up: bool,
        deferred: bool,
        blocked: bool,
    ) -> Result<NamespacePass, Error> {
        let mut pass = NamespacePass::default();
        if due {
            pass.recorded = handle.tick(now, catch_up, self.limit).await?;
        }
        // RunOnceNow for reminders also bounds safe-mode and undeliverable
        // backlogs: one pending fire per active generation at most.
        pass.retired = handle.coalesce_pending(now).await?;
        // A backoff defers retries of old undeliverable fires, never a fire
        // recorded in this pass.
        pass.dispatched = !blocked && (!deferred || pass.recorded > 0);
        if pass.dispatched {
            pass.dispatch =
                dispatch_pending(handle, &self.orchestrator, now, blocked, self.limit).await?;
        }
        pass.following = handle.next_at().await?;
        Ok(pass)
    }
    fn defer(
        backoff: &mut BTreeMap<String, Backoff>,
        namespace: &str,
        now: i64,
        next: &mut Option<i64>,
    ) {
        let failures = backoff
            .get(namespace)
            .map_or(0, |b| b.failures)
            .saturating_add(1);
        let delay = IDLE_RECHECK_SECONDS
            .saturating_mul(1i64 << failures.saturating_sub(1).min(4))
            .min(MAX_NAMESPACE_BACKOFF_SECONDS);
        let until = now.saturating_add(delay);
        backoff.insert(namespace.to_owned(), Backoff { until, failures });
        *next = earliest(*next, Some(until));
        if backoff.len() > HANDLE_CACHE {
            backoff.retain(|_, b| b.until > now);
        }
    }
    /// Sleep until `wake_at` on the runtime clock or a schedule mutation, and
    /// never longer than one re-check interval of real (monotonic) time.
    pub async fn wait(&self, wake_at: i64) {
        tokio::select! {
            _ = self.clock.sleep_until(wake_at) => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(IDLE_RECHECK_SECONDS as u64)) => {}
            _ = self.signal.notified() => {}
        }
    }
    /// Supervised daemon task. Stopping cancels the wait immediately; a pass
    /// interrupted mid-way is idempotent on the next start. Diagnostics carry
    /// counts and reason codes only, never reminder text.
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
                let step = agenda.step().await.map_err(|e| e.to_string())?;
                if step.dead_lettered > 0 || step.failed_namespaces > 0 {
                    eprintln!(
                        "daemon service agenda: {} fire(s) retired, {} namespace(s) deferred after errors",
                        step.dead_lettered, step.failed_namespaces
                    );
                }
                Ok(())
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

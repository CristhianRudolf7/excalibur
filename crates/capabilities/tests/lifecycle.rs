//! §14.12 rows: lifecycle through every state with a restart mid-task,
//! keyed effects resent once, cancellations that are not honoured, backoff
//! and deadlines. All remote behaviour comes from the deterministic simulation.
mod support;
use excalibur_capabilities::{
    CancelState, CapabilityProvider, CapabilityTasks, Error, FailureClass, ManualClock,
    ProviderError, RemoteStatus, StartRequest, TaskState,
    simulated::{CancelBehavior, SimStep, SimulatedCapability},
    tasks::task_id,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use support::*;

#[tokio::test]
async fn simulated_capability_reaches_every_state_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(T0);
    let h = open(dir.path(), clock.clone()).await;
    let mut steps = working(2);
    steps.push(SimStep::InputRequired {
        request: json!({"kind": "captcha", "prompt": "type the letters"}),
    });
    steps.extend(working(1));
    steps.push(completed(json!({"report": "done"})));
    h.sim
        .define("long_job", script(steps, CancelBehavior::Honor));
    let task = h
        .tasks
        .submit(submission("job-a", "long_job"))
        .await
        .unwrap();
    assert_eq!(task.id, task_id(SCOPE, "job-a"));
    assert_eq!(task.state, TaskState::Working);
    assert!(task.remote_id.is_none());
    let task = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    let remote = task.remote_id.clone().expect("remote task id persisted");
    assert_eq!(task.next_poll_at, T0 + 1_000);
    clock.advance(1_000);
    let task = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!(task.state, TaskState::Working);
    let polls_before = h.sim.polls();

    // Daemon restart mid-task: new store handle, new provider handle.
    let h = h.restart(dir.path(), clock.clone()).await;
    assert_eq!(h.tasks.recover_interrupted().await.unwrap(), 0);
    let reopened = h.tasks.get(&task.id).await.unwrap().unwrap();
    assert_eq!(reopened.remote_id.as_deref(), Some(remote.as_str()));
    assert_eq!(reopened.next_poll_at, task.next_poll_at);
    assert_eq!(h.tasks.next_wake().await.unwrap(), Some(task.next_poll_at));
    // Not yet due: no remote call.
    assert_eq!(h.tasks.drive_due(&h.sim, 10).await.unwrap().len(), 0);
    assert_eq!(h.sim.polls(), polls_before);
    clock.advance(1_000);
    let task = h.tasks.drive_due(&h.sim, 10).await.unwrap().remove(0);
    assert_eq!(h.sim.polls(), polls_before + 1, "polling resumed");
    assert_eq!(task.state, TaskState::Working);
    clock.advance(1_000);
    let task = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!(task.state, TaskState::InputRequired);
    assert_eq!(task.input_seq, 1);
    let request = task.input_request.clone().unwrap();
    assert!(request.untrusted);
    assert_eq!(request.value["kind"], "captcha");
    let waiting = h.tasks.awaiting_input().await.unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].id, task.id);
    // Polling while waiting does not invent a new request.
    clock.advance(1_000);
    let task = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!((task.state, task.input_seq), (TaskState::InputRequired, 1));
    assert!(matches!(
        h.tasks.respond(&task.id, 7, json!({"answer": "x"})).await,
        Err(Error::Conflict(_))
    ));
    let task = h
        .tasks
        .respond(&task.id, 1, json!({"answer": "QX7P"}))
        .await
        .unwrap();
    assert!(task.input_pending);
    assert!(h.tasks.awaiting_input().await.unwrap().is_empty());
    let task = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!(task.state, TaskState::Working);
    assert!(!task.input_pending && task.input_request.is_none());
    assert_eq!(h.sim.inputs(&remote), vec![json!({"answer": "QX7P"})]);
    for _ in 0..3 {
        clock.advance(1_000);
        h.tasks.drive(&h.sim, &task.id).await.unwrap();
    }
    let task = h.tasks.get(&task.id).await.unwrap().unwrap();
    assert_eq!(task.state, TaskState::Completed);
    let result = task.result.unwrap();
    assert!(result.untrusted);
    assert_eq!(result.value, json!({"report": "done"}));
    assert_eq!(task.remote_id.as_deref(), Some(remote.as_str()));
    // Terminal tasks are never polled again.
    let polls = h.sim.polls();
    clock.advance(10_000);
    assert!(h.tasks.drive_due(&h.sim, 10).await.unwrap().is_empty());
    h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!(h.sim.polls(), polls);

    // Failed and cancelled on two more tasks of the same capability.
    h.sim.define(
        "lookup",
        script(
            vec![
                SimStep::Working,
                SimStep::Failed {
                    class: FailureClass::Input,
                    message: "query not understood".into(),
                },
            ],
            CancelBehavior::Honor,
        ),
    );
    let failed = h.tasks.submit(submission("job-b", "lookup")).await.unwrap();
    h.tasks.drive(&h.sim, &failed.id).await.unwrap();
    for _ in 0..2 {
        clock.advance(1_000);
        h.tasks.drive(&h.sim, &failed.id).await.unwrap();
    }
    let failed = h.tasks.get(&failed.id).await.unwrap().unwrap();
    assert_eq!(failed.state, TaskState::Failed);
    let failure = failed.failure.unwrap();
    assert_eq!(failure.class, FailureClass::Input);
    assert!(!failure.effect_uncertain);

    h.sim
        .define("long_job", script(working(20), CancelBehavior::Honor));
    let cancelled = h
        .tasks
        .submit(submission("job-c", "long_job"))
        .await
        .unwrap();
    h.tasks.drive(&h.sim, &cancelled.id).await.unwrap();
    h.tasks.request_cancel(&cancelled.id).await.unwrap();
    let cancelled = h.tasks.drive(&h.sim, &cancelled.id).await.unwrap();
    assert_eq!(cancelled.state, TaskState::Cancelled);
    assert_eq!(cancelled.cancel, CancelState::Sent);

    let mut seen = BTreeSet::new();
    for id in [&task.id, &failed.id, &cancelled.id] {
        seen.insert("working".to_string());
        for event in h.tasks.events(id).await.unwrap() {
            if event.kind == "state" {
                seen.insert(event.detail["to"].as_str().unwrap().to_string());
            }
        }
    }
    assert_eq!(
        seen,
        [
            "cancelled",
            "completed",
            "failed",
            "input_required",
            "working"
        ]
        .map(String::from)
        .into_iter()
        .collect()
    );
    // The answer itself never reaches the audit log.
    let log = format!("{:?}", h.tasks.events(&task.id).await.unwrap());
    assert!(log.contains("input_recorded") && !log.contains("QX7P"));
}

#[tokio::test]
async fn keyed_effects_resent_after_lost_responses_are_applied_once() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(T0);
    let h = open(dir.path(), clock.clone()).await;
    let mut steps = vec![SimStep::InputRequired {
        request: json!({"kind": "confirm"}),
    }];
    steps.push(completed(json!({"sent": true})));
    h.sim
        .define("long_job", script(steps, CancelBehavior::Honor));
    let task = h
        .tasks
        .submit(submission("send-1", "long_job"))
        .await
        .unwrap();
    let key = format!("{}:start", task.id);
    h.sim.lose_next_responses(1);
    let after = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert!(after.remote_id.is_none());
    assert_eq!(after.consecutive_failures, 1);
    assert_eq!(after.next_poll_at, T0 + 1_000);
    clock.advance(1_000);
    let after = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert!(after.remote_id.is_some());
    assert_eq!(after.start_attempts, 2);
    assert_eq!((h.sim.receipts(&key), h.sim.effects(&key)), (2, 1));
    assert_eq!(h.sim.started_tasks(), 1);
    // Same for the answer to input_required.
    clock.advance(1_000);
    let waiting = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!(waiting.state, TaskState::InputRequired);
    h.tasks
        .respond(&task.id, 1, json!({"ok": true}))
        .await
        .unwrap();
    h.sim.lose_next_responses(1);
    let after = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert!(after.input_pending);
    clock.advance(1_000);
    let after = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert!(!after.input_pending);
    let input_key = format!("{}:input:1", task.id);
    assert_eq!(
        (h.sim.receipts(&input_key), h.sim.effects(&input_key)),
        (2, 1)
    );
    assert_eq!(h.sim.inputs(after.remote_id.as_deref().unwrap()).len(), 1);
    assert_eq!(after.state, TaskState::Completed);
    // Resubmitting the same operation key never creates a second effect.
    let again = h
        .tasks
        .submit(submission("send-1", "long_job"))
        .await
        .unwrap();
    assert_eq!(again.id, task.id);
    h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!(h.sim.total_effects(), 2);
    let mut other = submission("send-1", "long_job");
    other.input = json!({"query": "different"});
    assert!(matches!(
        h.tasks.submit(other).await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn unhonoured_cancellation_is_polled_to_its_final_state() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(T0);
    let h = open(dir.path(), clock.clone()).await;
    let mut steps = working(3);
    steps.push(completed(json!({"purchase": "made anyway"})));
    h.sim
        .define("long_job", script(steps, CancelBehavior::Ignore));
    let task = h
        .tasks
        .submit(submission("ignored", "long_job"))
        .await
        .unwrap();
    h.tasks.drive(&h.sim, &task.id).await.unwrap();
    h.tasks.request_cancel(&task.id).await.unwrap();
    // Idempotent request.
    assert_eq!(
        h.tasks.request_cancel(&task.id).await.unwrap().cancel,
        CancelState::Requested
    );
    let task = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!(
        (task.state, task.cancel),
        (TaskState::Working, CancelState::Sent)
    );
    let polls = h.sim.polls();
    let mut task = task;
    for _ in 0..10 {
        if task.state.is_terminal() {
            break;
        }
        clock.advance(1_000);
        task = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    }
    assert!(h.sim.polls() >= polls + 3, "re-polled after the cancel");
    assert_eq!(task.state, TaskState::Completed);
    assert!(task.cancel_unhonored);
    assert_eq!(h.sim.effects(&format!("{}:cancel", task.id)), 1);
    let kinds: Vec<_> = h
        .tasks
        .events(&task.id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"cancel_not_honored".to_string()));

    // A remote that honours late is also polled until it reports cancelled.
    h.sim.define(
        "long_job",
        script(working(30), CancelBehavior::HonorAfterPolls(2)),
    );
    let late = h
        .tasks
        .submit(submission("late", "long_job"))
        .await
        .unwrap();
    h.tasks.drive(&h.sim, &late.id).await.unwrap();
    h.tasks.request_cancel(&late.id).await.unwrap();
    let mut late = h.tasks.drive(&h.sim, &late.id).await.unwrap();
    let mut states = vec![late.state];
    while !late.state.is_terminal() && states.len() < 10 {
        clock.advance(1_000);
        late = h.tasks.drive(&h.sim, &late.id).await.unwrap();
        states.push(late.state);
    }
    assert_eq!(
        states,
        [
            TaskState::Working,
            TaskState::Working,
            TaskState::Working,
            TaskState::Cancelled
        ]
    );
    assert!(!late.cancel_unhonored);
}

#[tokio::test]
async fn transient_backoff_survives_restart_and_budget_marks_uncertainty() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(T0);
    let h = open(dir.path(), clock.clone()).await;
    h.sim.define(
        "long_job",
        script(vec![completed(json!({}))], CancelBehavior::Honor),
    );
    let task = h
        .tasks
        .submit(submission("flaky", "long_job"))
        .await
        .unwrap();
    h.sim.fail_next(3);
    let t = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!((t.consecutive_failures, t.next_poll_at), (1, T0 + 1_000));
    clock.advance(999);
    h.tasks.drive(&h.sim, &task.id).await.unwrap();
    let t = h.tasks.get(&task.id).await.unwrap().unwrap();
    assert_eq!(t.consecutive_failures, 1, "not retried before the backoff");
    clock.advance(1);
    let t = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!((t.consecutive_failures, t.next_poll_at), (2, T0 + 3_000));
    let h = h.restart(dir.path(), clock.clone()).await;
    clock.set(T0 + 3_000);
    let t = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!((t.consecutive_failures, t.next_poll_at), (3, T0 + 7_000));
    clock.set(T0 + 7_000);
    let t = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!(t.state, TaskState::Completed);
    assert_eq!(t.consecutive_failures, 0);

    // Exhausting the budget before the start is confirmed is flagged.
    let doomed = h
        .tasks
        .submit(submission("doomed", "long_job"))
        .await
        .unwrap();
    h.sim.fail_next(100);
    let mut t = doomed;
    for _ in 0..6 {
        clock.advance(20_000);
        t = h.tasks.drive(&h.sim, &t.id).await.unwrap();
    }
    assert_eq!(t.state, TaskState::Failed);
    let failure = t.failure.unwrap();
    assert_eq!(failure.class, FailureClass::Transient);
    assert!(failure.effect_uncertain);
    assert_eq!(t.start_attempts, 5);
    assert_eq!(policy().backoff(1), 1_000);
    assert_eq!(policy().backoff(5), 16_000);
    assert_eq!(policy().backoff(60), 16_000);
}

#[tokio::test]
async fn deadlines_and_early_cancels_cancel_without_extra_effects() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(T0);
    let h = open(dir.path(), clock.clone()).await;
    h.sim
        .define("long_job", script(working(50), CancelBehavior::Honor));
    let early = h
        .tasks
        .submit(submission("early", "long_job"))
        .await
        .unwrap();
    h.tasks.request_cancel(&early.id).await.unwrap();
    let early = h.tasks.drive(&h.sim, &early.id).await.unwrap();
    assert_eq!(early.state, TaskState::Cancelled);
    assert_eq!(h.sim.started_tasks(), 0, "nothing was sent to the remote");

    let mut timed = submission("timed", "long_job");
    timed.timeout_ms = Some(5_000);
    let timed = h.tasks.submit(timed).await.unwrap();
    assert_eq!(timed.deadline_at, Some(T0 + 5_000));
    h.tasks.drive(&h.sim, &timed.id).await.unwrap();
    clock.set(T0 + 4_500);
    h.tasks.drive(&h.sim, &timed.id).await.unwrap();
    assert_eq!(h.tasks.next_wake().await.unwrap(), Some(T0 + 5_000));
    clock.set(T0 + 5_000);
    let driven = h.tasks.drive_due(&h.sim, 10).await.unwrap();
    assert_eq!(driven.len(), 1);
    assert_eq!(driven[0].state, TaskState::Cancelled);
    let kinds: Vec<_> = h
        .tasks
        .events(&timed.id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"deadline_exceeded".to_string()));
    assert_eq!(h.sim.effects(&format!("{}:cancel", timed.id)), 1);
}

/// Wraps the simulation and records a user intent while a remote call is in
/// flight, as a concurrent conversation would.
struct Interleaving<'a> {
    sim: &'a SimulatedCapability,
    tasks: CapabilityTasks,
    id: String,
    during_start: bool,
}
impl CapabilityProvider for Interleaving<'_> {
    async fn start(&self, request: StartRequest) -> Result<RemoteStatus, ProviderError> {
        if self.during_start {
            self.tasks.request_cancel(&self.id).await.unwrap();
        }
        self.sim.start(request).await
    }
    async fn poll(&self, remote_id: &str) -> Result<RemoteStatus, ProviderError> {
        if !self.during_start {
            // Startup recovery racing a driver: the outcome must be dropped.
            self.tasks.recover_interrupted().await.unwrap();
        }
        self.sim.poll(remote_id).await
    }
    async fn provide_input(
        &self,
        remote_id: &str,
        key: &str,
        response: Value,
    ) -> Result<RemoteStatus, ProviderError> {
        self.sim.provide_input(remote_id, key, response).await
    }
    async fn cancel(&self, remote_id: &str, key: &str) -> Result<RemoteStatus, ProviderError> {
        self.sim.cancel(remote_id, key).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intents_during_flight_survive_and_stale_outcomes_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(T0);
    let h = open(dir.path(), clock.clone()).await;
    h.sim
        .define("long_job", script(working(50), CancelBehavior::Honor));
    let task = h
        .tasks
        .submit(submission("racy", "long_job"))
        .await
        .unwrap();
    let provider = Interleaving {
        sim: &h.sim,
        tasks: h.tasks.clone(),
        id: task.id.clone(),
        during_start: true,
    };
    let t = h.tasks.drive(&provider, &task.id).await.unwrap();
    assert!(t.remote_id.is_some());
    assert_eq!(t.cancel, CancelState::Requested, "cancel kept across apply");
    clock.advance(1_000);
    let t = h.tasks.drive(&h.sim, &task.id).await.unwrap();
    assert_eq!(t.state, TaskState::Cancelled);

    let other = h
        .tasks
        .submit(submission("stale", "long_job"))
        .await
        .unwrap();
    h.tasks.drive(&h.sim, &other.id).await.unwrap();
    clock.advance(1_000);
    let provider = Interleaving {
        sim: &h.sim,
        tasks: h.tasks.clone(),
        id: other.id.clone(),
        during_start: false,
    };
    let t = h.tasks.drive(&provider, &other.id).await.unwrap();
    assert!(!t.in_flight);
    let kinds: Vec<_> = h
        .tasks
        .events(&other.id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"resumed_after_restart".to_string()));
    assert_eq!(kinds.last().unwrap(), "stale_outcome_discarded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_drivers_send_one_start_and_concurrency_limits_hold() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(T0);
    let h = open(dir.path(), clock.clone()).await;
    h.sim
        .define("long_job", script(working(50), CancelBehavior::Honor));
    let task = h
        .tasks
        .submit(submission("shared", "long_job"))
        .await
        .unwrap();
    let (a, b, c) = tokio::join!(
        h.tasks.drive(&h.sim, &task.id),
        h.tasks.drive(&h.sim, &task.id),
        h.tasks.drive(&h.sim, &task.id)
    );
    a.unwrap();
    b.unwrap();
    c.unwrap();
    assert_eq!(h.sim.receipts(&format!("{}:start", task.id)), 1);

    // A capability with max_concurrency 1 defers a second start.
    let mut single = excalibur_capabilities::simulated::manifest(1);
    single.version = "1.1.0".into();
    h.tasks.register(single).await.unwrap();
    let mut first = submission("one", "long_job");
    first.capability_version = "1.1.0".into();
    let mut second = submission("two", "long_job");
    second.capability_version = "1.1.0".into();
    let first = h.tasks.submit(first).await.unwrap();
    let second = h.tasks.submit(second).await.unwrap();
    // The 1.0.0 task above counts toward the per-identity concurrency limit,
    // so cancel it first to isolate the 1.1.0 check.
    h.tasks.request_cancel(&task.id).await.unwrap();
    clock.advance(1_000);
    h.tasks.drive(&h.sim, &task.id).await.unwrap();
    h.tasks.drive(&h.sim, &first.id).await.unwrap();
    let deferred = h.tasks.drive(&h.sim, &second.id).await.unwrap();
    assert!(deferred.remote_id.is_none());
    assert_eq!(deferred.start_attempts, 0);
    h.tasks.request_cancel(&first.id).await.unwrap();
    clock.advance(1_000);
    assert_eq!(
        h.tasks.drive(&h.sim, &first.id).await.unwrap().state,
        TaskState::Cancelled
    );
    clock.advance(1_000);
    let started = h.tasks.drive(&h.sim, &second.id).await.unwrap();
    assert!(started.remote_id.is_some());
}

#[tokio::test]
async fn undeclared_states_rejected_answers_and_scopes_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(T0);
    let h = open(dir.path(), clock.clone()).await;
    let mut strict = excalibur_capabilities::simulated::manifest(4);
    strict.version = "2.0.0".into();
    strict.states.retain(|s| *s != TaskState::InputRequired);
    h.tasks.register(strict.clone()).await.unwrap();
    // Same identity and version with another definition is a conflict.
    let mut changed = strict.clone();
    changed.max_concurrency = 9;
    assert!(matches!(
        h.tasks.register(changed).await,
        Err(Error::Conflict(_))
    ));
    h.sim.define(
        "long_job",
        script(
            vec![
                SimStep::InputRequired {
                    request: json!({"kind": "login"}),
                },
                SimStep::Working,
            ],
            CancelBehavior::Honor,
        ),
    );
    let mut sub = submission("strict", "long_job");
    sub.capability_version = "2.0.0".into();
    let t = h.tasks.submit(sub).await.unwrap();
    let t = h.tasks.drive(&h.sim, &t.id).await.unwrap();
    assert_eq!(t.state, TaskState::Failed);
    assert_eq!(t.failure.unwrap().class, FailureClass::Permanent);

    // A rejected answer raises a new request generation instead of failing.
    let t = h
        .tasks
        .submit(submission("answer", "long_job"))
        .await
        .unwrap();
    h.tasks.drive(&h.sim, &t.id).await.unwrap();
    let t = h
        .tasks
        .respond(&t.id, 1, json!({"code": "1"}))
        .await
        .unwrap();
    // Same answer again is idempotent; a different one conflicts.
    h.tasks
        .respond(&t.id, 1, json!({"code": "1"}))
        .await
        .unwrap();
    assert!(matches!(
        h.tasks.respond(&t.id, 1, json!({"code": "2"})).await,
        Err(Error::Conflict(_))
    ));
    let t = h.tasks.drive(&h.sim, &t.id).await.unwrap();
    assert_eq!((t.state, t.input_seq), (TaskState::Working, 1));

    h.sim.define(
        "lookup",
        script(
            vec![
                SimStep::Working,
                SimStep::InputRequired {
                    request: json!({"kind": "otp"}),
                },
            ],
            CancelBehavior::Honor,
        ),
    );
    let r = h
        .tasks
        .submit(submission("rejecting", "lookup"))
        .await
        .unwrap();
    h.tasks.drive(&h.sim, &r.id).await.unwrap();
    // The remote is still `working`, so the answer recorded against a
    // previous request is rejected by the remote when delivered.
    clock.advance(1_000);
    let r = h.tasks.drive(&h.sim, &r.id).await.unwrap();
    assert_eq!(r.state, TaskState::Working);
    clock.advance(1_000);
    let r = h.tasks.drive(&h.sim, &r.id).await.unwrap();
    assert_eq!((r.state, r.input_seq), (TaskState::InputRequired, 1));

    // Another scope never sees or drives this scope's tasks.
    let other = CapabilityTasks::open(
        h.db.clone(),
        "principal:bob/project:a".into(),
        policy(),
        clock.clone(),
    )
    .await
    .unwrap();
    assert!(other.get(&r.id).await.unwrap().is_none());
    assert!(matches!(
        other.respond(&r.id, 1, json!({})).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        other.drive(&h.sim, &r.id).await,
        Err(Error::NotFound)
    ));
    assert_ne!(
        task_id("principal:bob/project:a", "rejecting"),
        task_id(SCOPE, "rejecting")
    );
    let mut unknown = submission("unknown", "missing_op");
    unknown.capability = "sim.capability".into();
    assert!(matches!(
        h.tasks.submit(unknown).await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn rejected_input_asks_again_and_schema_checksum_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::new(T0);
    let h = open(dir.path(), clock.clone()).await;
    h.sim.define(
        "long_job",
        script(
            vec![SimStep::InputRequired {
                request: json!({"kind": "otp"}),
            }],
            CancelBehavior::Honor,
        ),
    );
    let t = h.tasks.submit(submission("otp", "long_job")).await.unwrap();
    h.tasks.drive(&h.sim, &t.id).await.unwrap();
    h.tasks
        .respond(&t.id, 1, json!({"otp": "000000"}))
        .await
        .unwrap();
    // The remote cancels underneath us: the next delivery is rejected.
    let remote = h
        .tasks
        .get(&t.id)
        .await
        .unwrap()
        .unwrap()
        .remote_id
        .unwrap();
    h.sim.cancel(&remote, "external-cancel").await.unwrap();
    let t = h.tasks.drive(&h.sim, &t.id).await.unwrap();
    assert_eq!(
        (t.state, t.input_seq, t.input_pending),
        (TaskState::InputRequired, 2, false)
    );
    clock.advance(1_000);
    let t = h.tasks.drive(&h.sim, &t.id).await.unwrap();
    assert_eq!(t.state, TaskState::Cancelled);

    let Harness { db, tasks, .. } = h;
    drop(tasks);
    db.write(excalibur_store::Priority::Interactive, |c| {
        c.execute(
            "UPDATE capability_schema SET checksum='tampered' WHERE version=1",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(
        CapabilityTasks::open(db.clone(), SCOPE.into(), policy(), clock.clone())
            .await
            .is_err()
    );
}

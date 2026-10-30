mod support;
use excalibur_engine::Status;
use excalibur_orchestrator::{Control, recovery::BootRecord};
use excalibur_security::{Autonomy, Redactor, RunSecurity, Security};
use excalibur_store::{Priority, ReadLane};
use excalibur_types::runtime::{Effect, PreparedTool, ToolCall};
use serde_json::json;
use support::*;
fn delegation(operation: &str, scope: &str, seconds: u64) -> excalibur_orchestrator::Delegation {
    let mut d = support::delegation(operation, scope, seconds);
    d.profile = "coordinator".into();
    d
}

async fn authorize(f: &Fixture, run: &str, autonomy: Autonomy) {
    let security = Security::new(f.db.clone(), token(), Redactor::default())
        .await
        .unwrap();
    security
        .register_run(
            run,
            RunSecurity {
                parent: None,
                project: "general".into(),
                workspace: f.dir.path().canonicalize().unwrap(),
                token: token(),
                autonomy: vec![autonomy],
                full_acknowledged: false,
                user_requested: true,
            },
            excalibur_store::unix_seconds(),
        )
        .await
        .unwrap();
}
async fn start(f: &Fixture, run: &str, key: &str) {
    f.host
        .engine()
        .journal()
        .set_state(run, "running", None, key)
        .await
        .unwrap();
}
async fn boot(f: &Fixture, at: i64, autonomy: Autonomy) -> BootRecord {
    let b = f.host.recovery().begin(at).await.unwrap();
    f.host.recover_startup(&b, autonomy).await.unwrap();
    b
}
#[tokio::test]
async fn three_real_boot_receipts_gate_dispatch_and_quarantine_only_the_repeated_step() {
    let f = fixture().await;
    let at = excalibur_store::unix_seconds();
    let a = f
        .host
        .spawn(&f.master.id, delegation("suspect", "general", 300))
        .await
        .unwrap();
    let run = a.run_id.as_ref().unwrap();
    authorize(&f, run, Autonomy::Autonomous).await;
    for n in 0..3 {
        boot(&f, at + n, Autonomy::Autonomous).await;
        start(&f, run, &format!("active:{n}")).await;
        assert!(f.host.recovery().track_admission(run).await.unwrap());
    }
    let fourth = boot(&f, at + 3, Autonomy::Autonomous).await;
    let s = f.host.recovery().status(at + 3).await.unwrap();
    assert!(s.safe_mode);
    assert_eq!(s.recent_crashes, 3);
    assert_eq!(s.quarantined, vec![run.clone()]);
    let snapshot = f.host.engine().load(run).await.unwrap();
    assert_eq!(snapshot.status, Status::Blocked);
    assert!(
        snapshot
            .recovery_note
            .as_deref()
            .unwrap()
            .contains("quarantined")
    );
    assert!(f.host.tick().await.unwrap().is_empty());
    assert_eq!(f.host.status(&a.id).await.unwrap().state, "blocked");
    // Exiting global safe mode never clears a per-run quarantine.
    f.host
        .recovery()
        .exit_safe_mode("human-exit", at + 4)
        .await
        .unwrap();
    assert!(!f.host.recovery().track_admission(run).await.unwrap());
    f.host
        .release_recovery_quarantine("human-release", run)
        .await
        .unwrap();
    assert_eq!(
        f.host.engine().load(run).await.unwrap().status,
        Status::Paused
    );
    assert!(
        !f.host
            .recovery()
            .status(at + 4)
            .await
            .unwrap()
            .quarantined
            .contains(run)
    );
    f.host
        .control_keyed("explicit-resume", &a.id, Control::Resume)
        .await
        .unwrap();
    assert_eq!(f.host.tick().await.unwrap(), vec![a.id]);
    f.host.shutdown().await.unwrap();
    f.host
        .recovery()
        .clean_exit(&fourth.id, at + 5)
        .await
        .unwrap();
    let clean = f.host.recovery().begin(at + 6).await.unwrap();
    assert!(!clean.unclean);
    let s = f.host.recovery().status(at + 6).await.unwrap();
    assert!(!s.safe_mode);
    assert!(s.quarantined.is_empty());
    assert_eq!(s.recent_crashes, 3);
    f.db.shutdown().await.unwrap();
}
#[tokio::test]
async fn safe_mode_expires_only_after_thirty_minutes_and_controls_do_not_replay_across_boots() {
    let f = fixture().await;
    let at = excalibur_store::unix_seconds();
    for n in 0..4 {
        boot(&f, at + n, Autonomy::Autonomous).await;
    }
    let s = f.host.recovery().status(at + 3).await.unwrap();
    assert!(s.safe_mode);
    assert_eq!(s.safe_until, at + 3 + 1800);
    assert!(f.host.recovery().status(at + 1802).await.unwrap().safe_mode);
    assert!(!f.host.recovery().status(at + 1803).await.unwrap().safe_mode);
    f.host
        .recovery()
        .exit_safe_mode("exit-once", at + 4)
        .await
        .unwrap();
    boot(&f, at + 5, Autonomy::Autonomous).await;
    assert!(f.host.recovery().status(at + 5).await.unwrap().safe_mode);
    f.host
        .recovery()
        .exit_safe_mode("exit-once", at + 6)
        .await
        .unwrap();
    assert!(
        f.host.recovery().status(at + 6).await.unwrap().safe_mode,
        "old receipt must not exit a later safe-mode generation"
    );
    assert!(
        f.host
            .recovery()
            .release_quarantine("exit-once", "other", at + 7)
            .await
            .is_err()
    );
    f.db.shutdown().await.unwrap();
}
#[tokio::test]
async fn recovery_preserves_user_pause_approval_wait_and_blocks_unresolved_effects() {
    let f = fixture().await;
    let at = excalibur_store::unix_seconds();
    boot(&f, at, Autonomy::Autonomous).await;
    let user = f
        .host
        .spawn(&f.master.id, delegation("user", "general", 300))
        .await
        .unwrap();
    f.host
        .engine()
        .pause(user.run_id.as_ref().unwrap())
        .await
        .unwrap();
    let approval = f
        .host
        .spawn(&f.master.id, delegation("approval", "general", 300))
        .await
        .unwrap();
    let approval_run = approval.run_id.as_ref().unwrap();
    start(&f, approval_run, "approval:running").await;
    let _security = Security::new(f.db.clone(), token(), Redactor::default())
        .await
        .unwrap();
    let approval_owner = approval_run.clone();
    f.db.write(Priority::Journal,move|c|{
        c.execute("INSERT INTO approvals(id,run_id,content) VALUES('pending-startup',?1,'literal approval request')",[&approval_owner])?;
        c.execute("INSERT INTO security_approvals(id,run_id,project,action_hash,request,created_at,expires_at,status,scope) VALUES('pending-startup',?1,'general','synthetic-hash','{}',1,9999999999,'pending','once')",[approval_owner])?;
        Ok(())
    }).await.unwrap();
    let effect = f
        .host
        .spawn(&f.master.id, delegation("effect", "general", 300))
        .await
        .unwrap();
    let effect_run = effect.run_id.as_ref().unwrap();
    start(&f, effect_run, "effect:running").await;
    let prepared = PreparedTool {
        call: ToolCall {
            id: "external".into(),
            name: "irreversible".into(),
            arguments: "{}".into(),
        },
        effect: Effect::NonIdempotent,
        resources: vec![],
        exclusive: true,
        data: json!({}),
    };
    f.host
        .engine()
        .journal()
        .append(
            effect_run,
            "tool_intent",
            "uncertain",
            json!({"index":0,"prepared":prepared}),
        )
        .await
        .unwrap();
    boot(&f, at + 1, Autonomy::Autonomous).await;
    assert_eq!(
        f.host
            .engine()
            .load(user.run_id.as_ref().unwrap())
            .await
            .unwrap()
            .reason
            .as_deref(),
        Some("user")
    );
    assert_eq!(
        f.host
            .engine()
            .load(approval_run)
            .await
            .unwrap()
            .reason
            .as_deref(),
        Some("approval required")
    );
    let approval_fact:(String,String,String,bool)=f.db.read(ReadLane::Interactive,|c|Ok(c.query_row("SELECT a.content,s.request,s.status,s.consumed FROM security_approvals s JOIN approvals a ON a.id=s.id WHERE s.id='pending-startup'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?)).await.unwrap();
    assert_eq!(
        approval_fact,
        (
            "literal approval request".into(),
            "{}".into(),
            "pending".into(),
            false
        )
    );
    let uncertain = f.host.engine().load(effect_run).await.unwrap();
    assert_eq!(uncertain.status, Status::Blocked);
    assert!(uncertain.reason.unwrap().contains("unresolved effect"));
    assert!(f.host.tick().await.unwrap().is_empty());
    f.db.shutdown().await.unwrap();
}
#[tokio::test]
async fn frozen_autonomy_and_six_hour_bound_are_both_required_for_restart_admission() {
    for (autonomy, downtime, expected) in [
        (Autonomy::Autonomous, 30, Status::Running),
        (Autonomy::Supervised, 30, Status::Paused),
        (Autonomy::Autonomous, 21600, Status::Paused),
    ] {
        let f = fixture().await;
        let at = excalibur_store::unix_seconds();
        boot(&f, at, Autonomy::Autonomous).await;
        let a = f
            .host
            .spawn(&f.master.id, delegation("resume-policy", "general", 300))
            .await
            .unwrap();
        let run = a.run_id.as_ref().unwrap();
        authorize(&f, run, autonomy).await;
        start(&f, run, "policy:running").await;
        let mut b = f.host.recovery().begin(at + 1).await.unwrap();
        b.downtime_seconds = downtime;
        f.host
            .recover_startup(&b, Autonomy::Autonomous)
            .await
            .unwrap();
        assert_eq!(f.host.engine().load(run).await.unwrap().status, expected);
        f.db.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn safe_mode_pauses_owned_work_but_never_admits_a_new_queued_run() {
    let f = fixture().await;
    let at = excalibur_store::unix_seconds();
    for n in 0..4 {
        boot(&f, at + n, Autonomy::Autonomous).await;
    }
    let a = f
        .host
        .spawn(&f.master.id, delegation("queued-in-safe", "general", 300))
        .await
        .unwrap();
    assert!(f.host.tick().await.unwrap().is_empty());
    assert_eq!(f.host.status(&a.id).await.unwrap().state, "ready");
    f.host
        .control_keyed("user-safe-pause", &a.id, Control::Pause)
        .await
        .unwrap();
    f.host
        .recovery()
        .exit_safe_mode("safe-exit", at + 5)
        .await
        .unwrap();
    assert!(f.host.tick().await.unwrap().is_empty());
    assert_eq!(
        f.host.status(&a.id).await.unwrap().reason.as_deref(),
        Some("user")
    );
    f.db.shutdown().await.unwrap();
}
#[tokio::test]
async fn schema_checksum_is_additive_and_tampering_fails_closed() {
    let f = fixture().await;
    f.db.write(Priority::Journal, |c| {
        c.execute(
            "UPDATE daemon_recovery_schema SET checksum='tampered' WHERE version=1",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(
        excalibur_orchestrator::recovery::RecoveryLedger::open(f.db.clone())
            .await
            .is_err()
    );
    let n: i64 =
        f.db.read(ReadLane::Interactive, |c| {
            Ok(
                c.query_row("SELECT count(*) FROM orchestration_schema", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert!(n >= 2);
    f.db.shutdown().await.unwrap();
}

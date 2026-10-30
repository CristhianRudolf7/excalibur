mod support;
use excalibur_orchestrator::*;
use excalibur_store::{Priority, ReadLane};
use support::*;
#[tokio::test]
async fn additive_control_timer_migration_preserves_original_checksum_and_existing_agents() {
    let f = fixture().await;
    let p = f
        .host
        .spawn(&f.master.id, delegation("p", "general", 300))
        .await
        .unwrap();
    let original =
        f.db.read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT checksum FROM orchestration_schema WHERE version=1",
                [],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    f.db.write(Priority::Journal,|c|{c.execute_batch("DROP TABLE orchestration_question_inputs;DROP TABLE orchestration_question_deadlines;DROP TABLE orchestration_directive_clocks;DROP TABLE orchestration_admissions;DELETE FROM orchestration_schema WHERE version=2;")?;Ok(())}).await.unwrap();
    let reopened = Orchestrator::open(f.db.clone(), f.host.engine().clone(), Limits::default())
        .await
        .unwrap();
    assert_eq!(reopened.agent(&p.id).await.unwrap().token, p.token);
    let checksum =
        f.db.read(ReadLane::Interactive, |c| {
            // Version 3 (host notification targets) must survive reapplying version 2.
            let versions = c
                .prepare("SELECT version FROM orchestration_schema ORDER BY version")?
                .query_map([], |r| r.get::<_, u64>(0))?
                .collect::<std::result::Result<Vec<u64>, excalibur_store::rusqlite::Error>>()?;
            assert_eq!(versions, [1, 2, 3]);
            Ok(c.query_row(
                "SELECT checksum FROM orchestration_schema WHERE version=1",
                [],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(checksum, original);
    f.db.write(Priority::Journal, |c| {
        c.execute(
            "UPDATE orchestration_schema SET checksum='invalid' WHERE version=2",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(
        Orchestrator::open(f.db.clone(), f.host.engine().clone(), Limits::default())
            .await
            .is_err()
    );
}

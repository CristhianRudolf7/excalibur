use excalibur_journal::Journal;
use excalibur_security::*;
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use serde_json::json;
#[tokio::test]
async fn exact_host_verification_checks_require_read_only_offline_authority_and_keep_denials() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let db = Database::open(
        root.join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let mut token = CapabilityToken {
        roots: vec![Root {
            path: root.clone(),
            write: false,
        }],
        network: Network::None,
        processes: true,
        sandbox_profile: Some("read-only".into()),
        budget: 20,
        ..Default::default()
    };
    let command = "python3 -c 'print(42)'";
    let security = Security::new(db.clone(), token.clone(), Redactor::default())
        .await
        .unwrap()
        .with_noninteractive_policy()
        .with_verification_checks(
            [command.to_string(), "curl https://example.invalid".into()].into(),
        )
        .unwrap();
    let run = Journal::new(db.clone())
        .await
        .unwrap()
        .create_run("readonly-verifier", json!({}))
        .await
        .unwrap();
    security
        .register_run(
            &run,
            RunSecurity {
                parent: None,
                project: "verification".into(),
                workspace: root,
                token: token.clone(),
                autonomy: vec![Autonomy::Full],
                full_acknowledged: true,
                user_requested: true,
            },
            100,
        )
        .await
        .unwrap();
    security
        .mark_untrusted(&run, "read result", 101)
        .await
        .unwrap();
    assert!(matches!(
        security
            .authorize(
                &run,
                &Action::Exec {
                    command: command.into()
                },
                102
            )
            .await
            .unwrap(),
        Decision::Allow
    ));
    assert!(matches!(
        security
            .authorize(
                &run,
                &Action::Exec {
                    command: "python3 -c 'print(43)'".into()
                },
                102
            )
            .await
            .unwrap(),
        Decision::Deny { .. }
    ));
    assert!(matches!(
        security
            .authorize(
                &run,
                &Action::Exec {
                    command: "curl https://example.invalid".into()
                },
                102
            )
            .await
            .unwrap(),
        Decision::Deny { .. }
    ));
    assert!(!security.pending_approval(&run, 102).await.unwrap());
    security
        .add_rule(
            Rule {
                id: "deny-verifier-command".into(),
                action: "exec".into(),
                pattern: "**".into(),
                effect: RuleEffect::Deny,
                run: Some(run.clone()),
                project: None,
                expires_at: None,
            },
            true,
            103,
        )
        .await
        .unwrap();
    assert!(matches!(
        security
            .authorize(
                &run,
                &Action::Exec {
                    command: command.into()
                },
                104
            )
            .await
            .unwrap(),
        Decision::Deny { .. }
    ));

    token.roots[0].write = true;
    assert!(
        Security::new(db.clone(), token, Redactor::default())
            .await
            .unwrap()
            .with_verification_checks([command.into()].into())
            .is_err()
    );
    db.shutdown().await.unwrap();
}

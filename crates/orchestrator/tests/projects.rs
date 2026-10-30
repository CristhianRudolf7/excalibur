mod support;
use excalibur_orchestrator::*;
use std::collections::BTreeSet;
use support::*;
#[tokio::test]
async fn nested_roots_resolve_all_candidates_and_scope_reads_are_explicit() {
    let f = fixture().await;
    let nested = f.dir.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let file = nested.join("file");
    std::fs::write(&file, "Synthetic").unwrap();
    for (id, root) in [("outer", f.dir.path()), ("inner", nested.as_path())] {
        f.host
            .register_project(Project {
                id: id.into(),
                name: id.into(),
                aliases: vec![],
                description: String::new(),
                roots: vec![root.into()],
                status: "active".into(),
                vcs: None,
                settings: serde_json::json!({}),
            })
            .await
            .unwrap();
    }
    let resolved = f.host.resolve_path(&file, None, None).await.unwrap();
    assert_eq!(resolved.chosen.as_deref(), Some("inner"));
    assert_eq!(resolved.candidates.len(), 2);
    assert_eq!(
        f.host
            .resolve_path(&file, Some("outer"), Some("inner"))
            .await
            .unwrap()
            .chosen
            .as_deref(),
        Some("outer")
    );
    assert!(
        f.host
            .scoped_agents(&f.master.id, &BTreeSet::new())
            .await
            .is_err()
    );
    assert!(
        f.host
            .scoped_agents(&f.master.id, &BTreeSet::from(["project:inner".into()]))
            .await
            .is_err()
    );
    #[cfg(unix)]
    {
        let link = f.dir.path().join("link");
        std::os::unix::fs::symlink(&nested, &link).unwrap();
        assert_eq!(
            f.host
                .resolve_path(&link.join("file"), None, None)
                .await
                .unwrap()
                .chosen
                .as_deref(),
            Some("inner")
        );
    }
}

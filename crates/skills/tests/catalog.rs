use excalibur_skills::{
    builtin::BUILTIN_SKILLS,
    format::parse_skill,
    groups::{CatalogScope, TaskKind},
    index::{SkillIndex, SkillRoot},
    session::SkillSession,
};
use std::{collections::BTreeSet, fs};
fn skill(name: &str, body: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: Investigate bugs and verify root causes\nallowed-tools: read_file\nmetadata:\n  excalibur.group: debugging\n---\n# Debugging\n{body}\n"
    )
}
fn write(root: &std::path::Path, name: &str, body: &str) {
    fs::create_dir_all(root.join(name)).unwrap();
    fs::write(root.join(name).join("SKILL.md"), skill(name, body)).unwrap();
}
#[test]
fn integrated_catalog_is_valid_and_unique() {
    let mut names = BTreeSet::new();
    for (name, content) in BUILTIN_SKILLS {
        assert!(names.insert(*name));
        parse_skill(name, content).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    }
    assert_eq!(names.len(), 24);
}
#[test]
fn invalid_manifests_are_rejected() {
    assert!(parse_skill("other", &skill("valid-name", "body")).is_err());
    assert!(parse_skill("Bad", &skill("Bad", "body")).is_err());
    let long = skill("valid", "body")
        .replace("Investigate bugs and verify root causes", &"x".repeat(1025));
    assert!(parse_skill("valid", &long).is_err());
    let long = skill("valid", "body").replace(
        "allowed-tools:",
        &format!("compatibility: {}\nallowed-tools:", "x".repeat(501)),
    );
    assert!(parse_skill("valid", &long).is_err());
}
#[tokio::test]
async fn precedence_freezing_deltas_and_authority() {
    let project = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    write(project.path(), "debug", "project body");
    write(user.path(), "debug", "user body");
    let index = SkillIndex::new(vec![
        SkillRoot::user(user.path()),
        SkillRoot {
            kind: excalibur_skills::index::RootKind::Project,
            path: Some(project.path().into()),
        },
    ]);
    index.refresh().await;
    assert_eq!(index.snapshot().conflicts.len(), 1);
    let scope = CatalogScope::new(TaskKind::Programming, "implementer");
    let mut session = SkillSession::new(&index, &scope, 200_000, |s| s.chars().count());
    let frozen = session.catalog.clone();
    let loaded = session.load(&index, "debug", |_| Ok(())).unwrap();
    assert!(loaded.body.contains("project body"));
    let authority = BTreeSet::from(["read_file".into(), "write_file".into()]);
    assert_eq!(
        session.allowed_tools(&authority),
        BTreeSet::from(["read_file".into()])
    );
    write(project.path(), "debug", "changed body");
    index.refresh().await;
    assert_eq!(session.catalog, frozen);
    assert_eq!(session.deltas(&index).len(), 1);
    assert!(session.load(&index, "debug", |_| Ok(())).is_err());
    assert_eq!(index.stats("debug").loads, 1);
}
#[tokio::test]
async fn catalog_budget_suggestions_and_quarantine() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "danger", "not audited");
    let index = SkillIndex::new(vec![
        SkillRoot::builtin(),
        SkillRoot::installed(root.path()),
    ]);
    index.refresh().await;
    assert!(
        index.snapshot().diagnostics.is_empty(),
        "{:?}",
        index.snapshot().diagnostics
    );
    assert!(!index.get("danger").unwrap().usable());
    let scope = CatalogScope::new(TaskKind::Programming, "implementer");
    let mut session = SkillSession::new(&index, &scope, 10_000, |s| s.len());
    assert!(session.catalog.len() <= 200);
    assert!(
        session
            .suggestions(&index, "verify debugging tests", "mock")
            .len()
            <= 3
    );
    assert!(session.load(&index, "danger", |_| Ok(())).is_err());
    assert!(
        session
            .load(&index, "task-verification", |_| Err(
                "unresolved tool".into()
            ))
            .is_err()
    );
    assert_eq!(index.stats("task-verification").loads, 0);
}
#[cfg(unix)]
#[tokio::test]
async fn escaping_resources_are_excluded() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(root.path(), "debug", "body");
    fs::write(outside.path().join("secret"), "private").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret"),
        root.path().join("debug/reference"),
    )
    .unwrap();
    let index = SkillIndex::new(vec![SkillRoot::user(root.path())]);
    index.refresh().await;
    assert!(index.get("debug").is_none());
    assert_eq!(index.snapshot().diagnostics.len(), 1);
}

#[tokio::test]
async fn periodic_watcher_recovers_new_entries_and_stops() {
    use excalibur_skills::watcher::SkillWatcher;
    use std::{sync::Arc, time::Duration};
    let root = tempfile::tempdir().unwrap();
    let index = Arc::new(SkillIndex::new(vec![SkillRoot::user(root.path())]));
    let watcher = SkillWatcher::with_interval(index.clone(), Duration::from_millis(25)).unwrap();
    write(root.path(), "debug", "new body");
    tokio::time::timeout(Duration::from_secs(3), async {
        while index.get("debug").is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(watcher);
}

#[tokio::test]
async fn embeddings_are_reused_and_refreshed_on_resource_changes() {
    use excalibur_skills::index::SkillEmbedder;
    use excalibur_types::runtime::BoxFuture;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Embeddings(AtomicUsize);
    impl SkillEmbedder for Embeddings {
        fn model(&self) -> &str {
            "local-deterministic"
        }
        fn embed<'a>(&'a self, _text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, String>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(vec![1.0, 0.0]) })
        }
    }
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "debug", "body");
    let embedder = Arc::new(Embeddings(AtomicUsize::new(0)));
    let index = SkillIndex::new(vec![SkillRoot::user(root.path())]).with_embedder(embedder.clone());
    index.refresh().await;
    assert!(index.snapshot().has_vector("debug"));
    index.refresh().await;
    assert_eq!(embedder.0.load(Ordering::SeqCst), 1);
    fs::write(root.path().join("debug/reference.md"), "new evidence").unwrap();
    assert_eq!(index.refresh().await.changed, vec!["debug"]);
    assert_eq!(embedder.0.load(Ordering::SeqCst), 2);
    let hits = index.search("unrelated", None, 3).await.unwrap();
    assert_eq!(hits[0].name, "debug");
    assert_eq!(hits[0].cosine, Some(1.0));
}

#[tokio::test]
async fn oversized_resources_are_rejected_before_indexing() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "debug", "body");
    fs::File::create(root.path().join("debug/large.bin"))
        .unwrap()
        .set_len(17 * 1024 * 1024)
        .unwrap();
    let index = SkillIndex::new(vec![SkillRoot::user(root.path())]);
    index.refresh().await;
    assert!(index.get("debug").is_none());
    assert!(index.snapshot().diagnostics[0].errors[0].contains("16 MiB"));
}

#[tokio::test]
async fn negative_outcomes_filter_only_the_measured_model() {
    let index = SkillIndex::new(vec![SkillRoot::builtin()]);
    index.refresh().await;
    let relevant = BTreeSet::from(["debugging".into()]);
    for _ in 0..20 {
        index.record_task_outcome("measured", &relevant, &["root-cause-first".into()], false);
        index.record_task_outcome("measured", &relevant, &[], true);
    }
    assert!(
        index
            .disabled_groups("measured", 20, 0.05)
            .contains("debugging")
    );
    assert!(index.disabled_groups("other", 20, 0.05).is_empty());
    let session = SkillSession::new(
        &index,
        &CatalogScope::new(TaskKind::Programming, "implementer"),
        200_000,
        |s| s.len(),
    );
    assert!(
        session
            .suggestions(&index, "root cause debugging", "measured")
            .iter()
            .all(|h| h.group != "debugging")
    );
    assert!(
        session
            .suggestions(&index, "root cause debugging", "other")
            .iter()
            .any(|h| h.group == "debugging")
    );
}

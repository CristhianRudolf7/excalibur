use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::{ToolCall, ToolRuntime};
use excalibur_vault::{Redactor, SecretValue};
use serde_json::{Value, json};
fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}
async fn fixture(redactor: Redactor) -> (tempfile::TempDir, Database, LocalTools) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    db.write(excalibur_store::Priority::Normal, |c| {
        c.execute("INSERT INTO runs(id) VALUES('run')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let tools = LocalTools::new(
        db.clone(),
        root,
        ToolPolicy {
            write: Access::Allow,
            redactor,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    (temp, db, tools)
}
#[tokio::test]
async fn successful_edits_publish_actual_unified_diffs() {
    let (temp, db, tools) = fixture(Redactor::default()).await;
    let path = temp.path().join("work/file.txt");
    std::fs::write(&path, "synthetic before\n").unwrap();
    let read = tools
        .prepare("run", call("read", "read_file", json!({"path":"file.txt"})))
        .await
        .unwrap();
    assert!(tools.execute("run", "read", &read).await.success);
    let prepared=tools.prepare("run",call("edit","edit_file",json!({"path":"file.txt","old_text":"synthetic before","new_text":"synthetic after"}))).await.unwrap();
    let result = tools.execute("run", "edit", &prepared).await;
    assert!(result.success, "{result:?}");
    let report = &result.output["files"][0];
    let diff = report["diff"].as_str().unwrap();
    assert!(diff.contains("-synthetic before") && diff.contains("+synthetic after"));
    assert_eq!(report["path"], "file.txt");
    assert_eq!(report["diff_truncated"], false);
    assert_eq!(report["diff_bytes"], diff.len());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "synthetic after\n");
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn whole_multiline_values_are_redacted_before_diff_prefixes_and_truncation() {
    let redactor = Redactor::default();
    let (temp, db, tools) = fixture(redactor.clone()).await;
    let secret = "SYNTHETIC_SEGMENT_ONE\nSYNTHETIC_SEGMENT_TWO\nSYNTHETIC_SEGMENT_THREE";
    let before = format!(
        "{}\n{secret}\n{}",
        "before line\n".repeat(400),
        "before tail\n".repeat(400)
    );
    std::fs::write(temp.path().join("work/file.txt"), before).unwrap();
    let read = tools
        .prepare("run", call("read", "read_file", json!({"path":"file.txt"})))
        .await
        .unwrap();
    assert!(tools.execute("run", "read", &read).await.success);
    let after = "after synthetic line\n".repeat(800);
    let prepared = tools
        .prepare(
            "run",
            call(
                "write",
                "write_file",
                json!({"path":"file.txt","content":after}),
            ),
        )
        .await
        .unwrap();
    redactor.register_secret(&SecretValue::new(secret.as_bytes().to_vec()).unwrap());
    let result = tools.execute("run", "write", &prepared).await;
    assert!(result.success, "{result:?}");
    let report = &result.output["files"][0];
    assert_eq!(report["diff_truncated"], true);
    assert!(report["diff_bytes"].as_u64().unwrap() > 4096);
    assert!(report["diff"].as_str().unwrap().len() <= 4200);
    let full = db
        .read_blob(
            report["diff_blob"].as_str().unwrap().parse().unwrap(),
            1024 * 1024,
        )
        .await
        .unwrap();
    let full = String::from_utf8(full).unwrap();
    assert!(full.contains("+after synthetic line"));
    for part in [
        "SYNTHETIC_SEGMENT_ONE",
        "SYNTHETIC_SEGMENT_TWO",
        "SYNTHETIC_SEGMENT_THREE",
    ] {
        assert!(!full.contains(part));
        assert!(!result.output.to_string().contains(part));
    }
    db.shutdown().await.unwrap();
}

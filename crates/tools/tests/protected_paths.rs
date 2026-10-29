use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::{ToolCall, ToolRuntime};
use serde_json::json;
#[tokio::test]
async fn sensitive_configuration_is_read_only_even_with_write_allow() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let tools = LocalTools::new(
        db.clone(),
        root.clone(),
        ToolPolicy {
            write: Access::Allow,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for path in [
        ".git/config",
        ".git/hooks/pre-commit",
        ".agents/policy",
        ".aws/credentials",
        ".config/gcloud/config",
        ".github/workflows/test.yml",
        ".excalibur/policy",
    ] {
        std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        let call = ToolCall {
            id: path.into(),
            name: "write_file".into(),
            arguments: json!({"path":path,"content":"hostile"}).to_string(),
        };
        assert!(
            tools
                .prepare("run", call)
                .await
                .unwrap_err()
                .contains("read-only")
        );
        assert!(!root.join(path).exists());
    }
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn known_secrets_are_not_written_to_readset_or_full_output_blobs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let secret = b"synthetic-private-value-782613";
    std::fs::write(root.join("data.txt"), secret).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let redactor = excalibur_vault::Redactor::default();
    let value = excalibur_vault::SecretValue::new(secret.to_vec()).unwrap();
    redactor.register_secret(&value);
    let tools = LocalTools::new(
        db.clone(),
        root.clone(),
        ToolPolicy {
            redactor,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let call = ToolCall {
        id: "read".into(),
        name: "read_file".into(),
        arguments: json!({"path":"data.txt"}).to_string(),
    };
    let prepared = tools.prepare("run", call).await.unwrap();
    let result = tools.execute("run", "read", &prepared).await;
    assert!(!result.success);
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains(std::str::from_utf8(secret).unwrap())
    );
    let count = db
        .read(excalibur_store::ReadLane::Interactive, |c| {
            Ok(c.query_row("SELECT count(*) FROM tool_readset", [], |r| {
                r.get::<_, i64>(0)
            })?)
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn protected_vault_is_excluded_before_reads_and_searches() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let vault = root.join("vault.bin");
    std::fs::write(&vault, "ciphertext forbidden-match").unwrap();
    std::fs::write(root.join("public.txt"), "public forbidden-match").unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let tools = LocalTools::new(
        db.clone(),
        root.clone(),
        ToolPolicy {
            protected_paths: vec![vault],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        tools
            .prepare(
                "run",
                ToolCall {
                    id: "read".into(),
                    name: "read_file".into(),
                    arguments: json!({"path":"vault.bin"}).to_string()
                }
            )
            .await
            .is_err()
    );
    for (name, args) in [
        ("glob", json!({"pattern":"**/*"})),
        ("grep", json!({"pattern":"forbidden-match"})),
    ] {
        let call = ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: args.to_string(),
        };
        let prepared = tools.prepare("run", call).await.unwrap();
        let result = tools.execute("run", name, &prepared).await;
        assert!(result.success, "{result:?}");
        let text = serde_json::to_string(&result).unwrap();
        assert!(!text.contains("vault.bin"));
        assert!(text.contains("public.txt"));
    }
    db.shutdown().await.unwrap();
}

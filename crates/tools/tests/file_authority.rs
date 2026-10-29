#![cfg(unix)]
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::{ToolCall, ToolRuntime};
use serde_json::{Value, json};
use std::{fs, path::Path};

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.to_string(),
    }
}
async fn database(parent: &Path) -> Database {
    let db = Database::open(
        parent.join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    db.write(Priority::Normal, |connection| {
        connection.execute("INSERT INTO runs(id) VALUES('run')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    db
}
async fn assert_no_private_blobs(db: &Database, forbidden: &[&str]) {
    let hashes = db
        .read(ReadLane::Interactive, |connection| {
            let mut query = connection.prepare("SELECT hash FROM blobs")?;
            let hashes = query
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(hashes)
        })
        .await
        .unwrap();
    for hash in hashes {
        let bytes = db
            .read_blob(hash.parse().unwrap(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            forbidden.iter().all(|secret| !text.contains(secret)),
            "private bytes reached a persisted tool blob"
        );
    }
}

#[tokio::test]
async fn protected_hardlink_aliases_never_enter_reads_searches_readset_or_blobs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(root.join("private")).unwrap();
    let external = temp.path().join("vault.bin");
    fs::write(&external, "{\"fixture\":\"SYNTHETIC_VAULT_PRIVATE\"}\n").unwrap();
    fs::write(
        root.join("private/vault.bin"),
        "{\"fixture\":\"SYNTHETIC_INNER_PRIVATE\"}\n",
    )
    .unwrap();
    fs::hard_link(&external, root.join("external-alias.txt")).unwrap();
    fs::hard_link(root.join("private/vault.bin"), root.join("inner-alias.txt")).unwrap();
    fs::write(root.join("public.txt"), "SYNTHETIC_PUBLIC\n").unwrap();
    fs::hard_link(root.join("public.txt"), root.join("public-link.txt")).unwrap();
    let db = database(temp.path()).await;
    let tools = LocalTools::new(
        db.clone(),
        root.clone(),
        ToolPolicy {
            protected_paths: vec![external, root.join("private")],
            write: Access::Allow,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for (index, path) in [
        root.join("external-alias.txt"),
        root.join("inner-alias.txt"),
    ]
    .iter()
    .enumerate()
    {
        let id = format!("private-{index}");
        let prepared = tools
            .prepare("run", call(&id, "read_file", json!({"path":path})))
            .await
            .unwrap();
        let result = tools.execute("run", &id, &prepared).await;
        assert!(!result.success, "{result:?}");
        assert!(
            result.output["error"]
                .as_str()
                .unwrap()
                .contains("hardlink")
        );
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("SYNTHETIC_")
        );
    }
    for path in ["public.txt", "public-link.txt"] {
        let prepared = tools
            .prepare("run", call(path, "read_file", json!({"path":path})))
            .await
            .unwrap();
        let result = tools.execute("run", path, &prepared).await;
        assert!(
            result.success,
            "internal build hardlinks must remain readable: {result:?}"
        );
    }
    for (name, arguments) in [
        ("grep", json!({"pattern":"SYNTHETIC_"})),
        ("glob", json!({"pattern":"**/*.txt"})),
    ] {
        let prepared = tools
            .prepare("run", call(name, name, arguments))
            .await
            .unwrap();
        let result = tools.execute("run", name, &prepared).await;
        assert!(result.success, "{result:?}");
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(
            !serialized.contains("PRIVATE") && !serialized.contains("alias.txt"),
            "{result:?}"
        );
        assert_eq!(result.output["results"].as_array().unwrap().len(), 2);
    }
    let denied = tools
        .prepare(
            "run",
            call(
                "write-alias",
                "write_file",
                json!({"path":"external-alias.txt","content":"replacement"}),
            ),
        )
        .await
        .unwrap_err();
    assert!(denied.contains("hardlink"));
    let remembered = db
        .read(ReadLane::Interactive, |connection| {
            let mut query = connection.prepare("SELECT path FROM tool_readset")?;
            let paths = query
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(paths)
        })
        .await
        .unwrap();
    assert_eq!(remembered.len(), 2);
    assert!(
        remembered
            .iter()
            .all(|path| path.ends_with("public.txt") || path.ends_with("public-link.txt"))
    );
    assert_no_private_blobs(&db, &["SYNTHETIC_VAULT_PRIVATE", "SYNTHETIC_INNER_PRIVATE"]).await;
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn prepared_read_rechecks_the_opened_file_after_replacement_with_a_private_hardlink() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir(&root).unwrap();
    let external = temp.path().join("vault.bin");
    fs::write(
        &external,
        "{\"fixture\":\"SYNTHETIC_REPLACEMENT_PRIVATE\"}\n",
    )
    .unwrap();
    fs::write(root.join("input.txt"), "public preparation input").unwrap();
    let db = database(temp.path()).await;
    let tools = LocalTools::new(
        db.clone(),
        root.clone(),
        ToolPolicy {
            protected_paths: vec![external.clone()],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let prepared = tools
        .prepare(
            "run",
            call("read", "read_file", json!({"path":"input.txt"})),
        )
        .await
        .unwrap();
    fs::remove_file(root.join("input.txt")).unwrap();
    fs::hard_link(&external, root.join("input.txt")).unwrap();
    let result = tools.execute("run", "read", &prepared).await;
    assert!(!result.success, "{result:?}");
    assert!(
        result.output["error"]
            .as_str()
            .unwrap()
            .contains("hardlink")
    );
    let count = db
        .read(ReadLane::Interactive, |connection| {
            Ok(
                connection.query_row("SELECT count(*) FROM tool_readset", [], |row| {
                    row.get::<_, i64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_no_private_blobs(&db, &["SYNTHETIC_REPLACEMENT_PRIVATE"]).await;
    db.shutdown().await.unwrap();
}

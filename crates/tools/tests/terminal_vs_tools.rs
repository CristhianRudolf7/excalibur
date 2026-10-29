#![cfg(target_os = "linux")]
use excalibur_store::{Database, DatabaseKind, Priority, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::{ToolCall, ToolResult, ToolRuntime};
use serde_json::{Value, json};
async fn invoke(tools: &LocalTools, run: &str, id: &str, name: &str, args: Value) -> ToolResult {
    let prepared = tools
        .prepare(
            run,
            ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: args.to_string(),
            },
        )
        .await
        .unwrap();
    let result = tools.execute(run, id, &prepared).await;
    assert!(result.success, "{name}: {result:?}");
    result
}
#[tokio::test]
async fn synthetic_file_task_compares_real_terminal_and_specific_tools_with_equal_final_data() {
    let temp = tempfile::tempdir().unwrap();
    let specific_root = temp.path().join("specific");
    let terminal_root = temp.path().join("terminal");
    std::fs::create_dir(&specific_root).unwrap();
    std::fs::create_dir(&terminal_root).unwrap();
    excalibur_tools::terminal::prepare_project_boundary(&specific_root).unwrap();
    excalibur_tools::terminal::prepare_project_boundary(&terminal_root).unwrap();
    let source = (0..4000)
        .map(|i| {
            if i == 3210 {
                "record_3210: TODO_SYNTHETIC\n".into()
            } else {
                format!("unrelated_record_{i}: synthetic data for row {i}\n")
            }
        })
        .collect::<String>();
    std::fs::write(specific_root.join("fixture.txt"), &source).unwrap();
    std::fs::write(terminal_root.join("fixture.txt"), &source).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    db.write(Priority::Normal, |c| {
        c.execute(
            "INSERT INTO runs(id) VALUES('specific-run'),('terminal-run')",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let policy = ToolPolicy {
        read: Access::Allow,
        write: Access::Allow,
        exec: Access::Allow,
        ..Default::default()
    };
    let specific = LocalTools::new(db.clone(), specific_root.clone(), policy.clone())
        .await
        .unwrap();
    let terminal = LocalTools::new(db.clone(), terminal_root.clone(), policy)
        .await
        .unwrap();
    for (tools, run) in [(&specific, "specific-run"), (&terminal, "terminal-run")] {
        tools.execution_state(run, "running", None).await.unwrap();
        tools.begin_step(run, 0).await.unwrap();
    }
    let grep = invoke(
        &specific,
        "specific-run",
        "locate",
        "grep",
        json!({"pattern":"TODO_SYNTHETIC","glob":"fixture.txt"}),
    )
    .await;
    let read = invoke(
        &specific,
        "specific-run",
        "read",
        "read_file",
        json!({"path":"fixture.txt","start_line":3208,"line_count":7}),
    )
    .await;
    let edited=invoke(&specific,"specific-run","edit","edit_file",json!({"path":"fixture.txt","old_text":"TODO_SYNTHETIC","new_text":"DONE_SYNTHETIC","fp":read.output["fp"]})).await;
    let cat = invoke(
        &terminal,
        "terminal-run",
        "cat",
        "exec",
        json!({"cmd":"cat fixture.txt","yield_ms":30000}),
    )
    .await;
    assert_eq!(cat.output["exited"], true);
    assert_eq!(cat.output["truncated"], true);
    assert!(cat.full_output_blob.is_some());
    let edit = invoke(
        &terminal,
        "terminal-run",
        "sed",
        "exec",
        json!({"cmd":"sed -i 's/TODO_SYNTHETIC/DONE_SYNTHETIC/' fixture.txt","yield_ms":30000}),
    )
    .await;
    assert_eq!(edit.output["exit_code"], 0);
    for (tools, run) in [(&specific, "specific-run"), (&terminal, "terminal-run")] {
        tools.end_step(run, 0).await.unwrap();
    }
    let specific_bytes = grep.output.to_string().len()
        + read.output.to_string().len()
        + edited.output.to_string().len();
    let terminal_bytes = cat.output.to_string().len() + edit.output.to_string().len();
    assert!(
        specific_bytes < terminal_bytes,
        "specific={specific_bytes}, terminal={terminal_bytes}"
    );
    let expected = source.replace("TODO_SYNTHETIC", "DONE_SYNTHETIC");
    assert_eq!(
        std::fs::read_to_string(specific_root.join("fixture.txt")).unwrap(),
        expected
    );
    assert_eq!(
        std::fs::read_to_string(terminal_root.join("fixture.txt")).unwrap(),
        expected
    );
    // Output bytes are a deterministic context-volume proxy, not tokenizer,
    // model-call, latency or production benchmark measurements.
    eprintln!(
        "{}",
        json!({"fixture":"single_marker_in_4000_rows","specific_calls":3,"terminal_calls":2,"specific_output_bytes":specific_bytes,"terminal_output_bytes":terminal_bytes,"equal_final_data":true,"inference_calls":0})
    );
    specific.terminals().shutdown().await;
    terminal.terminals().shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn terminal_status_exposes_progress_without_an_owner_read_or_wait_call() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    excalibur_tools::terminal::prepare_project_boundary(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    db.write(Priority::Normal, |c| {
        c.execute("INSERT INTO runs(id) VALUES('live-run')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let tools = LocalTools::new(
        db.clone(),
        root,
        ToolPolicy {
            exec: Access::Allow,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tools
        .execution_state("live-run", "running", None)
        .await
        .unwrap();
    tools.begin_step("live-run", 0).await.unwrap();
    let first=invoke(&tools,"live-run","long","exec",json!({"cmd":"printf 'LIVE_PROGRESS\\n'; IFS= read -r line; printf 'DONE:%s\\n' \"$line\"","yield_ms":250})).await;
    assert_eq!(first.output["exited"], false);
    let status = tools.live_status("live-run");
    assert!(
        status[0]["tail"]
            .as_str()
            .unwrap()
            .contains("LIVE_PROGRESS")
    );
    let id = first.output["session_id"].clone();
    invoke(
        &tools,
        "live-run",
        "input",
        "exec_io",
        json!({"action":"write","session_id":id,"text":"finish\n"}),
    )
    .await;
    let done = invoke(
        &tools,
        "live-run",
        "wait",
        "exec_io",
        json!({"action":"wait_for","session_id":id,"pattern":"DONE:finish","timeout_ms":5000}),
    )
    .await;
    assert_eq!(done.output["matched"], true);
    tools.terminals().shutdown().await;
    db.shutdown().await.unwrap();
}

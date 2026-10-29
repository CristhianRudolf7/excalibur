//! Local file effects are prepared against the preceding confirmed batch.
use excalibur_context::ContextInput;
use excalibur_engine::{Budget, Criterion, Engine, RunSpec, Status};
use excalibur_journal::Journal;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_store::{Database, DatabaseKind, ReadLane, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy};
use excalibur_types::runtime::{
    BoxFuture, Message, ModelRequest, ModelResponse, ToolCall, ToolResult, Usage,
};
use serde_json::json;
use std::{path::PathBuf, sync::Arc};

struct SequentialEdits {
    second_path: PathBuf,
}
impl ModelGateway for SequentialEdits {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let results = request
                .messages
                .iter()
                .filter_map(|m| m.tool_call_id.as_deref())
                .collect::<Vec<_>>();
            let calls = if !results.contains(&"initial_read") {
                vec![call("initial_read", "read_file", json!({"path":"f.txt"}))]
            } else if !results.contains(&"edit_first") {
                vec![
                    call(
                        "edit_first",
                        "edit_file",
                        json!({"path":"./f.txt","old_text":"uno","new_text":"UNO"}),
                    ),
                    call(
                        "edit_second",
                        "edit_file",
                        json!({"path":self.second_path,"old_text":"dos","new_text":"DOS"}),
                    ),
                ]
            } else if !results.contains(&"verify_contents") {
                vec![call(
                    "verify_contents",
                    "read_file",
                    json!({"path":"f.txt"}),
                )]
            } else {
                Vec::new()
            };
            let mut message = Message::text(
                "assistant",
                if calls.is_empty() {
                    "Both ordered edits have been checked."
                } else {
                    ""
                },
            );
            message.tool_calls = calls;
            Ok(ModelResponse {
                message,
                usage: Usage::default(),
            })
        })
    }
}
fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_file_edits_with_relative_and_absolute_aliases_prepare_after_prior_postimage() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("f.txt"), b"uno\ndos\n").unwrap();
    let db = Database::open(
        dir.path().join("data/main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let tools = Arc::new(
        LocalTools::new(
            db.clone(),
            workspace.clone(),
            ToolPolicy {
                write: Access::Allow,
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let engine = Engine::new(
        journal.clone(),
        Arc::new(SequentialEdits {
            second_path: workspace.join("f.txt"),
        }),
        tools.clone(),
    );
    let run = engine
        .create(
            "same-file-ordered-edits",
            RunSpec {
                context: ContextInput::new(
                    "Use real file tools and keep requested call order.",
                    "Replace uno and dos with UNO and DOS in f.txt.",
                    "fixed synthetic time",
                ),
                budget: Budget::default(),
                criteria: vec![
                    Criterion::ToolOutput {
                        name: "read_file".into(),
                        pointer: "/lines/0/text".into(),
                        expected: json!("UNO"),
                    },
                    Criterion::ToolOutput {
                        name: "read_file".into(),
                        pointer: "/lines/1/text".into(),
                        expected: json!("DOS"),
                    },
                ],
            },
        )
        .await
        .unwrap();
    let state = engine.run(&run).await.unwrap();
    assert_eq!(
        std::fs::read(workspace.join("f.txt")).unwrap(),
        b"UNO\nDOS\n",
        "a later same-resource edit must use the preceding edit's postimage"
    );
    assert_eq!(state.status, Status::Completed, "{:?}", state.reason);
    let events = journal.replay(&run, 0).await.unwrap();
    let intents = events
        .iter()
        .filter(|event| {
            event.kind == "tool_intent" && event.payload["prepared"]["call"]["name"] == "edit_file"
        })
        .collect::<Vec<_>>();
    assert_eq!(intents.len(), 2);
    assert_eq!(intents[0].payload["prepared"]["call"]["id"], "edit_first");
    assert_eq!(intents[1].payload["prepared"]["call"]["id"], "edit_second");
    assert_eq!(
        intents[0].payload["prepared"]["data"]["edits"][0]["path"],
        "f.txt"
    );
    assert_eq!(
        intents[1].payload["prepared"]["data"]["edits"][0]["path"],
        "f.txt"
    );
    let first_post = &intents[0].payload["prepared"]["data"]["edits"][0]["post"];
    let second_pre = &intents[1].payload["prepared"]["data"]["edits"][0]["pre"];
    assert!(!first_post.is_null());
    assert_eq!(
        first_post, second_pre,
        "the second durable intention must describe the first confirmed postimage as its preimage"
    );
    let first_result = events
        .iter()
        .find(|event| {
            event.kind == "tool_result"
                && event.payload["name"] == "edit_file"
                && event.payload["index"] == 0
        })
        .unwrap();
    assert!(
        first_result.sequence < intents[1].sequence,
        "second preparation/intention follows the first confirmed result"
    );
    let edit_results = state
        .history
        .iter()
        .filter(|message| {
            matches!(
                message.tool_call_id.as_deref(),
                Some("edit_first" | "edit_second")
            )
        })
        .map(|message| serde_json::from_str::<ToolResult>(&message.content).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(edit_results.len(), 2);
    assert!(
        edit_results.iter().all(|result| result.success),
        "{edit_results:?}"
    );
    let versions=db.read(ReadLane::Interactive,|c|{
        let mut q=c.prepare("SELECT tool_call_id,preimage_blob,postimage_blob FROM file_versions ORDER BY tool_call_id")?;
        Ok(q.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?.collect::<Result<Vec<_>,_>>()?)
    }).await.unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].0, "edit_first");
    assert_eq!(versions[1].0, "edit_second");
    assert_eq!(versions[0].2, versions[1].1);
    tools.terminals().shutdown().await;
    db.shutdown().await.unwrap();
}

#![allow(dead_code)]
use excalibur_context::ContextInput;
use excalibur_engine::{Budget, Criterion, RunSpec};
use excalibur_journal::Journal;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_types::runtime::*;
use serde_json::json;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
pub fn increment(path: &Path) {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(b"1\n").unwrap();
    f.sync_all().unwrap();
}
pub fn count(path: &Path) -> usize {
    fs::read_to_string(path).unwrap_or_default().lines().count()
}
pub struct Model {
    pub root: PathBuf,
    pub file: bool,
    pub steps: usize,
}
impl ModelGateway for Model {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            increment(&self.root.join("models"));
            let completed = request.messages.iter().filter(|m| m.role == "tool").count();
            let mut message = Message::text("assistant", "Synthetic task complete");
            if completed < self.steps {
                message.content = String::new();
                message.tool_calls = vec![ToolCall {
                    id: format!("call{completed}"),
                    name: if self.file { "write_file" } else { "effect" }.into(),
                    arguments: if self.file {
                        json!({"path":"answer.txt","content":"42\n"}).to_string()
                    } else {
                        "{}".into()
                    },
                }];
            }
            Ok(ModelResponse {
                message,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cost_microusd: 1,
                    estimated: false,
                },
            })
        })
    }
}
pub struct Effects {
    pub root: PathBuf,
    pub effect: Effect,
    pub journal: Journal,
}
impl ToolRuntime for Effects {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "effect".into(),
            description: "Synthetic durable effect fixture".into(),
            input_schema: json!({"type":"object","additionalProperties":false}),
        }]
    }
    fn prepare<'a>(
        &'a self,
        _run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            Ok(PreparedTool {
                call,
                effect: self.effect,
                resources: vec![Resource {
                    key: self.root.join("effect").display().to_string(),
                    write: self.effect != Effect::Pure,
                }],
                exclusive: false,
                data: json!({"post":"42"}),
            })
        })
    }
    fn execute<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        _tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            assert!(
                self.journal
                    .replay(run, 0)
                    .await
                    .unwrap()
                    .iter()
                    .any(|e| e.kind == "tool_intent" && e.key == key),
                "no effect before confirmed intention"
            );
            increment(&self.root.join("executions"));
            match self.effect {
                Effect::Pure => {}
                Effect::Idempotent => {
                    if !self.root.join("effect").exists() {
                        fs::write(self.root.join("effect"), b"42").unwrap();
                        increment(&self.root.join("effects"));
                    }
                }
                Effect::Keyed => {
                    use sha2::{Digest, Sha256};
                    let path = self
                        .root
                        .join(format!("key-{:x}", Sha256::digest(key.as_bytes())));
                    if let Ok(f) = fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(path)
                    {
                        f.sync_all().unwrap();
                        increment(&self.root.join("effects"));
                    }
                }
                Effect::NonIdempotent => increment(&self.root.join("effects")),
            }
            ToolResult::ok(json!({"done":true}))
        })
    }
    fn reconcile<'a>(
        &'a self,
        _run: &'a str,
        _key: &'a str,
        _tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async move {
            match self.effect {
                Effect::Pure | Effect::Keyed => Reconcile::Retry,
                Effect::NonIdempotent => Reconcile::Uncertain("no reliable effect probe".into()),
                Effect::Idempotent => match fs::read(self.root.join("effect")) {
                    Ok(bytes) if bytes == b"42" => {
                        Reconcile::Done(ToolResult::ok(json!({"done":true})))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Reconcile::Retry,
                    _ => Reconcile::Uncertain("external state changed".into()),
                },
            }
        })
    }
}
pub fn spec(file: bool, max_steps: u64) -> RunSpec {
    RunSpec {
        context: ContextInput::new(
            "Solve only the authorized task.",
            "Create the requested synthetic result and verify it.",
            "fixed-host-time",
        ),
        budget: Budget {
            max_steps,
            ..Default::default()
        },
        criteria: vec![Criterion::ToolSuccess {
            name: if file { "write_file" } else { "effect" }.into(),
        }],
    }
}
pub fn model(root: &Path, file: bool, steps: usize) -> Arc<dyn ModelGateway> {
    Arc::new(Model {
        root: root.into(),
        file,
        steps,
    })
}

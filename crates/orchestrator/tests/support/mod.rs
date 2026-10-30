#![allow(dead_code)]
use excalibur_context::ContextInput;
use excalibur_engine::*;
use excalibur_journal::Journal;
use excalibur_models::{ModelError, ModelGateway};
use excalibur_orchestrator::*;
use excalibur_security::CapabilityToken;
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_types::runtime::*;
use std::{collections::BTreeSet, sync::Arc};
struct Model;
impl ModelGateway for Model {
    fn complete<'a>(
        &'a self,
        _: ModelRequest,
    ) -> BoxFuture<'a, std::result::Result<ModelResponse, ModelError>> {
        Box::pin(async {
            Ok(ModelResponse {
                message: Message::text("assistant", "Synthetic done"),
                usage: Usage::default(),
            })
        })
    }
}
struct NoTools;
impl ToolRuntime for NoTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        _: ToolCall,
    ) -> BoxFuture<'a, std::result::Result<PreparedTool, String>> {
        Box::pin(async { Err("no tools".into()) })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { ToolResult::error("no tool") })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { Reconcile::Retry })
    }
}
struct Authority;
impl RunAuthority for Authority {
    fn bind<'a>(&'a self, _: &'a Agent) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}
pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub db: Database,
    pub host: Orchestrator,
    pub master: Agent,
}
pub async fn fixture() -> Fixture {
    open_fixture(tempfile::tempdir().unwrap()).await
}
pub async fn open_fixture(dir: tempfile::TempDir) -> Fixture {
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let engine = Arc::new(Engine::new(journal, Arc::new(Model), Arc::new(NoTools)));
    let host = Orchestrator::open(
        db.clone(),
        engine,
        Limits {
            running: 2,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .with_authority(Arc::new(Authority));
    let master = host
        .rebind_master_at_startup("Excalibur", token())
        .await
        .unwrap();
    Fixture {
        dir,
        db,
        host,
        master,
    }
}
pub fn token() -> CapabilityToken {
    CapabilityToken {
        data_scopes: BTreeSet::from(["general".into()]),
        budget: 1000,
        ..Default::default()
    }
}
pub fn delegation(operation: &str, scope: &str, seconds: u64) -> Delegation {
    Delegation {
        operation: operation.into(),
        name: operation.into(),
        profile: if operation == "root" || operation == "p" || operation == "q" {
            "coordinator"
        } else {
            "implementer"
        }
        .into(),
        scope: scope.into(),
        task: format!("task:{operation}"),
        origin_conversation: "conversation:test".into(),
        goal: "Synthetic independent work".into(),
        deliverable: "Synthetic result".into(),
        inputs: vec![],
        pinned_instructions: vec![],
        suggested_skills: vec![],
        fork_context: false,
        token: token(),
        spec: RunSpec {
            context: ContextInput::new("Synthetic rules", "Synthetic task", "fixed"),
            budget: Budget {
                max_steps: if seconds > 100 { 150 } else { 30 },
                max_tokens: if seconds > 100 { 100000 } else { 1000 },
                max_wall_seconds: seconds,
                max_tool_calls: if seconds > 100 { 500 } else { 10 },
                ..Default::default()
            },
            criteria: vec![Criterion::FinalResponse { schema: None }],
        },
        priority: 1,
    }
}

pub fn no_tools() -> Arc<dyn ToolRuntime> {
    Arc::new(NoTools)
}
pub fn model() -> Arc<dyn ModelGateway> {
    Arc::new(Model)
}

pub fn authority() -> Arc<dyn RunAuthority> {
    Arc::new(Authority)
}

//! Scoped skill indexes and durable worker sessions. All filesystem scans and
//! tokenizer work run off the interactive executor.
use excalibur_context::{SessionPolicy, TokenCounter};
use excalibur_models::ModelGateway;
use excalibur_orchestrator::{Agent, Project};
use excalibur_skills::{
    groups::{CatalogScope, TaskKind},
    index::{SkillIndex, SkillRoot},
    runtime::SkillRuntime,
    watcher::SkillWatcher,
};
use excalibur_store::Database;
use excalibur_types::runtime::ToolRuntime;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
#[derive(Clone, Default)]
pub struct SkillsOptions {
    pub user_roots: Vec<PathBuf>,
    pub installed_roots: Vec<PathBuf>,
    /// Override only when the host knows the selected model's context window.
    pub model_window: Option<usize>,
    pub counter: Option<TokenCounter>,
}
pub struct SkillsHost {
    pub runtime: Arc<SkillRuntime>,
    pub(crate) indexes: BTreeMap<String, Arc<SkillIndex>>,
    pub(crate) window: usize,
    pub(crate) counter: TokenCounter,
    watchers: Mutex<Vec<SkillWatcher>>,
}
impl SkillsHost {
    pub async fn open(
        db: Database,
        delegate: Arc<dyn ToolRuntime>,
        projects: &[Project],
        options: SkillsOptions,
        model: &dyn ModelGateway,
    ) -> Result<Arc<Self>, String> {
        let mut indexes = BTreeMap::new();
        let mut scopes = vec![("general".to_string(), Vec::<PathBuf>::new())];
        scopes.extend(
            projects
                .iter()
                .map(|p| (format!("project:{}", p.id), p.roots.clone())),
        );
        for (scope, projects) in scopes {
            let mut roots = projects.iter().map(SkillRoot::project).collect::<Vec<_>>();
            roots.extend(options.user_roots.iter().cloned().map(SkillRoot::user));
            roots.extend(
                options
                    .installed_roots
                    .iter()
                    .cloned()
                    .map(SkillRoot::installed),
            );
            roots.push(SkillRoot::builtin());
            let index = Arc::new(SkillIndex::new(roots));
            index.refresh_background().await?;
            indexes.insert(scope, index);
        }
        let policy = model
            .context_policy_override()
            .map(serde_json::from_value::<SessionPolicy>)
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let counter = options.counter.unwrap_or_else(|| {
            TokenCounter::for_model(model.metadata().model.unwrap_or_else(|| "unknown".into()))
        });
        let runtime = Arc::new(
            SkillRuntime::new(
                db,
                delegate,
                indexes.clone(),
                options.model_window.unwrap_or(policy.model_window),
                counter.clone(),
            )
            .await?
            .with_core_tools(excalibur_engine::builtin_tool_definitions()),
        );
        let mut watchers = Vec::new();
        for index in indexes.values() {
            let index = index.clone();
            let handle = tokio::runtime::Handle::current();
            watchers.push(
                tokio::task::spawn_blocking(move || {
                    let _entered = handle.enter();
                    SkillWatcher::start(index)
                })
                .await
                .map_err(|e| e.to_string())??,
            );
        }
        Ok(Arc::new(Self {
            runtime,
            indexes,
            window: options.model_window.unwrap_or(policy.model_window),
            counter,
            watchers: Mutex::new(watchers),
        }))
    }
    pub async fn bind_agent(&self, agent: &Agent) -> Result<(), String> {
        let Some(run) = agent.run_id.as_ref() else {
            return Ok(());
        };
        if !agent.token.data_scopes.contains(&agent.scope) {
            return Err("skill scope exceeds run grant".into());
        }
        let kind = if agent.scope.starts_with("project:") {
            TaskKind::Programming
        } else {
            TaskKind::Conversation
        };
        let profile = CatalogScope::new(kind, &agent.profile);
        let query = agent
            .delegation
            .as_ref()
            .map(|d| format!("{} {}", d.goal, d.deliverable))
            .unwrap_or_default();
        self.runtime.bind(run, &agent.scope, profile, &query).await
    }
    pub async fn refresh(&self, scope: &str) -> Result<(), String> {
        self.indexes
            .get(scope)
            .ok_or("unknown skill scope")?
            .refresh_background()
            .await?;
        Ok(())
    }
    pub fn stop(&self) {
        self.watchers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

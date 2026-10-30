use crate::FastActions;
use excalibur_tools::deferred::{CallerRole, DeferredTools, ToolCatalog};
use excalibur_types::runtime::*;
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc};

/// Opt-in host composition: the delegate must already enforce permissions for
/// the supplied run. The master receives only search/call wrappers, never the
/// delegate's eager file or terminal core. CallerRole::Master rejects every
/// non-fast catalog entry at both discovery and actual execution.
pub struct MasterFastTools {
    core: Arc<dyn FastActions>,
    deferred: DeferredTools,
    run: String,
    scopes: BTreeSet<String>,
}
impl MasterFastTools {
    pub fn new(
        core: Arc<dyn FastActions>,
        secured_delegate: Arc<dyn ToolRuntime>,
        catalog: Arc<ToolCatalog>,
        run: String,
        scopes: BTreeSet<String>,
    ) -> std::result::Result<Self, String> {
        if run.is_empty() || scopes.is_empty() {
            return Err("master deferred adapter needs an authorized run and scopes".into());
        }
        if core
            .definitions()
            .iter()
            .any(|d| matches!(d.name.as_str(), "search_tools" | "call_tool"))
        {
            return Err("master deferred wrapper name collision".into());
        }
        for name in catalog.names() {
            if let Some(tool) = catalog.get(&name)
                && tool.fast
                && (tool.timeout_ms > 10_000 || forbidden(&tool.name))
            {
                return Err("a fast master tool must finish within ten seconds".into());
            }
        }
        Ok(Self {
            core,
            deferred: DeferredTools::new(secured_delegate, catalog, CallerRole::Master)?,
            run,
            scopes,
        })
    }
    pub fn deferred(&self) -> &DeferredTools {
        &self.deferred
    }
}
impl FastActions for MasterFastTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self.core.definitions();
        definitions.extend(
            self.deferred
                .definitions()
                .into_iter()
                .filter(|d| matches!(d.name.as_str(), "search_tools" | "call_tool")),
        );
        definitions
    }
    fn execute<'a>(
        &'a self,
        conversation: &'a str,
        command: &'a str,
        target: Option<&'a str>,
        key: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Value, String>> {
        self.core.execute(conversation, command, target, key)
    }
    fn context<'a>(
        &'a self,
        conversation: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Value, String>> {
        self.core.context(conversation)
    }
    fn invoke<'a>(
        &'a self,
        conversation: &'a str,
        name: &'a str,
        args: Value,
        key: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Value, String>> {
        if matches!(name, "search_tools" | "call_tool") {
            return Box::pin(async {
                Err("deferred master tools require a frozen authorized scope".into())
            });
        }
        self.core.invoke(conversation, name, args, key)
    }
    fn invoke_scoped<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
        name: &'a str,
        args: Value,
        key: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Value, String>> {
        if !matches!(name, "search_tools" | "call_tool") {
            return self
                .core
                .invoke_scoped(conversation, scope, name, args, key);
        }
        Box::pin(async move {
            if !self.scopes.contains(scope) {
                return Err("master deferred tool scope is not authorized".into());
            }
            if name == "call_tool" {
                let tool = args
                    .get("name")
                    .and_then(Value::as_str)
                    .or_else(|| args.get("tool").and_then(Value::as_str))
                    .ok_or("missing deferred tool name")?;
                let metadata = self
                    .deferred
                    .catalog()
                    .get(tool)
                    .ok_or("unknown deferred tool")?;
                if !metadata.fast || metadata.timeout_ms > 10_000 || forbidden(&metadata.name) {
                    return Err("master cannot execute slow domain work".into());
                }
            }
            let call = ToolCall {
                id: key.into(),
                name: name.into(),
                arguments: args.to_string(),
            };
            let prepared = self.deferred.prepare(&self.run, call).await?;
            let result = self.deferred.execute(&self.run, key, &prepared).await;
            Ok(serde_json::json!({"success":result.success,"output":result.output}))
        })
    }
}

fn forbidden(name: &str) -> bool {
    matches!(
        name,
        "exec"
            | "exec_io"
            | "read_file"
            | "file_edit"
            | "file_write"
            | "file_list"
            | "file_search"
            | "lsp"
            | "code_graph"
            | "web_read"
            | "browser"
    )
}

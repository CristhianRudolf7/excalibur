use crate::{
    profile::FrozenProfile,
    skills::{Skill, builtin_skills},
};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::sync::Arc;
pub struct ExecutionTools {
    delegate: Arc<dyn ToolRuntime>,
    profile: Option<FrozenProfile>,
    skills: Vec<Skill>,
    web: bool,
}
impl ExecutionTools {
    pub fn new(delegate: Arc<dyn ToolRuntime>, profile: Option<FrozenProfile>, web: bool) -> Self {
        Self {
            delegate,
            profile,
            skills: builtin_skills(),
            web,
        }
    }
    pub fn with_skills(mut self, skills: Vec<Skill>) -> Self {
        self.skills = skills;
        self
    }
    fn available(&self, name: &str) -> bool {
        self.profile.as_ref().is_none_or(|p| p.allows_tool(name))
            && (self.web || !matches!(name, "web_search" | "web_read" | "source_search"))
    }
}
impl ToolRuntime for ExecutionTools {
    fn interrupt_execution<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
        tool: Option<&'a excalibur_types::runtime::PreparedTool>,
    ) -> excalibur_types::runtime::BoxFuture<'a, std::result::Result<bool, String>> {
        self.delegate.interrupt_execution(run, cancelled, tool)
    }
    fn configure_execution_limits<'a>(
        &'a self,
        run: &'a str,
        limits: excalibur_types::runtime::ExecutionLimits,
    ) -> excalibur_types::runtime::BoxFuture<'a, std::result::Result<(), String>> {
        self.delegate.configure_execution_limits(run, limits)
    }
    fn execution_resources<'a>(
        &'a self,
        run: &'a str,
    ) -> excalibur_types::runtime::BoxFuture<
        'a,
        std::result::Result<Option<excalibur_types::runtime::ExecutionResources>, String>,
    > {
        self.delegate.execution_resources(run)
    }

    fn sanitize_durable(&self, v: &Value) -> Result<Value, String> {
        self.delegate.sanitize_durable(v)
    }
    fn pending_approval<'a>(&'a self, r: &'a str) -> BoxFuture<'a, bool> {
        self.delegate.pending_approval(r)
    }
    fn approval_blocked<'a>(&'a self, r: &'a str) -> BoxFuture<'a, Option<String>> {
        self.delegate.approval_blocked(r)
    }
    fn finalization_guard<'a>(
        &'a self,
        r: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn FinalizationGuard>, String>> {
        self.delegate.finalization_guard(r)
    }
    fn observe_untrusted<'a>(
        &'a self,
        r: &'a str,
        s: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.observe_untrusted(r, s)
    }
    fn execution_tool_budget<'a>(
        &'a self,
        r: &'a str,
    ) -> BoxFuture<'a, Result<Option<u64>, String>> {
        self.delegate.execution_tool_budget(r)
    }
    fn configure_process_boundary<'a>(
        &'a self,
        r: &'a str,
        b: ProcessBoundary,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_process_boundary(r, b)
    }
    fn configure_network_authorizer<'a>(
        &'a self,
        r: &'a str,
        a: Arc<dyn NetworkAuthorizer>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_network_authorizer(r, a)
    }
    fn audit_builtin<'a>(
        &'a self,
        r: &'a str,
        c: &'a ToolCall,
        o: Option<&'a ToolResult>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.audit_builtin(r, c, o)
    }
    fn durable_blob_inspector(&self) -> Option<Box<dyn DurableByteInspector>> {
        self.delegate.durable_blob_inspector()
    }
    fn result_requires_approval(&self, r: &ToolResult) -> bool {
        self.delegate.result_requires_approval(r)
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = self
            .delegate
            .definitions()
            .into_iter()
            .filter(|d| self.available(&d.name))
            .collect::<Vec<_>>();
        for name in ["search_skills", "load_skill"] {
            defs.push(ToolDefinition{name:name.into(),description:if name=="load_skill"{"Load one immutable programming skill."}else{"Search the frozen programming skill catalog."}.into(),input_schema:json!({"type":"object","properties":{"query":{"type":"string"},"name":{"type":"string"}},"additionalProperties":false})});
        }
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }
    fn scheduling<'a>(
        &'a self,
        r: &'a str,
        c: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        if matches!(c.name.as_str(), "search_skills" | "load_skill") {
            Box::pin(async {
                Some(ToolScheduling {
                    resources: vec![],
                    exclusive: false,
                })
            })
        } else {
            self.delegate.scheduling(r, c)
        }
    }
    fn prepare<'a>(
        &'a self,
        r: &'a str,
        c: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            if !self.available(&c.name) {
                return Err("operation is disabled by the frozen execution profile".into());
            }
            if matches!(c.name.as_str(), "web_search" | "web_read" | "source_search")
                && crate::profile::benchmark_source_blocked(&c.arguments)
            {
                return Err("benchmark sources are prohibited".into());
            }
            if matches!(c.name.as_str(), "search_skills" | "load_skill") {
                let args: Value =
                    serde_json::from_str(&c.arguments).map_err(|_| "invalid skill arguments")?;
                if !args.is_object()
                    || args
                        .as_object()
                        .unwrap()
                        .keys()
                        .any(|k| k != "name" && k != "query")
                {
                    return Err("invalid skill arguments".into());
                }
                if c.name == "load_skill"
                    && !self
                        .skills
                        .iter()
                        .any(|s| Some(s.name.as_str()) == args["name"].as_str())
                {
                    return Err("unknown frozen skill".into());
                }
                Ok(PreparedTool {
                    call: c,
                    effect: Effect::Pure,
                    resources: vec![],
                    exclusive: false,
                    data: args,
                })
            } else {
                self.delegate.prepare(r, c).await
            }
        })
    }
    fn execute<'a>(
        &'a self,
        r: &'a str,
        k: &'a str,
        t: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            if matches!(t.call.name.as_str(), "search_skills" | "load_skill") {
                if self.delegate.audit_builtin(r, &t.call, None).await.is_err() {
                    return ToolResult::error("skill audit admission failed");
                }
                let result = if t.call.name == "load_skill" {
                    self.skills
                        .iter()
                        .find(|s| Some(s.name.as_str()) == t.data["name"].as_str())
                        .map_or_else(
                            || ToolResult::error("unknown frozen skill"),
                            |s| ToolResult::ok(json!(s)),
                        )
                } else {
                    let q = t.data["query"].as_str().unwrap_or("").to_lowercase();
                    ToolResult::ok(
                        json!({"skills":self.skills.iter().filter(|s|s.name.contains(&q)||s.description.to_lowercase().contains(&q)).map(|s|json!({"name":s.name,"group":s.group,"description":s.description})).collect::<Vec<_>>()}),
                    )
                };
                let result = match serde_json::to_value(&result)
                    .ok()
                    .and_then(|v| self.delegate.sanitize_durable(&v).ok())
                    .and_then(|v| serde_json::from_value(v).ok())
                {
                    Some(result) => result,
                    None => return ToolResult::error("skill output sanitization failed"),
                };
                if self
                    .delegate
                    .audit_builtin(r, &t.call, Some(&result))
                    .await
                    .is_err()
                {
                    return ToolResult::error("skill result audit failed");
                }
                result
            } else {
                self.delegate.execute(r, k, t).await
            }
        })
    }
    fn reconcile<'a>(
        &'a self,
        r: &'a str,
        k: &'a str,
        t: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        if matches!(t.call.name.as_str(), "search_skills" | "load_skill") {
            Box::pin(async { Reconcile::Retry })
        } else {
            self.delegate.reconcile(r, k, t)
        }
    }
}

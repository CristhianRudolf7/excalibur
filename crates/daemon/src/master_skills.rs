//! Master-only skill adapter. Its schemas come exclusively from FastActions.
use excalibur_master::{FastActions, MasterContextRequest};
use excalibur_skills::{
    conversation::ConversationBinding,
    groups::{CatalogScope, TaskKind},
    runtime::SkillRuntime,
};
use excalibur_store::{Database, ReadLane, rusqlite::OptionalExtension};
use excalibur_types::runtime::*;
use excalibur_vault::Redactor;
use serde_json::Value;
use std::sync::Arc;

struct AdapterSchemas {
    inner: Arc<dyn FastActions>,
    redactor: Redactor,
}
impl ToolRuntime for AdapterSchemas {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }
    fn sanitize_durable(&self, value: &Value) -> Result<Value, String> {
        Ok(self.redactor.redact_json(value))
    }
    fn prepare<'a>(
        &'a self,
        _: &'a str,
        _: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async { Err("master effects use the source-scoped fast action port".into()) })
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async { ToolResult::error("master adapter has no worker execution port") })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async { Reconcile::Uncertain("master effects use master action receipts".into()) })
    }
}

pub struct MasterSkillFastActions {
    inner: Arc<dyn FastActions>,
    runtime: Arc<SkillRuntime>,
    db: Database,
    principal: String,
}
impl MasterSkillFastActions {
    pub async fn new(
        db: Database,
        inner: Arc<dyn FastActions>,
        host: &crate::skills::SkillsHost,
        principal: String,
        redactor: Redactor,
    ) -> Result<Self, String> {
        if inner
            .definitions()
            .iter()
            .any(|d| !excalibur_master::fast_tool_name(&d.name))
        {
            return Err("master skill adapter received a non-fast tool".into());
        }
        let runtime = Arc::new(
            SkillRuntime::new(
                db.clone(),
                Arc::new(AdapterSchemas {
                    inner: inner.clone(),
                    redactor,
                }),
                host.indexes.clone(),
                host.window,
                host.counter.clone(),
            )
            .await?,
        );
        Ok(Self {
            inner,
            runtime,
            db,
            principal,
        })
    }
    async fn bind(&self, request: MasterContextRequest<'_>) -> Result<String, String> {
        if request.principal != self.principal {
            return Err("master skill principal mismatch".into());
        }
        let binding = ConversationBinding {
            principal: self.principal.clone(),
            conversation: request.conversation.into(),
            scope: request.original_scope.into(),
            source: request.source_message_id.into(),
        };
        let expected = binding.clone();
        let literal=self.db.read(ReadLane::Interactive,move|c| {
            let source:Option<String>=c.query_row("SELECT COALESCE(s.text,m.text) FROM messages m JOIN master_inbox i ON i.message_id=m.id LEFT JOIN master_user_sources s ON s.id=m.id WHERE m.id=?1 AND m.conversation_id=?2 AND m.principal_id=?3 AND COALESCE(m.scope_id,'general')=?4 AND i.conversation_id=m.conversation_id",
                (&expected.source,&expected.conversation,&expected.principal,&expected.scope),|r|r.get(0)).optional()?;
            Ok(source)
        }).await.map_err(|e|e.to_string())?.ok_or("master skill source binding is not accepted")?;
        self.runtime
            .bind_conversation_turn(
                &binding,
                CatalogScope::new(TaskKind::Conversation, "master"),
                &literal,
            )
            .await
    }
}
impl FastActions for MasterSkillFastActions {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut all = self.inner.definitions();
        all.retain(|d| !matches!(d.name.as_str(), "search_skills" | "load_skill"));
        all.extend(excalibur_skills::runtime::definitions());
        all
    }
    fn session_context<'a>(
        &'a self,
        request: MasterContextRequest<'a>,
    ) -> BoxFuture<'a, Result<RunContext, String>> {
        Box::pin(async move {
            let run = self.bind(request.clone()).await?;
            let mut context = self.inner.session_context(request).await?;
            let skills = self.runtime.session_context(&run).await?;
            context.identity.push_str(&skills.identity);
            context.suggested_skills.extend(skills.suggested_skills);
            context.suggested_skills.truncate(3);
            context.updates.extend(skills.updates);
            Ok(context)
        })
    }
    fn invoke_source_scoped<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
        source: &'a str,
        name: &'a str,
        args: Value,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            if !self.definitions().iter().any(|d| d.name == name) {
                return Err("tool is not an authorized master fast action".into());
            }
            let run = self
                .bind(MasterContextRequest {
                    principal: &self.principal,
                    conversation,
                    original_scope: scope,
                    source_message_id: source,
                })
                .await?;
            let call = ToolCall {
                id: key.into(),
                name: name.into(),
                arguments: args.to_string(),
            };
            if matches!(name, "search_skills" | "load_skill") {
                let result = self.runtime.invoke_skill(&run, key, call).await?;
                if result.success {
                    Ok(result.output)
                } else {
                    Err(result.output.to_string())
                }
            } else {
                let _guard = self.runtime.admit_call(&run, &call).await?;
                self.inner
                    .invoke_source_scoped(conversation, scope, source, name, args, key)
                    .await
            }
        })
    }
    fn invoke<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Value,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async {
            Err("master skill tools require an accepted source and original scope".into())
        })
    }
    fn invoke_scoped<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a str,
        _: Value,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async { Err("master skill tools require an accepted source".into()) })
    }
    fn execute<'a>(
        &'a self,
        c: &'a str,
        command: &'a str,
        target: Option<&'a str>,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        self.inner.execute(c, command, target, key)
    }
    // Human confirmation deliberately bypasses model skill policy. It remains
    // protected by the inner durable human-decision guard and has no schema.
    fn confirm_cancel<'a>(
        &'a self,
        c: &'a str,
        s: &'a str,
        t: &'a str,
        k: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        self.inner.confirm_cancel(c, s, t, k)
    }
    fn context<'a>(&'a self, c: &'a str) -> BoxFuture<'a, Result<Value, String>> {
        self.inner.context(c)
    }
    fn stable_context<'a>(&'a self, c: &'a str, s: &'a str) -> BoxFuture<'a, (String, String)> {
        self.inner.stable_context(c, s)
    }
    fn finalize_response_scoped<'a>(
        &'a self,
        c: &'a str,
        s: &'a str,
        t: &'a str,
    ) -> BoxFuture<'a, String> {
        self.inner.finalize_response_scoped(c, s, t)
    }
    fn finalize_response<'a>(&'a self, c: &'a str, t: &'a str) -> BoxFuture<'a, String> {
        self.inner.finalize_response(c, t)
    }
}

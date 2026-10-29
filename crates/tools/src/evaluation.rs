//! Episode-scoped tools backed by the shared writer. This is a small durable
//! task scratchpad, not the full project/planning subsystem.
use excalibur_store::{
    Database, Error, Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::sync::Arc;

const LOCAL: &[&str] = &[
    "apply_patch",
    "edit_file",
    "exec",
    "exec_io",
    "glob",
    "grep",
    "read_file",
    "write_file",
];
const ENTITIES: &[&str] = &[
    "task",
    "plan",
    "requirement",
    "criterion",
    "check",
    "evidence",
    "assumption",
    "decision",
    "note",
];
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS eval_tools_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL);CREATE TABLE IF NOT EXISTS eval_episode_records(scope TEXT NOT NULL,id TEXT NOT NULL,entity TEXT NOT NULL,version INTEGER NOT NULL CHECK(version>0),data TEXT NOT NULL CHECK(json_valid(data)),PRIMARY KEY(scope,id));CREATE TABLE IF NOT EXISTS eval_task_receipts(scope TEXT NOT NULL,operation_key TEXT NOT NULL,request_hash TEXT NOT NULL,result TEXT NOT NULL CHECK(json_valid(result)),PRIMARY KEY(scope,operation_key));";

#[derive(Clone)]
pub struct EpisodeTools {
    delegate: Arc<dyn ToolRuntime>,
    db: Database,
    scope: String,
    catalog: Vec<ToolDefinition>,
}
impl EpisodeTools {
    pub async fn new(
        delegate: Arc<dyn ToolRuntime>,
        db: Database,
        scope: impl Into<String>,
    ) -> Result<Self, String> {
        let scope = scope.into();
        if scope.is_empty() || scope.len() > 128 || scope.chars().any(char::is_control) {
            return Err("episode scope must contain 1..128 printable bytes".into());
        }
        let checksum = crate::files::hash(SCHEMA.as_bytes());
        db.write(Priority::Normal, move |connection| {
            connection.execute_batch(SCHEMA)?;
            let old = connection
                .query_row(
                    "SELECT checksum FROM eval_tools_schema WHERE version=1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            match old {
                Some(old) if old != checksum => {
                    return Err(Error::Configuration(
                        "episode tools schema checksum mismatch".into(),
                    ));
                }
                None => {
                    connection.execute("INSERT INTO eval_tools_schema VALUES(1,?1)", [checksum])?;
                }
                _ => {}
            }
            Ok(())
        })
        .await
        .map_err(|error| error.to_string())?;
        let mut catalog = delegate
            .definitions()
            .into_iter()
            .filter(|definition| LOCAL.contains(&definition.name.as_str()))
            .collect::<Vec<_>>();
        catalog.sort_by(|a, b| a.name.cmp(&b.name));
        catalog.dedup_by(|a, b| a.name == b.name);
        Ok(Self {
            delegate,
            db,
            scope,
            catalog,
        })
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
    fn args(&self, call: &ToolCall) -> Result<Value, String> {
        let args: Value = serde_json::from_str(&call.arguments)
            .map_err(|_| "episode tools require a JSON object")?;
        if !args.is_object() {
            return Err("episode tools require a JSON object".into());
        }
        if self.delegate.sanitize_durable(&args)? != args {
            return Err("sensitive content cannot enter episode tool arguments".into());
        }
        Ok(args)
    }
    fn dynamic(&self, call: &ToolCall) -> Result<ToolCall, String> {
        let args = self.args(call)?;
        keys(&args, &["name", "arguments"])?;
        let name = text(&args, "name")?;
        if !self
            .catalog
            .iter()
            .any(|definition| definition.name == name)
        {
            return Err("requested tool is unavailable in this frozen episode catalog".into());
        }
        let arguments = args
            .get("arguments")
            .filter(|v| v.is_object())
            .ok_or("call_tool.arguments must be an object")?;
        Ok(ToolCall {
            id: call.id.clone(),
            name: name.into(),
            arguments: arguments.to_string(),
        })
    }
    fn wrap(&self, call: ToolCall, inner: PreparedTool) -> PreparedTool {
        PreparedTool {
            call,
            effect: inner.effect,
            resources: inner.resources.clone(),
            exclusive: inner.exclusive,
            data: json!({"scope":self.scope,"inner":inner}),
        }
    }
    fn inner(&self, tool: &PreparedTool) -> Result<PreparedTool, String> {
        if tool.data["scope"] != self.scope {
            return Err("prepared tool belongs to another episode scope".into());
        }
        let inner: PreparedTool = serde_json::from_value(tool.data["inner"].clone())
            .map_err(|_| "invalid delegated episode preparation")?;
        let expected = if tool.call.name == "call_tool" {
            self.dynamic(&tool.call)?.name
        } else {
            tool.call.name.clone()
        };
        if !self.catalog.iter().any(|d| d.name == expected)
            || inner.call.name != expected
            || inner.call.id != tool.call.id
        {
            return Err("delegated tool differs from the frozen episode catalog".into());
        }
        Ok(inner)
    }
    fn own_args(&self, tool: &PreparedTool) -> Result<Value, String> {
        let args = self.args(&tool.call)?;
        if tool.data["scope"] != self.scope || tool.data["args"] != args {
            return Err("episode preparation changed after admission".into());
        }
        Ok(args)
    }
    async fn receipt(&self, key: &str, request_hash: &str) -> Result<Option<Value>, String> {
        let (scope, key, hash) = (self.scope.clone(), key.to_owned(), request_hash.to_owned());
        self.db.read(ReadLane::Interactive,move|c| {
            let found=c.query_row("SELECT request_hash,result FROM eval_task_receipts WHERE scope=?1 AND operation_key=?2",[scope,key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?;
            match found {Some((stored,_))if stored!=hash=>Err(Error::Configuration("episode operation key reused with different arguments".into())),Some((_,result))=>serde_json::from_str(&result).map(Some).map_err(|e|Error::Configuration(e.to_string())),None=>Ok(None)}
        }).await.map_err(|e|e.to_string())
    }
    async fn task_db(&self, key: &str, args: Value) -> Result<Value, String> {
        let scope = self.scope.clone();
        let key = key.to_owned();
        let request_hash = crate::files::hash(args.to_string().as_bytes());
        let sanitizer = self.delegate.clone();
        self.db.write(Priority::Normal,move|connection| {
            let old=connection.query_row("SELECT request_hash,result FROM eval_task_receipts WHERE scope=?1 AND operation_key=?2",[&scope,&key],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).optional()?;
            if let Some((hash,result))=old {if hash!=request_hash{return Err(Error::Configuration("episode operation key reused with different arguments".into()));}return serde_json::from_str(&result).map_err(|e|Error::Configuration(e.to_string()));}
            let value=apply_batch(connection,&scope,&args).map_err(Error::Configuration)?;
            let value=sanitizer.sanitize_durable(&value).map_err(Error::Configuration)?;
            connection.execute("INSERT INTO eval_task_receipts VALUES(?1,?2,?3,?4)",rusqlite::params![scope,key,request_hash,value.to_string()])?;
            Ok(value)
        }).await.map_err(|e|e.to_string())
    }
    async fn own_execute(
        &self,
        run: &str,
        key: &str,
        tool: &PreparedTool,
    ) -> Result<Value, String> {
        let args = self.own_args(tool)?;
        match tool.call.name.as_str() {
            "task_db" => self.task_db(key, args).await,
            "clock" => Ok(json!({"unix_seconds":excalibur_store::unix_seconds(),"timezone":"UTC"})),
            "status" => {
                if args["action"] == "block" {
                    return Ok(
                        json!({"task_state":"blocked","reason":args["reason"],"scope":self.scope}),
                    );
                }
                let (scope, run) = (self.scope.clone(), run.to_owned());
                self.db.read(ReadLane::Interactive,move|c| {
                    let state=c.query_row("SELECT state FROM runs WHERE id=?1",[&run],|row|row.get::<_,String>(0)).optional()?;
                    let count=c.query_row("SELECT count(*) FROM eval_episode_records WHERE scope=?1",[&scope],|row|row.get::<_,u64>(0))?;
                    Ok(json!({"scope":scope,"run_id":run,"run_state":state,"task_records":count}))
                }).await.map_err(|e|e.to_string())
            }
            "search_tools" => {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase();
                let results = self
                    .catalog
                    .iter()
                    .filter(|d| {
                        format!("{} {}", d.name, d.description)
                            .to_lowercase()
                            .contains(&query)
                    })
                    .take(20)
                    .collect::<Vec<_>>();
                Ok(json!({"tools":results,"frozen":true,"scope":self.scope}))
            }
            _ => Err("tool is unavailable in the evaluation episode".into()),
        }
    }
}
impl ToolRuntime for EpisodeTools {
    fn sanitize_durable(&self, value: &Value) -> Result<Value, String> {
        self.delegate.sanitize_durable(value)
    }
    fn pending_approval<'a>(&'a self, run: &'a str) -> BoxFuture<'a, bool> {
        self.delegate.pending_approval(run)
    }
    fn approval_blocked<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Option<String>> {
        self.delegate.approval_blocked(run)
    }
    fn observe_untrusted<'a>(
        &'a self,
        run: &'a str,
        source: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.observe_untrusted(run, source)
    }
    fn execution_tool_budget<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<u64>, String>> {
        self.delegate.execution_tool_budget(run)
    }
    fn finalization_guard<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn FinalizationGuard>, String>> {
        self.delegate.finalization_guard(run)
    }
    fn result_requires_approval(&self, result: &ToolResult) -> bool {
        self.delegate.result_requires_approval(result)
    }
    fn durable_blob_inspector(&self) -> Option<Box<dyn DurableByteInspector>> {
        self.delegate.durable_blob_inspector()
    }
    fn audit_builtin<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
        result: Option<&'a ToolResult>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.audit_builtin(run, call, result)
    }
    fn configure_process_boundary<'a>(
        &'a self,
        run: &'a str,
        boundary: ProcessBoundary,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_process_boundary(run, boundary)
    }
    fn configure_network_authorizer<'a>(
        &'a self,
        run: &'a str,
        guard: Arc<dyn NetworkAuthorizer>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.delegate.configure_network_authorizer(run, guard)
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut catalog = self.catalog.clone();
        for (name, description, schema) in [
            (
                "task_db",
                "Atomic episode-local create_task/update_task/create_record/update_record/query. Updates require expected_version; queries return at most 20 rows and 8000 bytes with a cursor. No projects or full planner.",
                task_schema(),
            ),
            (
                "clock",
                "Read the current Unix time in UTC.",
                json!({"type":"object","additionalProperties":false}),
            ),
            (
                "status",
                "Read this episode's state, or report missing mandatory input with action=block and a reason; blocking never asks the user.",
                json!({"type":"object","properties":{"action":{"enum":["get","block"]},"reason":{"type":"string","minLength":1,"maxLength":2048}},"additionalProperties":false}),
            ),
            (
                "search_tools",
                "Search only the actual frozen local-tool catalog; no installation or discovery outside the episode.",
                json!({"type":"object","properties":{"query":{"type":"string","maxLength":256}},"additionalProperties":false}),
            ),
            (
                "call_tool",
                "Execute a tool from the frozen local catalog through its existing security adapter.",
                json!({"type":"object","required":["name","arguments"],"properties":{"name":{"type":"string"},"arguments":{"type":"object"}},"additionalProperties":false}),
            ),
        ] {
            catalog.push(ToolDefinition {
                name: name.into(),
                description: description.into(),
                input_schema: schema,
            });
        }
        catalog.sort_by(|a, b| a.name.cmp(&b.name));
        catalog
    }
    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async move {
            if LOCAL.contains(&call.name.as_str()) {
                self.delegate.scheduling(run, call).await
            } else if call.name == "call_tool" {
                let inner = self.dynamic(call).ok()?;
                self.delegate.scheduling(run, &inner).await
            } else {
                Some(ToolScheduling {
                    resources: vec![Resource {
                        key: format!("episode:{}", self.scope),
                        write: call.name == "task_db",
                    }],
                    exclusive: false,
                })
            }
        })
    }
    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            if LOCAL.contains(&call.name.as_str()) {
                if !self
                    .catalog
                    .iter()
                    .any(|definition| definition.name == call.name)
                {
                    return Err("tool is absent from the frozen episode catalog".into());
                }
                let inner = self.delegate.prepare(run, call.clone()).await?;
                return Ok(self.wrap(call, inner));
            }
            if call.name == "call_tool" {
                let inner = self.delegate.prepare(run, self.dynamic(&call)?).await?;
                return Ok(self.wrap(call, inner));
            }
            let args = self.args(&call)?;
            match call.name.as_str() {
                "task_db" => validate_batch(&self.scope, &args)?,
                "clock" => keys(&args, &[])?,
                "status" => {
                    keys(&args, &["action", "reason"])?;
                    match args.get("action").and_then(Value::as_str) {
                        None if args.get("action").is_none() => {}
                        Some("get") => {}
                        Some("block") => {
                            if args["reason"]
                                .as_str()
                                .is_none_or(|s| s.trim().is_empty() || s.len() > 2048)
                            {
                                return Err(
                                    "blocking requires a reason of at most 2048 bytes".into()
                                );
                            }
                        }
                        _ => return Err("unknown episode status action".into()),
                    }
                    if args["action"] != "block" && args.get("reason").is_some() {
                        return Err("a reason is valid only for a blocking status".into());
                    }
                }
                "search_tools" => {
                    keys(&args, &["query"])?;
                    if args
                        .get("query")
                        .is_some_and(|q| !q.is_string() || q.as_str().unwrap().len() > 256)
                    {
                        return Err("search query must be a string up to 256 bytes".into());
                    }
                }
                _ => return Err("tool is disabled or unavailable in the evaluation episode".into()),
            }
            let keyed = call.name == "task_db";
            Ok(PreparedTool {
                call,
                effect: if keyed { Effect::Keyed } else { Effect::Pure },
                resources: vec![Resource {
                    key: format!("episode:{}", self.scope),
                    write: keyed,
                }],
                exclusive: false,
                data: json!({"scope":self.scope,"args":args}),
            })
        })
    }
    fn execute<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            if LOCAL.contains(&tool.call.name.as_str()) || tool.call.name == "call_tool" {
                return match self.inner(tool) {
                    Ok(inner) => self.delegate.execute(run, key, &inner).await,
                    Err(error) => ToolResult::error(error),
                };
            }
            if self
                .delegate
                .audit_builtin(run, &tool.call, None)
                .await
                .is_err()
            {
                return ToolResult::error("episode tool audit admission failed");
            }
            let result = match self.own_execute(run, key, tool).await {
                Ok(value) => match self.delegate.sanitize_durable(&value) {
                    Ok(value) => ToolResult::ok(value),
                    Err(error) => ToolResult::error(error),
                },
                Err(error) => ToolResult::error(error),
            };
            if self
                .delegate
                .audit_builtin(run, &tool.call, Some(&result))
                .await
                .is_err()
            {
                return ToolResult::error(
                    "episode tool result audit failed; inspect the durable operation receipt",
                );
            }
            result
        })
    }
    fn reconcile<'a>(
        &'a self,
        run: &'a str,
        key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async move {
            if LOCAL.contains(&tool.call.name.as_str()) || tool.call.name == "call_tool" {
                return match self.inner(tool) {
                    Ok(inner) => self.delegate.reconcile(run, key, &inner).await,
                    Err(error) => Reconcile::Uncertain(error),
                };
            }
            let args = match self.own_args(tool) {
                Ok(args) => args,
                Err(error) => return Reconcile::Uncertain(error),
            };
            if tool.call.name != "task_db" {
                return Reconcile::Retry;
            }
            let hash = crate::files::hash(args.to_string().as_bytes());
            match self.receipt(key, &hash).await {
                Ok(Some(result)) => match self.delegate.sanitize_durable(&result) {
                    Ok(result) => Reconcile::Done(ToolResult::ok(result)),
                    Err(error) => Reconcile::Uncertain(error),
                },
                Ok(None) => Reconcile::Retry,
                Err(error) => Reconcile::Uncertain(error),
            }
        })
    }
}
fn task_schema() -> Value {
    let id = json!({"type":"string","minLength":1,"maxLength":128,"description":"At most 128 UTF-8 bytes, without control characters."});
    let data = json!({"type":"object","description":"Record fields, at most 4096 serialized UTF-8 bytes. Updates merge these fields; null can clear a value."});
    let entity = json!({"enum":ENTITIES});
    json!({"type":"object","required":["ops"],"properties":{"ops":{"type":"array","minItems":1,"maxItems":64,"items":{"oneOf":[
        {"type":"object","required":["op","id","title"],"properties":{"op":{"const":"create_task"},"id":id,"title":id,"scope":{"type":"string"},"data":data},"additionalProperties":false},
        {"type":"object","required":["op","id","entity"],"properties":{"op":{"const":"create_record"},"id":id,"entity":entity,"scope":{"type":"string"},"data":data},"additionalProperties":false},
        {"type":"object","required":["op","id","expected_version"],"properties":{"op":{"const":"update_task"},"id":id,"title":id,"scope":{"type":"string"},"data":data,"expected_version":{"type":"integer","minimum":1}},"additionalProperties":false},
        {"type":"object","required":["op","id","expected_version"],"properties":{"op":{"const":"update_record"},"id":id,"entity":entity,"scope":{"type":"string"},"data":data,"expected_version":{"type":"integer","minimum":1}},"additionalProperties":false},
        {"type":"object","required":["op"],"properties":{"op":{"const":"query"},"entity":entity,"scope":{"type":"string"},"after":{"type":"string","maxLength":128},"limit":{"type":"integer","minimum":1,"maximum":20,"default":20}},"additionalProperties":false}
    ]}}},"additionalProperties":false})
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
        .ok_or_else(|| format!("{key} must contain 1..128 printable bytes"))
}
fn keys(value: &Value, allowed: &[&str]) -> Result<(), String> {
    if value
        .as_object()
        .ok_or("expected object")?
        .keys()
        .any(|key| !allowed.contains(&key.as_str()))
    {
        Err("unsupported field or namespace in episode operation".into())
    } else {
        Ok(())
    }
}
fn validate_batch(scope: &str, args: &Value) -> Result<(), String> {
    keys(args, &["ops"])?;
    let ops = args["ops"].as_array().ok_or("ops must be an array")?;
    if ops.is_empty() || ops.len() > 64 {
        return Err("ops must contain 1..64 operations".into());
    }
    for op in ops {
        if op.get("scope").is_some_and(|s| s != scope) {
            return Err("operation targets another episode scope".into());
        }
        let name = text(op, "op")?;
        match name {
            "create_task" | "update_task" => {
                keys(
                    op,
                    &["op", "scope", "id", "title", "data", "expected_version"],
                )?;
                text(op, "id")?;
                if name == "create_task" {
                    text(op, "title")?;
                }
                if op.get("title").is_some() {
                    text(op, "title")?;
                }
            }
            "create_record" | "update_record" => {
                keys(
                    op,
                    &["op", "scope", "id", "entity", "data", "expected_version"],
                )?;
                text(op, "id")?;
                if name == "create_record" && !ENTITIES.contains(&text(op, "entity")?) {
                    return Err("entity is disabled in the evaluation episode".into());
                }
                if op.get("entity").is_some() && !ENTITIES.contains(&text(op, "entity")?) {
                    return Err("entity is disabled in the evaluation episode".into());
                }
            }
            "query" => {
                keys(op, &["op", "scope", "entity", "limit", "after"])?;
                if op
                    .get("entity")
                    .is_some_and(|v| !ENTITIES.contains(&v.as_str().unwrap_or("")))
                {
                    return Err("query entity is unavailable".into());
                }
                if op.get("after").is_some_and(|value| {
                    value
                        .as_str()
                        .is_none_or(|s| s.len() > 128 || s.chars().any(char::is_control))
                }) {
                    return Err("query cursor must be a string up to 128 bytes".into());
                }
                if op
                    .get("limit")
                    .is_some_and(|v| v.as_u64().is_none_or(|n| n == 0 || n > 20))
                {
                    return Err("queries allow 1..20 rows".into());
                }
            }
            _ => return Err("project, personal and cross-scope operations are disabled".into()),
        }
        if name.starts_with("update_")
            && op
                .get("expected_version")
                .and_then(Value::as_u64)
                .is_none_or(|n| n == 0 || n > i64::MAX as u64)
        {
            return Err("updates require a positive expected_version".into());
        }
        if op
            .get("data")
            .is_some_and(|data| !data.is_object() || data.to_string().len() > 4096)
        {
            return Err("record data must be an object up to 4096 bytes".into());
        }
    }
    Ok(())
}
fn apply_batch(
    connection: &rusqlite::Connection,
    scope: &str,
    args: &Value,
) -> Result<Value, String> {
    validate_batch(scope, args)?;
    let mut results = Vec::new();
    for op in args["ops"].as_array().unwrap() {
        let name = text(op, "op")?;
        if name == "query" {
            let entity = op.get("entity").and_then(Value::as_str);
            let after = op.get("after").and_then(Value::as_str).unwrap_or("");
            let limit = op.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
            let mut statement=connection.prepare("SELECT id,entity,version,data FROM eval_episode_records WHERE scope=?1 AND (?2 IS NULL OR entity=?2) AND id>?3 ORDER BY id LIMIT 21").map_err(|e|e.to_string())?;
            let rows = statement
                .query_map(rusqlite::params![scope, entity, after], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            let mut output = Vec::new();
            let mut bytes = 0;
            let mut more = false;
            for (id, entity, version, data) in rows {
                let value = json!({"id":id,"entity":entity,"version":version,"data":serde_json::from_str::<Value>(&data).map_err(|e|e.to_string())?});
                let size = value.to_string().len();
                if output.len() >= limit || bytes + size > 8000 {
                    more = true;
                    break;
                }
                bytes += size;
                output.push(value);
            }
            let cursor = output.last().map(|value| value["id"].clone());
            results.push(json!({"rows":output,"more":more,"next":cursor}));
            continue;
        }
        let id = text(op, "id")?;
        let mut data = op.get("data").cloned().unwrap_or(json!({}));
        if let Some(title) = op.get("title") {
            data["title"] = title.clone();
        }
        if name.starts_with("create_") {
            if data.to_string().len() > 4096 {
                return Err("record exceeds the 4096-byte data limit".into());
            }
            let entity = if name == "create_task" {
                "task"
            } else {
                text(op, "entity")?
            };
            connection
                .execute(
                    "INSERT INTO eval_episode_records VALUES(?1,?2,?3,1,?4)",
                    rusqlite::params![scope, id, entity, data.to_string()],
                )
                .map_err(|e| format!("record creation failed: {e}"))?;
            results.push(json!({"id":id,"entity":entity,"version":1}));
        } else {
            let expected = op["expected_version"].as_u64().unwrap();
            let old = connection
                .query_row(
                    "SELECT entity,version,data FROM eval_episode_records WHERE scope=?1 AND id=?2",
                    [scope, id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, u64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|e| e.to_string())?
                .ok_or("record does not exist in this episode")?;
            if old.1 != expected {
                return Err(format!("version conflict: current_version={}", old.1));
            }
            if name == "update_task" && old.0 != "task" {
                return Err("update_task requires a task record".into());
            }
            if op
                .get("entity")
                .is_some_and(|entity| entity.as_str() != Some(old.0.as_str()))
            {
                return Err("record entity cannot change during update".into());
            }
            let patch = data;
            data = serde_json::from_str(&old.2).map_err(|_| "stored episode record is invalid")?;
            let object = data
                .as_object_mut()
                .ok_or("stored episode record must be an object")?;
            for (key, value) in patch.as_object().unwrap() {
                object.insert(key.clone(), value.clone());
            }
            if data.to_string().len() > 4096 {
                return Err("updated record exceeds the 4096-byte data limit".into());
            }
            connection.execute("UPDATE eval_episode_records SET version=version+1,data=?3 WHERE scope=?1 AND id=?2",rusqlite::params![scope,id,data.to_string()]).map_err(|e|e.to_string())?;
            results.push(json!({"id":id,"entity":old.0,"version":expected+1}));
        }
    }
    Ok(json!({"scope":scope,"results":results}))
}

use excalibur_engine::{Budget, Criterion, RunSpec};
use excalibur_master::{FastActions, Master};
use excalibur_orchestrator::{Control, Delegation, DirectiveImpact, Orchestrator};
use excalibur_store::{
    Priority,
    rusqlite::{self, OptionalExtension},
};
use excalibur_tasks::TaskStore;
use excalibur_types::runtime::{BoxFuture, ToolDefinition};
use excalibur_types::{CriterionId, RequirementId, TaskId};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
/// Interactive adapter to shared durable services. No file or terminal operation
/// executes in a master turn; work always becomes a level-one engine run.
pub struct DaemonActions {
    pub orchestrator: Orchestrator,
    pub master_agent: String,
    master: Mutex<Option<Master>>,
    tasks: Option<TaskStore>,
    security: Option<excalibur_security::Security>,
    worktree_roots: std::collections::BTreeMap<String, std::path::PathBuf>,
    redactor: excalibur_vault::Redactor,
}
impl DaemonActions {
    pub fn new(orchestrator: Orchestrator, master_agent: String) -> Self {
        Self {
            orchestrator,
            master_agent,
            master: Mutex::new(None),
            tasks: None,
            security: None,
            worktree_roots: Default::default(),
            redactor: Default::default(),
        }
    }
    pub fn with_security(mut self, security: excalibur_security::Security) -> Self {
        self.security = Some(security);
        self
    }
    pub fn with_worktree_roots(
        mut self,
        roots: std::collections::BTreeMap<String, std::path::PathBuf>,
    ) -> Self {
        self.worktree_roots = roots;
        self
    }
    pub fn with_tasks(mut self, tasks: TaskStore) -> Self {
        self.tasks = Some(tasks);
        self
    }
    pub fn with_redactor(mut self, redactor: excalibur_vault::Redactor) -> Self {
        self.redactor = redactor;
        self
    }
    pub fn bind_master(&self, master: Master) {
        *self.master.lock().unwrap_or_else(|p| p.into_inner()) = Some(master);
    }
    pub fn detach(&self) {
        self.master.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
    fn master(&self) -> Result<Master, String> {
        self.master
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| "master not bound".into())
    }
    async fn task_batch(&self, scope: &str, key: &str, args: Value) -> Result<Value, String> {
        if [scope, key]
            .iter()
            .any(|value| self.redactor.redact_text(value) != *value)
        {
            return Err("secret literal in task metadata".into());
        }
        let ceiling = self
            .orchestrator
            .agent(&self.master_agent)
            .await
            .map_err(|e| e.to_string())?
            .token;
        if !ceiling.data_scopes.contains(scope) {
            return Err("task scope exceeds provisioned master authority".into());
        }
        let tasks = self
            .tasks
            .as_ref()
            .ok_or("durable task service is not configured")?;
        if self.redactor.redact_json(&args) != args {
            return Err("secret literal in task update; use a secret reference".into());
        }
        let update = args["ops"].as_array().is_some_and(|ops| {
            ops.iter().any(|op| {
                matches!(
                    op["op"].as_str(),
                    Some("update_requirement" | "update_criterion" | "update_task")
                )
            })
        });
        tasks
            .validate_batch(scope, &args)
            .map_err(|e| e.to_string())?;
        if update {
            let (scope, key, encoded) = (scope.to_owned(), key.to_owned(), args.to_string());
            self.orchestrator.database().write(Priority::Interactive,move|conn|{
                conn.execute_batch("CREATE TABLE IF NOT EXISTS daemon_task_directives(revision INTEGER PRIMARY KEY AUTOINCREMENT,operation TEXT NOT NULL UNIQUE,scope TEXT NOT NULL,arguments TEXT NOT NULL CHECK(json_valid(arguments)),state TEXT NOT NULL DEFAULT 'pending')")?;
                let old:Option<(String,String)>=conn.query_row("SELECT scope,arguments FROM daemon_task_directives WHERE operation=?1",[&key],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
                if old.is_some_and(|old|old!=(scope.clone(),encoded.clone())){return Err(excalibur_store::Error::Invalid("task correction operation reused"));}
                conn.execute("INSERT OR IGNORE INTO daemon_task_directives(operation,scope,arguments) VALUES(?1,?2,?3)",(&key,&scope,&encoded))?;Ok(())
            }).await.map_err(|e|e.to_string())?;
        }
        let result = tasks
            .batch(scope, key, args)
            .await
            .map_err(|e| e.to_string())?;
        if update {
            self.recover_task_directives().await?;
        }
        Ok(result)
    }
    /// Replays only corrections with a confirmed TaskStore receipt. The host
    /// invokes this after recovery and on maintenance ticks.
    pub async fn recover_task_directives(&self) -> Result<usize, String> {
        let Some(tasks) = &self.tasks else {
            return Ok(0);
        };
        let records=self.orchestrator.database().write(Priority::Interactive,|conn|{
            conn.execute_batch("CREATE TABLE IF NOT EXISTS daemon_task_directives(revision INTEGER PRIMARY KEY AUTOINCREMENT,operation TEXT NOT NULL UNIQUE,scope TEXT NOT NULL,arguments TEXT NOT NULL CHECK(json_valid(arguments)),state TEXT NOT NULL DEFAULT 'pending')")?;
            let mut query=conn.prepare("SELECT revision,operation,scope,arguments FROM daemon_task_directives WHERE state='pending' AND EXISTS(SELECT 1 FROM task_batch_receipts receipt WHERE receipt.scope_id=daemon_task_directives.scope AND receipt.operation_key=daemon_task_directives.operation) ORDER BY revision LIMIT 64")?;
            Ok(query.query_map([],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)
        }).await.map_err(|e|e.to_string())?;
        let mut processed = 0;
        for (revision, key, scope, args) in records {
            let args: Value =
                serde_json::from_str(&args).map_err(|_| "invalid task correction record")?;
            let Some(receipt) = tasks
                .receipt(&scope, &key, &args)
                .await
                .map_err(|e| e.to_string())?
            else {
                continue;
            };
            let mut affected = BTreeSet::new();
            let mut patterns = BTreeSet::new();
            let mut changes = Vec::new();
            for (index, op) in args["ops"]
                .as_array()
                .ok_or("invalid correction operations")?
                .iter()
                .enumerate()
            {
                let Some(name) = op["op"].as_str().filter(|name| {
                    matches!(
                        *name,
                        "update_requirement" | "update_criterion" | "update_task"
                    )
                }) else {
                    continue;
                };
                let id = receipt["results"][index]["id"]
                    .as_str()
                    .ok_or("correction receipt lacks entity")?
                    .to_owned();
                let (current_scope, entity, name_owned) =
                    (scope.clone(), id.clone(), name.to_owned());
                let (target_tasks,target_patterns)=self.orchestrator.database().read(excalibur_store::ReadLane::Interactive,move|conn|{
                    let sql=match name_owned.as_str(){
                        "update_task"=>"SELECT id FROM tasks WHERE id=?1 AND scope_id=?2",
                        "update_criterion"=>"SELECT tr.task_id FROM task_requirements tr JOIN criteria cr ON cr.requirement_id=tr.requirement_id WHERE cr.id=?1 AND cr.scope_id=?2",
                        _=>"SELECT tr.task_id FROM task_requirements tr JOIN requirements r ON r.id=tr.requirement_id WHERE r.id=?1 AND r.scope_id=?2",
                    };
                    let mut query=conn.prepare(sql)?;let found=query.query_map((&entity,&current_scope),|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
                    let mut patterns=Vec::new();for task in &found{
                        let mut query=conn.prepare("SELECT COALESCE(cr.impact_area,'[]') FROM criteria cr JOIN task_requirements tr ON tr.requirement_id=cr.requirement_id WHERE tr.task_id=?1 AND cr.scope_id=?2")?;
                        for row in query.query_map((task,&current_scope),|r|r.get::<_,String>(0))?{patterns.extend(serde_json::from_str::<Vec<String>>(&row?).map_err(|_|excalibur_store::Error::Invalid("invalid criterion impact area"))?);}
                    }Ok((found,patterns))
                }).await.map_err(|e|e.to_string())?;
                affected.extend(target_tasks);
                patterns.extend(target_patterns);
                changes.push(
                    json!({"entity":name,"id":id,"literal_change":op.get("data").unwrap_or(op)}),
                );
            }
            if !affected.is_empty() {
                let mut roots = BTreeSet::new();
                for agent in self
                    .orchestrator
                    .active_agents()
                    .await
                    .map_err(|e| e.to_string())?
                {
                    if affected.contains(&agent.task) {
                        roots.extend(agent.token.roots.into_iter().map(|root| root.path));
                    }
                }
                let mut resources = BTreeSet::new();
                for root in roots {
                    for pattern in &patterns {
                        let prefix = pattern
                            .split(['*', '?', '[', '{'])
                            .next()
                            .unwrap_or("")
                            .trim_end_matches('/');
                        resources.insert(root.join(prefix).to_string_lossy().to_string());
                    }
                }
                let impact = DirectiveImpact {
                    tasks: affected,
                    resources,
                    instruction_revision: revision,
                    text: json!({"scope":scope,"changes":changes}).to_string(),
                    ..Default::default()
                };
                self.orchestrator
                    .propagate_directive(&self.master_agent, impact)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            self.orchestrator
                .database()
                .write(Priority::Interactive, move |conn| {
                    conn.execute(
                        "UPDATE daemon_task_directives SET state='processed' WHERE operation=?1",
                        [key],
                    )?;
                    Ok(())
                })
                .await
                .map_err(|e| e.to_string())?;
            processed += 1;
        }
        Ok(processed)
    }
    async fn request_task(
        &self,
        conversation: &str,
        request: (&str, Vec<Value>),
        goal: &str,
        deliverable: &str,
        operation: &str,
        budget: &Budget,
    ) -> Result<String, String> {
        let (scope, originals) = request;
        if [goal, deliverable, operation]
            .iter()
            .any(|value| self.redactor.redact_text(value) != *value)
        {
            return Err("secret literal in delegation; use a secret reference".into());
        }
        let tasks = self
            .tasks
            .as_ref()
            .ok_or("durable task service is not configured")?;
        let request = (
            conversation.to_owned(),
            scope.to_owned(),
            goal.to_owned(),
            deliverable.to_owned(),
            operation.to_owned(),
            originals,
        );
        let (task_id,card)=self.orchestrator.database().write(Priority::Interactive,move|conn|{
            let (conversation,scope,goal,deliverable,operation,originals)=request;
            conn.execute_batch("CREATE TABLE IF NOT EXISTS daemon_task_requests(operation TEXT PRIMARY KEY,task_id TEXT NOT NULL UNIQUE,conversation TEXT NOT NULL,scope TEXT NOT NULL,goal TEXT NOT NULL,deliverable TEXT NOT NULL,request_card TEXT NOT NULL CHECK(json_valid(request_card)))")?;
            let known:Option<(String,String,String,String,String,String)>=conn.query_row("SELECT task_id,conversation,scope,goal,deliverable,request_card FROM daemon_task_requests WHERE operation=?1",[&operation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
            if let Some((task,old_conversation,old_scope,old_goal,old_deliverable,card))=known {
                if old_conversation!=conversation||old_scope!=scope||old_goal!=goal||old_deliverable!=deliverable{return Err(excalibur_store::Error::Invalid("delegation task operation reused with different arguments"));}
                return Ok((task,card));
            }
            if originals.is_empty(){return Err(excalibur_store::Error::Invalid("delegation requires persisted literal source messages"));}
            let typed_id=TaskId::new();
            let requirement=RequirementId::from_ulid(typed_id.ulid()).to_string();
            let criterion=CriterionId::from_ulid(typed_id.ulid()).to_string();
            let pins=client_constraints(&originals);
            let card=json!({"original":originals,"interpretation":goal,"requirements":[{"id":requirement,"type":"functional","text":goal}],"criteria":[{"id":criterion,"requirement_id":requirement,"method":"inspection","statement":deliverable}],"assumptions":[],"open_questions":[],"pinned_instructions":pins}).to_string();
            let id=typed_id.to_string();
            conn.execute("INSERT INTO daemon_task_requests VALUES(?1,?2,?3,?4,?5,?6,?7)",(&operation,&id,&conversation,&scope,&goal,&deliverable,&card))?;
            Ok((id,card))
        }).await.map_err(|e|e.to_string())?;
        let id = task_id
            .parse::<TaskId>()
            .map_err(|_| "invalid durable task identity")?;
        let requirement = RequirementId::from_ulid(id.ulid()).to_string();
        let criterion = CriterionId::from_ulid(id.ulid()).to_string();
        let mut card: Value =
            serde_json::from_str(&card).map_err(|_| "invalid durable request card")?;
        // Pending receipts from earlier hosts may contain literal-only references.
        card["requirements"][0]["id"] = json!(requirement);
        card["criteria"][0]["id"] = json!(criterion);
        card["criteria"][0]["requirement_id"] = json!(requirement);
        let goal = card["interpretation"]
            .as_str()
            .ok_or("missing persisted interpretation")?;
        let deliverable = card["criteria"][0]["statement"]
            .as_str()
            .ok_or("missing persisted criterion")?;
        let scope = scope.to_owned();
        let conversation = conversation.to_owned();
        let lookup_task = task_id.clone();
        let lookup_scope = scope.clone();
        let exists=tasks.database().read(excalibur_store::ReadLane::Interactive,move|conn|Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND COALESCE(scope_id,'general')=?2)",(&lookup_task,&lookup_scope),|row|row.get::<_,bool>(0))?)).await.map_err(|e|e.to_string())?;
        let existing = if exists {
            Some(
                tasks
                    .task(&scope, &task_id)
                    .await
                    .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };
        if let Some(task) = &existing {
            if task.metadata["delegation_operation"] != operation
                || task.origin_conversation_id.as_deref() != Some(conversation.as_str())
                || task.request_card["original"] != card["original"]
                || task.request_card["interpretation"] != card["interpretation"]
            {
                return Err("existing task does not match the durable delegation receipt".into());
            }
        } else {
            tasks.host_batch(&scope,&format!("request-task:{operation}"),json!({"ops":[
            {"op":"create_requirement","id":requirement,"data":{"text":goal,"state":"accepted","kind":"functional","priority":"must","project_id":scope.strip_prefix("project:")}},
            {"op":"create_criterion","id":criterion,"data":{"requirement_id":requirement,"statement":deliverable,"method":"inspection","expected":"Nonempty final response; additional observable criteria and checks remain authoritative when present","impact_area":[],"metadata":{"implicit_final_response":true}}},
            {"op":"create_task","id":task_id,"title":goal.chars().take(120).collect::<String>(),"data":{"resolve_request_card_codes":true,"request_card":card.clone(),"budget":budget,"project_id":scope.strip_prefix("project:"),"origin_conversation_id":conversation,"metadata":{"delegation_operation":operation}}},
            {"op":"link_requirement","task":task_id,"requirement":requirement}
        ]})).await.map_err(|e|e.to_string())?;
        }
        let card = tasks
            .task(&scope, &task_id)
            .await
            .map_err(|e| e.to_string())?
            .request_card;
        let registry = excalibur_journal::instructions::InstructionStore::open(
            self.orchestrator.database().clone(),
            self.redactor.clone(),
        )
        .await
        .map_err(|e| e.to_string())?;
        for (index, pin) in card["pinned_instructions"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            if let Some(text) = pin.as_str() {
                registry
                    .register(
                        &format!("request:{task_id}:instruction:{index}"),
                        None,
                        excalibur_journal::instructions::InstructionScope::Task(task_id.clone()),
                        excalibur_journal::instructions::InstructionOrigin {
                            kind: excalibur_journal::instructions::InstructionOriginKind::Host,
                            reference: card["original"][0]["msg_id"].as_str().unwrap_or("").into(),
                        },
                        text,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(task_id)
    }
    async fn targets(&self, target: Option<&str>) -> Result<Vec<String>, String> {
        if let Some(target) = target.filter(|s| !s.is_empty() && *s != "all") {
            if let Ok(agent) = self.orchestrator.agent(target).await {
                return Ok(vec![agent.id]);
            }
            if let Ok(agent) = self.orchestrator.agent_for_run(target).await {
                return Ok(vec![agent.id]);
            }
        }
        let agents = self
            .orchestrator
            .active_agents()
            .await
            .map_err(|e| e.to_string())?;
        let Some(target) = target.filter(|s| !s.is_empty()) else {
            return Ok(agents
                .into_iter()
                .filter(|a| a.depth > 0)
                .map(|a| a.id)
                .collect());
        };
        let found = agents
            .into_iter()
            .filter(|a| {
                a.id == target
                    || a.name.eq_ignore_ascii_case(target)
                    || a.run_id.as_deref() == Some(target)
                    || a.task == target
            })
            .map(|a| a.id)
            .collect::<Vec<_>>();
        if found.len() == 1 {
            Ok(found)
        } else {
            Err(format!(
                "target is missing or ambiguous; candidates: {}",
                found.join(", ")
            ))
        }
    }
    async fn approval_command(
        &self,
        conversation: &str,
        approved: bool,
        target: Option<&str>,
        key: &str,
    ) -> Result<Value, String> {
        use excalibur_gateway::{RpcBackend, SharedBackend};
        let security = self
            .security
            .as_ref()
            .ok_or("approval service is not configured")?;
        let master = self.master()?;
        master
            .conversation_scope(conversation)
            .await
            .map_err(|_| "unknown authenticated conversation")?;
        let (conversation, key, requested) = (
            conversation.to_owned(),
            key.to_owned(),
            target.filter(|id| !id.is_empty()).map(str::to_owned),
        );
        let now = excalibur_store::unix_seconds();
        let approval_key = key.clone();
        let (id,receipt)=self.orchestrator.database().write(Priority::Interactive,move|connection|{
            connection.execute_batch("CREATE TABLE IF NOT EXISTS daemon_approval_commands(operation TEXT PRIMARY KEY,conversation TEXT NOT NULL,approved INTEGER NOT NULL,requested_target TEXT,approval_id TEXT NOT NULL,result TEXT)")?;
            type ApprovalCommandReceipt = (String,bool,Option<String>,String,Option<String>);
            let previous:Option<ApprovalCommandReceipt>=connection.query_row("SELECT conversation,approved,requested_target,approval_id,result FROM daemon_approval_commands WHERE operation=?1",[&key],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional()?;
            if let Some((owner,decision,old_target,id,receipt))=previous{
                if owner!=conversation||decision!=approved||old_target!=requested{return Err(excalibur_store::Error::Invalid("approval command operation reused"));}return Ok((id,receipt));
            }
            let mut query=connection.prepare("SELECT a.id FROM security_approvals a JOIN orchestration_agents agent ON agent.run_id=a.run_id WHERE json_extract(agent.body,'$.origin_conversation')=?1 AND a.status='pending' AND a.expires_at>?2 AND (?3 IS NULL OR a.id=?3) ORDER BY a.created_at,a.id LIMIT 2")?;
            let candidates=query.query_map(rusqlite::params![conversation,now,requested],|row|row.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
            if candidates.len()!=1{return Err(excalibur_store::Error::Invalid("approval is missing or ambiguous in this conversation; supply its exact pending ID"));}
            let id=candidates[0].clone();
            connection.execute("INSERT INTO daemon_approval_commands(operation,conversation,approved,requested_target,approval_id) VALUES(?1,?2,?3,?4,?5)",rusqlite::params![key,conversation,approved,requested,id])?;Ok((id,None))
        }).await.map_err(|error|error.to_string())?;
        if let Some(receipt) = receipt {
            return serde_json::from_str(&receipt)
                .map_err(|_| "invalid approval command receipt".into());
        }
        // Reuse the same shared application operation as authenticated API
        // clients; no provider or model-callable approval tool is involved.
        let backend =
            SharedBackend::new(master, self.orchestrator.clone()).with_security(security.clone());
        let changed = backend
            .dispatch(
                "approval.respond",
                json!({"approval":id,"approved":approved,"scope":"once","operation":approval_key}),
            )
            .await
            .map_err(|error| error.message)?;
        let approval = id.clone();
        let actual = self
            .orchestrator
            .database()
            .read(excalibur_store::ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT status FROM security_approvals WHERE id=?1",
                    [approval],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .await
            .map_err(|_| "approval outcome unavailable")?;
        let result = json!({"approval_id":id,"requested_decision":if approved{"approved"}else{"rejected"},"decision":actual,"changed":changed});
        let encoded = result.to_string();
        self.orchestrator
            .database()
            .write(Priority::Interactive, move |connection| {
                connection.execute(
                    "UPDATE daemon_approval_commands SET result=?2 WHERE operation=?1",
                    rusqlite::params![approval_key, encoded],
                )?;
                Ok(())
            })
            .await
            .map_err(|error| error.to_string())?;
        Ok(result)
    }
    async fn dispatch(
        &self,
        conversation: &str,
        command: &str,
        target: Option<&str>,
        key: &str,
    ) -> Result<Value, String> {
        if matches!(command, "approve" | "deny") {
            return self
                .approval_command(conversation, command == "approve", target, key)
                .await;
        }
        if command == "project" {
            let ceiling = self
                .orchestrator
                .agent(&self.master_agent)
                .await
                .map_err(|e| e.to_string())?
                .token
                .data_scopes;
            let name = target.ok_or("project target required")?;
            let candidates = self
                .orchestrator
                .projects()
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|p| {
                    p.id == name
                        || p.name.eq_ignore_ascii_case(name)
                        || p.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
                })
                .collect::<Vec<_>>();
            if name == "general" {
                if !ceiling.contains("general") {
                    return Err("scope is not provisioned by the host".into());
                }
                self.master()?
                    .set_scope(conversation, "general")
                    .await
                    .map_err(|e| e.to_string())?;
                return Ok(json!({"scope":"general"}));
            }
            if candidates.len() != 1 {
                return Err("project is missing or ambiguous".into());
            }
            let scope = format!("project:{}", candidates[0].id);
            if !ceiling.contains(&scope) {
                return Err("scope is not provisioned by the host".into());
            }
            self.master()?
                .set_scope(conversation, &scope)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(json!({"scope":scope}));
        }
        if command == "stop" && target.is_none() {
            self.master()?
                .stop_turn(conversation)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(json!({"master_turn":"interrupted","background_runs":"unchanged"}));
        }
        let mut ids = self.targets(target.filter(|t| *t != "all")).await?;
        if command != "status" && target.is_none_or(|target| target == "all") {
            let roots = self
                .orchestrator
                .active_agents()
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|agent| agent.depth == 1)
                .map(|agent| agent.id)
                .collect::<BTreeSet<_>>();
            ids.retain(|id| roots.contains(id));
        }
        if command == "cancel" && ids.len() != 1 {
            let mut candidates = Vec::new();
            for id in ids {
                let agent = self
                    .orchestrator
                    .agent(&id)
                    .await
                    .map_err(|e| e.to_string())?;
                candidates.push(json!({"id":agent.id,"name":agent.name,"scope":agent.scope}));
            }
            return Ok(
                json!({"selection_required":true,"candidates":candidates,"action":"cancel"}),
            );
        }
        let mut result = Vec::new();
        for id in ids {
            if command == "status" {
                result.push(
                    serde_json::to_value(
                        self.orchestrator
                            .status(&id)
                            .await
                            .map_err(|e| e.to_string())?,
                    )
                    .map_err(|e| e.to_string())?,
                );
                continue;
            }
            let action = match command {
                "pause" | "stop" => Control::Pause,
                "resume" => Control::Resume,
                "interrupt" => Control::Interrupt,
                "cancel" => {
                    let agent = self
                        .orchestrator
                        .agent(&id)
                        .await
                        .map_err(|e| e.to_string())?;
                    return Ok(
                        json!({"confirmation_required":true,"target":id,"scope":agent.scope,"action":"cancel"}),
                    );
                }
                _ => return Err("unknown fast command".into()),
            };
            result.push(
                serde_json::to_value(
                    self.orchestrator
                        .control_keyed(&format!("{key}:{id}"), &id, action)
                        .await
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?,
            );
        }
        Ok(json!({"results":result}))
    }
}
fn definition(
    name: &str,
    description: &str,
    properties: Value,
    required: Vec<&str>,
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
    }
}
impl FastActions for DaemonActions {
    fn confirm_cancel<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
        target: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let master = self.master()?;
            if !master
                .authorizes_cancel(conversation, scope, target, key)
                .await
                .map_err(|e| e.to_string())?
            {
                return Err("cancellation lacks an authenticated accepted decision".into());
            }
            if master
                .conversation_scope(conversation)
                .await
                .map_err(|e| e.to_string())?
                != scope
            {
                return Err("cancellation scope changed".into());
            }
            let agent = self
                .orchestrator
                .agent(target)
                .await
                .map_err(|e| e.to_string())?;
            let ceiling = self
                .orchestrator
                .agent(&self.master_agent)
                .await
                .map_err(|e| e.to_string())?;
            if agent.scope != scope || !ceiling.token.data_scopes.contains(scope) {
                return Err("cancellation target exceeds conversation authority".into());
            }
            serde_json::to_value(
                self.orchestrator
                    .control_keyed(key, target, Control::Cancel { confirmed: true })
                    .await
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())
        })
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition{name:"task_db".into(),description:"Query or update versioned tasks and criteria in this turn scope; corrections propagate to affected work".into(),input_schema:excalibur_tasks::task_schema()},
            definition(
                "status",
                "Read deterministic agent status; no model needed",
                json!({"target":{"type":"string"}}),
                vec![],
            ),
            definition(
                "subagent",
                "Delegate long work or control an agent. Never execute files or terminal in this turn.",
                json!({"action":{"enum":["spawn","pause","resume","interrupt","cancel","redirect","message","answer"]},"target":{"type":"string"},"scope":{"type":"string"},"scopes":{"type":"array","minItems":1,"maxItems":8,"uniqueItems":true,"items":{"type":"string"}},"goal":{"type":"string"},"deliverable":{"type":"string"},"name":{"type":"string"},"text":{"type":"string"},"confirmed":{"type":"boolean"},"payload":{"type":["object","string"]}}),
                vec!["action"],
            ),
            definition(
                "question",
                "Ask a bounded clarification, blocking when an obligatory fact is missing",
                json!({"text":{"type":"string"},"blocking":{"type":"boolean"},"default_answer":{"type":"string"}}),
                vec!["text", "blocking"],
            ),
            definition(
                "clock",
                "Read the host's current UTC Unix timestamp",
                json!({}),
                vec![],
            ),
        ]
    }
    fn execute<'a>(
        &'a self,
        conversation: &'a str,
        command: &'a str,
        target: Option<&'a str>,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(self.dispatch(conversation, command, target, key))
    }
    fn invoke<'a>(
        &'a self,
        conversation: &'a str,
        name: &'a str,
        args: Value,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let scope = self
                .master()?
                .conversation_scope(conversation)
                .await
                .map_err(|e| e.to_string())?;
            self.invoke_scoped(conversation, &scope, name, args, key)
                .await
        })
    }
    fn invoke_scoped<'a>(
        &'a self,
        conversation: &'a str,
        scope: &'a str,
        name: &'a str,
        args: Value,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            match name {
                "task_db" => {
                    let operations = args["ops"].as_array().ok_or("task operations required")?;
                    let mut groups = std::collections::BTreeMap::<String, Vec<Value>>::new();
                    for operation in operations {
                        let target = match operation.get("scope") {
                            None => scope,
                            Some(Value::String(target)) => target,
                            _ => return Err("invalid task scope".into()),
                        };
                        groups
                            .entry(target.to_owned())
                            .or_default()
                            .push(operation.clone());
                    }
                    if groups.len() == 1 && groups.contains_key(scope) {
                        return self.task_batch(scope, key, args).await;
                    }
                    let requested = groups.keys().cloned().collect();
                    crate::project_scope::authorize(
                        &self.orchestrator,
                        &self.master_agent,
                        conversation,
                        scope,
                        &requested,
                        key,
                        &self.redactor,
                    )
                    .await?;
                    // Validate every group before the first effect. Transactions
                    // and receipts remain per scope; a retry replays each result.
                    let tasks = self.tasks.as_ref().ok_or("task service unavailable")?;
                    for (target, ops) in &groups {
                        tasks
                            .validate_batch(target, &json!({"ops":ops}))
                            .map_err(|e| e.to_string())?;
                    }
                    let mut results = vec![];
                    for (target, ops) in groups {
                        let group_key = serde_json::to_string(&(key, &target))
                            .map_err(|_| "invalid grouped operation")?;
                        let result = self
                            .task_batch(&target, &group_key, json!({"ops":ops}))
                            .await?;
                        results.push(json!({"scope":target,"result":result}));
                    }
                    Ok(json!({"scopes":results}))
                }
                "status" => {
                    self.dispatch(conversation, "status", args["target"].as_str(), key)
                        .await
                }
                "clock" => {
                    Ok(json!({"unix_seconds":excalibur_store::unix_seconds(),"timezone":"UTC"}))
                }
                "question" => {
                    let id = self
                        .master()?
                        .ask_scoped(
                            conversation,
                            scope,
                            args["text"].as_str().ok_or("question text required")?,
                            args["blocking"].as_bool().ok_or("blocking required")?,
                            args["default_answer"].as_str().map(String::from),
                            excalibur_store::unix_seconds(),
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(json!({"question_id":id}))
                }
                "subagent" => {
                    let action = args["action"].as_str().ok_or("action required")?;
                    if action != "spawn" {
                        let targets = self.targets(args["target"].as_str()).await?;
                        if targets.len() != 1 {
                            return Err("control requires one explicit target".into());
                        }
                        if matches!(action, "message" | "answer") {
                            let payload = args
                                .get("payload")
                                .cloned()
                                .ok_or("message payload required")?;
                            let kind = if action == "answer" {
                                "answer"
                            } else {
                                "directive"
                            };
                            let message = self
                                .orchestrator
                                .send_message(
                                    key,
                                    &self.master_agent,
                                    &targets[0],
                                    kind,
                                    payload,
                                    "normal",
                                )
                                .await
                                .map_err(|e| e.to_string())?;
                            return serde_json::to_value(message).map_err(|e| e.to_string());
                        }
                        let control = match action {
                            "pause" => Control::Pause,
                            "resume" => Control::Resume,
                            "interrupt" => Control::Interrupt,
                            "cancel" => Control::Cancel { confirmed: false },
                            "redirect" => Control::Redirect {
                                text: args["text"]
                                    .as_str()
                                    .ok_or("redirect text required")?
                                    .into(),
                            },
                            _ => return Err("unknown control action".into()),
                        };
                        return serde_json::to_value(
                            self.orchestrator
                                .control_keyed(key, &targets[0], control)
                                .await
                                .map_err(|e| e.to_string())?,
                        )
                        .map_err(|e| e.to_string());
                    }
                    let source_scope = scope;
                    let target_scope = match args.get("scope") {
                        None => scope.to_owned(),
                        Some(Value::String(target)) => target.clone(),
                        _ => return Err("invalid delegation scope".into()),
                    };
                    let scope = target_scope.as_str();
                    let requested: BTreeSet<String> = match args.get("scopes") {
                        None => BTreeSet::from([scope.to_owned()]),
                        Some(Value::Array(scopes)) if !scopes.is_empty() && scopes.len() <= 8 => {
                            scopes
                                .iter()
                                .map(|scope| {
                                    scope
                                        .as_str()
                                        .map(str::to_owned)
                                        .ok_or_else(|| "invalid delegation scopes".to_owned())
                                })
                                .collect::<Result<_, _>>()?
                        }
                        _ => return Err("invalid delegation scopes".into()),
                    };
                    if !requested.contains(scope) {
                        return Err("primary scope must be explicitly included".into());
                    }
                    let originals = crate::project_scope::authorize(
                        &self.orchestrator,
                        &self.master_agent,
                        conversation,
                        source_scope,
                        &requested,
                        key,
                        &self.redactor,
                    )
                    .await?;
                    let parent = self
                        .orchestrator
                        .agent(&self.master_agent)
                        .await
                        .map_err(|e| e.to_string())?;
                    let goal = args["goal"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or("delegation goal required")?;
                    let mut token = parent.token;
                    token
                        .data_scopes
                        .retain(|s| s == "general" || requested.contains(s));
                    let mut roots = vec![];
                    for target in &requested {
                        if let Some(project_id) = target.strip_prefix("project:") {
                            let project = self
                                .orchestrator
                                .project(project_id)
                                .await
                                .map_err(|e| e.to_string())?;
                            for path in project.roots {
                                if !token
                                    .roots
                                    .iter()
                                    .any(|root| path.starts_with(&root.path) && root.write)
                                {
                                    return Err("project roots exceed master authority".into());
                                }
                                roots.push(excalibur_security::Root { path, write: true });
                            }
                            // Git worktrees belong to one project. Transversal
                            // work uses source roots and delegates Git to scoped workers.
                            if requested.len() == 1
                                && let Some(managed) = self.worktree_roots.get(target)
                            {
                                roots.push(excalibur_security::Root {
                                    path: managed.clone(),
                                    write: true,
                                });
                            }
                        } else if target != "general" {
                            return Err(
                                "only registered project or general scopes may be delegated".into(),
                            );
                        }
                    }
                    if !roots.is_empty() {
                        token.roots = roots;
                    }
                    let budget = Budget {
                        max_steps: 150,
                        max_wall_seconds: 3600,
                        ..Budget::default()
                    };
                    let deliverable = args["deliverable"]
                        .as_str()
                        .unwrap_or("Verified result and explicit remaining work");
                    let pinned_instructions = client_constraints(&originals);
                    let task = self
                        .request_task(
                            conversation,
                            (scope, originals),
                            goal,
                            deliverable,
                            key,
                            &budget,
                        )
                        .await?;
                    let task_ulid = task
                        .parse::<TaskId>()
                        .map_err(|_| "invalid task ID")?
                        .ulid();
                    let inputs = vec![
                        task.clone(),
                        RequirementId::from_ulid(task_ulid).to_string(),
                        CriterionId::from_ulid(task_ulid).to_string(),
                    ];
                    let agent=self.orchestrator.spawn(&self.master_agent,Delegation{operation:format!("delegate:{task}"),name:args["name"].as_str().unwrap_or("Coordinator").into(),profile:"coordinator".into(),scope:scope.to_owned(),task:task.clone(),origin_conversation:conversation.into(),goal:goal.into(),deliverable:deliverable.into(),inputs,pinned_instructions,suggested_skills:vec![],fork_context:false,token,spec:RunSpec{context:excalibur_context::ContextInput::new("Work within the delegated scope and permissions. Ask rather than guessing irreversible details.",goal,"host-time-in-durable-journal"),budget,criteria:vec![Criterion::FinalResponse{schema:None}]},priority:1}).await.map_err(|e|e.to_string())?;
                    Ok(
                        json!({"agent_id":agent.id,"run_id":agent.run_id,"task_id":task,"state":"ready"}),
                    )
                }
                _ => Err("master tool unavailable".into()),
            }
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
        if name != "question" {
            return self.invoke_scoped(conversation, scope, name, args, key);
        }
        Box::pin(async move {
            // Model-selected optionality cannot waive a missing mandatory fact.
            let id = self
                .master()?
                .ask_for_source(
                    conversation,
                    scope,
                    source,
                    args["text"].as_str().ok_or("question text required")?,
                    true,
                    None,
                    excalibur_store::unix_seconds(),
                )
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({"question_id":id,"blocking":true}))
        })
    }
    fn context<'a>(&'a self, conversation: &'a str) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let mut local = Vec::new();
            let mut other = Vec::new();
            for a in self
                .orchestrator
                .active_agents()
                .await
                .map_err(|e| e.to_string())?
            {
                if a.depth != 1 {
                    continue;
                }
                let status = self
                    .orchestrator
                    .status(&a.id)
                    .await
                    .map_err(|e| e.to_string())?;
                if matches!(
                    status.state.as_str(),
                    "completed" | "completed_with_pending" | "cancelled" | "failed"
                ) {
                    continue;
                }
                if a.origin_conversation == conversation {
                    local.push(json!({"id":a.id,"name":a.name,"scope":a.scope,"state":status.state,"queue_position":status.queue_position,"step":status.step,"report_reminder":status.report_reminder}));
                } else if other.len() < 10 {
                    other.push(
                        json!({"id":a.id,"name":a.name,"scope":a.scope,"state":status.state}),
                    );
                }
            }
            let pending = self
                .orchestrator
                .mailbox(&self.master_agent)
                .await
                .map_err(|e| e.to_string())?;
            let pending = pending
                .into_iter()
                .take(32)
                .map(|message| json!({"id":message.id,"kind":message.kind,"sender":message.sender}))
                .collect::<Vec<_>>();
            let questions = self
                .master()?
                .questions(conversation)
                .await
                .map_err(|e| e.to_string())?;
            let questions=questions.into_iter().map(|question|json!({"id":question.id,"blocking":question.blocking,"answered":question.answer.is_some()})).collect::<Vec<_>>();
            Ok(
                json!({"conversation_jobs":local,"other_jobs":other,"pending_messages":pending,"questions":questions}),
            )
        })
    }
}
pub type SharedActions = Arc<DaemonActions>;

/// Literal client constraints are never inferred from model/tool notices.
fn client_constraints(originals: &[Value]) -> Vec<String> {
    let mut pins = Vec::new();
    for source in originals {
        if let Some(text) = source["text"].as_str() {
            for fragment in text.split_inclusive(['\n', '.', '!', '?']) {
                let lower = fragment.trim_start().to_lowercase();
                if [
                    "must ",
                    "never ",
                    "do not ",
                    "don't ",
                    "no ",
                    "debe ",
                    "sin ",
                    "prohibido ",
                    "siempre ",
                    "keep ",
                    "conserva ",
                    "only ",
                    "solo ",
                    "sólo ",
                ]
                .iter()
                .any(|marker| lower.starts_with(marker))
                    && !pins.iter().any(|pin| pin == fragment)
                {
                    pins.push(fragment.to_owned());
                }
            }
        }
    }
    pins
}

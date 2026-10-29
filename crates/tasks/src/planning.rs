use crate::*;
use excalibur_store::rusqlite::{Connection, params};
use std::collections::BTreeSet;
pub(crate) fn task_changed(
    c: &Connection,
    scope: &str,
    task: &str,
    principal: Option<&str>,
    reason: &str,
) -> Result<()> {
    let value = ops::load(c, "task", scope, task, principal)?;
    if let Some(run) = value["root_run_id"].as_str()
        && !crate::control::terminal(&crate::control::state(c, run)?)
    {
        let id = new_id("dir");
        let revision = value["version"].as_u64().unwrap_or(1);
        let text = format!(
            "{reason}; task {task} revision {revision}. Re-read task_db before continuing."
        );
        c.execute(
            "INSERT INTO task_directives VALUES(?1,?2,?3,?4,?5,'pending',?6)",
            params![
                id,
                task,
                run,
                revision,
                text,
                excalibur_store::unix_seconds()
            ],
        )?;
        crate::control::append(
            c,
            run,
            "directive_received",
            &format!("directive:{id}:received"),
            &json!({"id":id,"text":text}),
        )?;
    }
    Ok(())
}
pub(crate) fn requirement_changed(
    c: &Connection,
    scope: &str,
    req: &str,
    principal: Option<&str>,
) -> Result<()> {
    c.execute("UPDATE evidence SET stale=1 WHERE check_id IN(SELECT k.id FROM checks k JOIN criteria c ON c.id=k.criterion_id WHERE c.requirement_id=?1)",[req])?;
    c.execute(
        "UPDATE criteria SET instruction_revision=instruction_revision+1 WHERE requirement_id=?1",
        [req],
    )?;
    let mut s = c.prepare("SELECT task_id FROM task_requirements WHERE requirement_id=?1")?;
    let tasks = s
        .query_map([req], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for task in tasks {
        task_changed(c, scope, &task, principal, "Requirement changed")?;
        request_replan_tx(
            c,
            &task,
            "requirement changed",
            vec![req.into()],
            &TaskPolicy {
                principal_id: principal.map(str::to_owned),
                ..Default::default()
            },
        )?;
    }
    Ok(())
}
pub(crate) fn snapshot(c: &Connection, plan: &str) -> Result<Value> {
    let (scope, principal): (String, Option<String>) = c.query_row(
        "SELECT COALESCE(scope_id,'general'),principal_id FROM plans WHERE id=?1",
        [plan],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let plan_row = ops::load(c, "plan", &scope, plan, principal.as_deref())?;
    let mut q =
        c.prepare("SELECT id FROM plan_items WHERE plan_id=?1 ORDER BY priority DESC,id")?;
    let ids = q
        .query_map([plan], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let items = ids
        .into_iter()
        .map(|id| ops::load(c, "plan_item", &scope, &id, principal.as_deref()))
        .collect::<Result<Vec<_>>>()?;
    let mut graph = serde_json::Map::new();
    for (entity, key) in [
        ("requirement", "requirements"),
        ("criterion", "criteria"),
        ("check", "checks"),
        ("milestone", "milestones"),
    ] {
        let table = ops::table(entity)?;
        let mut q = c.prepare(&format!(
            "SELECT id FROM {table} WHERE scope_id=?1 AND principal_id IS ?2 ORDER BY id"
        ))?;
        let ids = q
            .query_map(params![scope, principal], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let rows = ids
            .into_iter()
            .map(|id| ops::load(c, entity, &scope, &id, principal.as_deref()))
            .collect::<Result<Vec<_>>>()?;
        graph.insert(key.into(), json!(rows));
    }
    let mut q=c.prepare("SELECT id,from_id,to_id,kind FROM dependencies WHERE scope_id=?1 AND principal_id IS ?2 ORDER BY id")?;
    let dependencies=q.query_map(params![scope,principal],|r|Ok(json!({"id":r.get::<_,String>(0)?,"from":r.get::<_,String>(1)?,"to":r.get::<_,String>(2)?,"kind":r.get::<_,String>(3)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let mut value = json!({"plan_id":plan,"version":plan_row["version"],"plan":plan_row,"items":items,"dependencies":dependencies});
    for (key, value_part) in graph {
        value[key] = value_part;
    }
    Ok(value)
}
pub(crate) fn save_plan(c: &Connection, plan: &str) -> Result<()> {
    let value = snapshot(c, plan)?;
    c.execute(
        "INSERT OR IGNORE INTO task_plan_versions VALUES(?1,?2,?3,?4)",
        params![
            plan,
            value["version"].as_u64().unwrap_or(1),
            value.to_string(),
            excalibur_store::unix_seconds()
        ],
    )?;
    Ok(())
}
pub(crate) fn activate(
    c: &Connection,
    scope: &str,
    op: &Value,
    policy: &TaskPolicy,
) -> Result<Value> {
    let id = ops::resolve(
        c,
        "plan",
        scope,
        ops::text(op, "id")?,
        policy.principal_id.as_deref(),
    )?;
    let plan = ops::load(c, "plan", scope, &id, policy.principal_id.as_deref())?;
    if op["expected_version"].as_u64() != plan["version"].as_u64() {
        return Err(fail(format!("version conflict: current={plan}")));
    }
    if policy.autonomy < 2 {
        return Err(fail(
            "plan activation requires a durable approved proposal at low autonomy",
        ));
    }
    let count: u64 = c.query_row(
        "SELECT count(*) FROM tasks WHERE scope_id=?1 AND principal_id IS ?2 AND level='N3'",
        params![scope, policy.principal_id],
        |r| r.get(0),
    )?;
    if count > 0 {
        return Err(fail(
            "N3 plan activation requires a durable approved proposal",
        ));
    }
    validate_plan(c, scope, &id)?;
    save_plan(c, &id)?;
    c.execute(
        "UPDATE plans SET state='active',version=version+1 WHERE id=?1",
        [&id],
    )?;
    save_plan(c, &id)?;
    Ok(
        json!({"id":id,"entity":"plan","version":plan["version"].as_u64().unwrap()+1,"state":"active"}),
    )
}
fn validate_plan(c: &Connection, scope: &str, plan: &str) -> Result<()> {
    let missing:u64=c.query_row("SELECT count(*) FROM requirements r WHERE r.scope_id=?1 AND r.state='accepted' AND NOT EXISTS(SELECT 1 FROM criteria c WHERE c.requirement_id=r.id)",[scope],|r|r.get(0))?;
    if missing > 0 {
        return Err(fail("plan has accepted requirements without criteria"));
    }
    let invalid:u64=c.query_row("SELECT count(*) FROM criteria WHERE scope_id=?1 AND(method IS NULL OR method NOT IN('command','test','inspection','metric','user_confirmation'))",[scope],|r|r.get(0))?;
    if invalid > 0 {
        return Err(fail("plan criterion has no valid method"));
    }
    let count: u64 = c.query_row(
        "SELECT count(*) FROM plan_items WHERE plan_id=?1",
        [plan],
        |r| r.get(0),
    )?;
    if count == 0 {
        return Err(fail("active plan requires at least one plan item"));
    }
    Ok(())
}
pub(crate) fn apply_inline(
    c: &Connection,
    scope: &str,
    op: &Value,
    policy: &TaskPolicy,
) -> Result<Value> {
    let id = ops::resolve(
        c,
        "plan",
        scope,
        ops::text(op, "id")?,
        policy.principal_id.as_deref(),
    )?;
    let plan = ops::load(c, "plan", scope, &id, policy.principal_id.as_deref())?;
    let expected = op["expected_version"]
        .as_u64()
        .ok_or_else(|| fail("plan mutation requires expected_version"))?;
    if plan["version"].as_u64() != Some(expected) {
        return Err(fail(format!("version conflict: current={plan}")));
    }
    let changes = op["ops"]
        .as_array()
        .ok_or_else(|| fail("plan diff requires ops"))?;
    if changes.iter().any(|op| {
        matches!(
            op["op"].as_str(),
            Some("replan" | "apply_plan" | "activate_plan")
        )
    }) {
        return Err(fail("nested plan mutations are forbidden"));
    }
    let n3: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE scope_id=?1 AND principal_id IS ?2 AND level='N3')",
        params![scope,policy.principal_id],
        |r| r.get(0),
    )?;
    if policy.autonomy < 2 || n3 {
        let approval = op["approval"]
            .as_str()
            .ok_or_else(|| fail("plan requires durable user approval"))?;
        let hash = digest(&serde_json::to_vec(&op["ops"])?);
        let accepted:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM task_plan_approvals a JOIN task_plan_proposals p ON p.id=a.proposal_id WHERE a.proposal_id=?1 AND a.approved=1 AND a.request_hash=?2 AND p.plan_id=?3 AND p.expected_version=?4 AND p.state='proposed')",params![approval,hash,id,expected],|r|r.get(0))?;
        if !accepted {
            return Err(fail(
                "approval does not authorize this exact plan diff/version",
            ));
        }
    }
    let frozen = affected(
        c,
        scope,
        op.get("changed")
            .and_then(Value::as_array)
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        policy.principal_id.as_deref(),
    )?;
    for change in changes {
        if let Some(changed) = change["id"].as_str()
            && frozen.1.contains(&changed.to_owned())
        {
            return Err(fail(format!(
                "completed unaffected node {changed} is frozen"
            )));
        }
    }
    ops::validate(scope, &json!({"ops":changes}), policy)?;
    save_plan(c, &id)?;
    let result = ops::apply_plan_diff(c, scope, &json!({"ops":changes}), policy)?;
    validate_plan(c, scope, &id)?;
    c.execute(
        "UPDATE plans SET version=version+1,state='active' WHERE id=?1",
        [&id],
    )?;
    save_plan(c, &id)?;
    for task in &frozen.0 {
        if ops::load(c, "task", scope, task, policy.principal_id.as_deref()).is_ok() {
            task_changed(
                c,
                scope,
                task,
                policy.principal_id.as_deref(),
                "Plan replanned",
            )?;
        }
    }
    for task in &frozen.0 {
        c.execute("UPDATE task_replan_requests SET state='applied',version=version+1,updated_at=unixepoch() WHERE task_id=?1 AND state='pending'",[task])?;
    }
    if let Some(approval) = op["approval"].as_str() {
        c.execute(
            "UPDATE task_plan_proposals SET state='applied' WHERE id=?1",
            [approval],
        )?;
    }
    Ok(
        json!({"id":id,"entity":"plan","version":expected+1,"affected":frozen.0,"frozen":frozen.1,"changes":result["results"]}),
    )
}
fn affected(
    c: &Connection,
    scope: &str,
    changed: Vec<String>,
    principal: Option<&str>,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut nodes = BTreeSet::new();
    let mut pending = std::collections::VecDeque::from(changed);
    while let Some(id) = pending.pop_front() {
        if !nodes.insert(id.clone()) {
            continue;
        }
        ops::node(c, scope, &id, principal)?;
        let mut q=c.prepare("SELECT to_id FROM dependencies WHERE from_id=?1 AND scope_id=?2 AND principal_id IS ?3 UNION SELECT tr.task_id FROM task_requirements tr JOIN tasks t ON t.id=tr.task_id WHERE tr.requirement_id=?1 AND t.scope_id=?2 AND t.principal_id IS ?3")?;
        pending.extend(
            q.query_map(params![id, scope, principal], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
        let mut parents=c.prepare("SELECT requirement_id FROM criteria WHERE id=?1 AND scope_id=?2 AND principal_id IS ?3 UNION SELECT cr.requirement_id FROM checks k JOIN criteria cr ON cr.id=k.criterion_id WHERE k.id=?1 AND k.scope_id=?2 AND k.principal_id IS ?3 UNION SELECT task_id FROM plan_items WHERE id=?1 AND scope_id=?2 AND principal_id IS ?3 AND task_id IS NOT NULL")?;
        pending.extend(
            parents
                .query_map(params![id, scope, principal], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
    }
    let mut q=c.prepare("SELECT id FROM plan_items WHERE scope_id=?1 AND principal_id IS ?2 AND state='done' UNION SELECT id FROM tasks WHERE scope_id=?1 AND principal_id IS ?2 AND derived_state IN('completed','completed_with_pending')")?;
    let frozen = q
        .query_map(params![scope, principal], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|id| !nodes.contains(id))
        .collect();
    Ok((nodes.into_iter().collect(), frozen))
}

impl TaskStore {
    pub async fn affected_subgraph(&self, scope: &str, changed: Vec<String>) -> Result<Value> {
        self.check_scope(scope)?;
        let scope = scope.to_owned();
        let principal = self.policy.principal_id.clone();
        self.db
            .read(ReadLane::Interactive, move |c| {
                affected(c, &scope, changed, principal.as_deref())
                    .map(|(nodes, frozen)| json!({"affected":nodes,"frozen":frozen}))
                    .map_err(|e| excalibur_store::Error::Configuration(e.to_string()))
            })
            .await
            .map_err(Error::from)
    }
    pub async fn plan_version(&self, scope: &str, plan: &str, version: u64) -> Result<Value> {
        self.check_scope(scope)?;
        let (scope, plan, principal) = (
            scope.to_owned(),
            plan.to_owned(),
            self.policy.principal_id.clone(),
        );
        self.db
            .read(ReadLane::Interactive, move |c| {
                ops::load(c, "plan", &scope, &plan, principal.as_deref())
                    .map_err(|e| excalibur_store::Error::Configuration(e.to_string()))?;
                let text: String = c.query_row(
                    "SELECT snapshot FROM task_plan_versions WHERE plan_id=?1 AND version=?2",
                    params![plan, version],
                    |r| r.get(0),
                )?;
                serde_json::from_str(&text)
                    .map_err(|_| excalibur_store::Error::Invalid("plan history corrupt"))
            })
            .await
            .map_err(Error::from)
    }
    #[allow(clippy::too_many_arguments)] // Explicit scope, optimistic version and immutable operation identity.
    pub async fn propose_replan(
        &self,
        scope: &str,
        plan: &str,
        expected_version: u64,
        key: &str,
        changes: Value,
        changed: Vec<String>,
        reason: &str,
    ) -> Result<Value> {
        self.check_scope(scope)?;
        let _guard = self.mutation_lock.write().await;
        ops::validate(scope, &json!({"ops":changes}), &self.policy)?;
        let changes = self.redactor.redact_json(&changes);
        let (scope, plan, key, reason, policy) = (
            scope.to_owned(),
            plan.to_owned(),
            key.to_owned(),
            self.redactor.redact_text(reason),
            self.policy.clone(),
        );
        self.db.write(Priority::Normal, move |c| {
            // Resolve the requested plan under the current actor/scope before consulting keys.
            let body = ops::load(c, "plan", &scope, &plan, policy.principal_id.as_deref())
                .map_err(|e| excalibur_store::Error::Configuration(e.to_string()))?;
            let hash = digest(changes.to_string().as_bytes());
            let old = c.query_row(
                "SELECT q.id,q.request_hash,q.plan_id,q.expected_version,COALESCE(p.scope_id,'general'),p.principal_id,q.requires_approval,q.affected,q.frozen FROM task_plan_proposals q JOIN plans p ON p.id=q.plan_id WHERE q.operation_key=?1",
                [&key],
                |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,String>(2)?,r.get::<_,u64>(3)?, r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,bool>(6)?,r.get::<_,String>(7)?,r.get::<_,String>(8)?)),
            ).optional()?;
            if let Some((id, stored, old_plan, old_version, old_scope, old_principal, required, affected, frozen)) = old {
                if stored != hash || old_plan != plan || old_version != expected_version
                    || old_scope != scope || old_principal != policy.principal_id {
                    return Err(excalibur_store::Error::Invalid("proposal key reused with different actor, scope, plan, version or diff"));
                }
                let affected:Value=serde_json::from_str(&affected).map_err(|_|excalibur_store::Error::Invalid("stored proposal affected graph is corrupt"))?;
                let frozen:Value=serde_json::from_str(&frozen).map_err(|_|excalibur_store::Error::Invalid("stored proposal frozen graph is corrupt"))?;
                return Ok(json!({"id":id,"request_hash":stored,"requires_approval":required,"affected":affected,"frozen":frozen}));
            }
            if body["version"].as_u64() != Some(expected_version) {
                return Err(excalibur_store::Error::Configuration(format!("version conflict: current={body}")));
            }
            let (affected, frozen) = affected(c,&scope,changed,policy.principal_id.as_deref())
                .map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;
            let required=policy.autonomy<2||c.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE scope_id=?1 AND principal_id IS ?2 AND level='N3')",params![scope,policy.principal_id],|r|r.get::<_,bool>(0))?;
            let id=new_id("prop");
            c.execute("INSERT INTO task_plan_proposals VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,'proposed',?11)",params![id,plan,expected_version,key,changes.to_string(),hash,json!(affected).to_string(),json!(frozen).to_string(),reason,required,excalibur_store::unix_seconds()])?;
            Ok(json!({"id":id,"request_hash":hash,"requires_approval":required,"affected":affected,"frozen":frozen}))
        }).await.map_err(Error::from)
    }
    pub async fn approve_plan(&self, proposal: &str, approved: bool, actor: &str) -> Result<()> {
        if actor.trim().is_empty() {
            return Err(fail("approval requires host actor"));
        }
        let _guard = self.mutation_lock.write().await;
        let (proposal, actor) = (proposal.to_owned(), self.redactor.redact_text(actor));
        let policy = self.policy.clone();
        self.db.write(Priority::Normal,move|c|{let scope:String=c.query_row("SELECT COALESCE(p.scope_id,'general') FROM task_plan_proposals q JOIN plans p ON p.id=q.plan_id WHERE q.id=?1 AND p.principal_id IS ?2",params![proposal,policy.principal_id],|r|r.get(0))?;if policy.allowed_scopes.as_ref().is_some_and(|scopes|!scopes.contains(&scope)){return Err(excalibur_store::Error::Invalid("plan scope is not authorized"));}
if let Some(previous)=c.query_row("SELECT approved FROM task_plan_approvals WHERE proposal_id=?1",[&proposal],|r|r.get::<_,bool>(0)).optional()?{if previous!=approved{return Err(excalibur_store::Error::Invalid("approval already has a different durable answer"));}return Ok(())}let hash:String=c.query_row("SELECT request_hash FROM task_plan_proposals WHERE id=?1 AND state='proposed'",[&proposal],|r|r.get(0))?;c.execute("INSERT INTO task_plan_approvals VALUES(?1,?2,?3,?4,?5) ON CONFLICT(proposal_id) DO NOTHING",params![proposal,approved,actor,hash,excalibur_store::unix_seconds()])?;Ok(())}).await?;
        Ok(())
    }
}
pub(crate) fn attach_macro_item(
    c: &Connection,
    task: &str,
    project: &str,
    principal: Option<&str>,
) -> Result<()> {
    let current: Option<String> =
        c.query_row("SELECT macro_item_id FROM tasks WHERE id=?1", [task], |r| {
            r.get(0)
        })?;
    if current.is_some() {
        return Ok(());
    }
    let plan:Option<String>=c.query_row("SELECT id FROM plans WHERE scope_id=?1 AND kind='macro' AND state<>'archived' AND principal_id IS ?2 ORDER BY version DESC,id LIMIT 1",params![project,principal],|r|r.get(0)).optional()?;
    let plan = if let Some(plan) = plan {
        plan
    } else {
        let plan = new_id("pln");
        c.execute("INSERT INTO plans(id,kind,title,state,scope_id,principal_id) VALUES(?1,'macro','Work plan','draft',?2,?3)",params![plan,project,principal])?;
        plan
    };
    let active: bool = c.query_row(
        "SELECT state='active' FROM plans WHERE id=?1",
        [&plan],
        |r| r.get(0),
    )?;
    if active {
        save_plan(c, &plan)?;
    }
    let title: String = c.query_row("SELECT title FROM tasks WHERE id=?1", [task], |r| r.get(0))?;
    let id = new_id("pli");
    let code = ops::short_code(c, project, "P")?;
    c.execute("INSERT INTO plan_items(id,code,plan_id,task_id,title,scope_id,principal_id) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![id,code,plan,task,title,project,principal])?;
    c.execute(
        "UPDATE tasks SET macro_item_id=?2 WHERE id=?1",
        params![task, id],
    )?;
    if active {
        c.execute("UPDATE plans SET version=version+1 WHERE id=?1", [&plan])?;
        save_plan(c, &plan)?;
    }
    ops::edge(
        c,
        project,
        principal,
        "task",
        task,
        "implements",
        "plan_item",
        &id,
    )?;
    Ok(())
}
pub(crate) fn request_replan_tx(
    c: &Connection,
    task: &str,
    reason: &str,
    changed: Vec<String>,
    policy: &TaskPolicy,
) -> Result<Value> {
    let scope = crate::control::authorize_task(c, task, policy)?;
    let (affected, frozen) = affected(c, &scope, changed.clone(), policy.principal_id.as_deref())?;
    let source:u64=c.query_row("SELECT COALESCE(MAX(j.sequence),0) FROM journal j JOIN tasks t ON t.root_run_id=j.run_id WHERE t.id=?1",[task],|r|r.get(0))?;
    let id = format!(
        "replan-{}",
        digest(format!("{task}\0{reason}\0{}\0{source}", json!(changed)).as_bytes())
    );
    let now = excalibur_store::unix_seconds();
    c.execute("INSERT OR IGNORE INTO task_replan_requests(id,task_id,scope_id,principal_id,reason,changed,affected,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8)",params![id,task,scope,policy.principal_id,reason,json!(changed).to_string(),json!(affected).to_string(),now])?;
    let root: Option<String> =
        c.query_row("SELECT root_run_id FROM tasks WHERE id=?1", [task], |r| {
            r.get(0)
        })?;
    if let Some(run) = root
        && !crate::control::terminal(&crate::control::state(c, &run)?)
    {
        let directive = format!("replan:{id}");
        let text = format!(
            "Replan requested: {reason}. Query task_db replan requests, preserve completed unaffected work, and submit a validated plan diff."
        );
        crate::control::append(
            c,
            &run,
            "directive_received",
            &format!("directive:{directive}:received"),
            &json!({"id":directive,"text":text,"wake":false}),
        )?;
    }
    Ok(
        json!({"id":id,"task_id":task,"reason":reason,"affected":affected,"frozen":frozen,"state":"pending"}),
    )
}
impl TaskStore {
    pub async fn request_replan(
        &self,
        task: &str,
        reason: &str,
        changed: Vec<String>,
    ) -> Result<Value> {
        if reason.trim().is_empty() {
            return Err(fail("replan reason required"));
        }
        let _guard = self.mutation_lock.write().await;
        let (task, reason, policy) = (
            task.to_owned(),
            self.redactor.redact_text(reason),
            self.policy.clone(),
        );
        let value = self
            .db
            .write(Priority::Normal, move |c| {
                request_replan_tx(c, &task, &reason, changed, &policy)
                    .map_err(|e| excalibur_store::Error::Configuration(e.to_string()))
            })
            .await?;
        Ok(self.redactor.redact_json(&value))
    }
    pub async fn pending_replans(&self, task: &str) -> Result<Vec<Value>> {
        let (task, policy) = (task.to_owned(), self.policy.clone());
        self.db.read(ReadLane::Interactive,move|c|{crate::control::authorize_task(c,&task,&policy).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;let mut q=c.prepare("SELECT id,reason,affected FROM task_replan_requests WHERE task_id=?1 AND state='pending' ORDER BY created_at,id LIMIT 20")?;Ok(q.query_map([task],|r|Ok(json!({"id":r.get::<_,String>(0)?,"reason":r.get::<_,String>(1)?,"affected":serde_json::from_str::<Value>(&r.get::<_,String>(2)?).unwrap_or(Value::Null)})))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await.map_err(Error::from)
    }
}
pub(crate) fn dependency_changed(
    c: &Connection,
    task: &str,
    policy: &TaskPolicy,
    reason: &str,
) -> Result<()> {
    let scope = crate::control::authorize_task(c, task, policy)?;
    let mut q=c.prepare("SELECT DISTINCT t.id FROM dependencies d JOIN tasks t ON t.id=d.to_id WHERE d.from_id=?1 AND d.scope_id=?2 AND t.principal_id IS ?3 AND t.scope_id=?2")?;
    let tasks = q
        .query_map(params![task, scope, policy.principal_id], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for dependent in tasks {
        request_replan_tx(c, &dependent, reason, vec![task.into()], policy)?;
    }
    Ok(())
}
impl TaskStore {
    /// Planning remains available while effectful tools await macro approval.
    pub async fn effect_admission(&self, run: &str) -> Result<()> {
        let Some(task) = self.task_for_run(run).await? else {
            return Ok(());
        };
        let required = task.level == WorkLevel::N3
            || (self.policy.autonomy < 2
                && (task.level != WorkLevel::N1
                    || task.macro_item_id.is_some()
                    || task.project_id.is_some()));
        if !required {
            return Ok(());
        }
        let (task_id, scope, principal) =
            (task.id, task.scope_id, self.policy.principal_id.clone());
        let approved=self.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM tasks t JOIN plan_items i ON i.id=t.macro_item_id JOIN plans p ON p.id=i.plan_id JOIN task_plan_proposals q ON q.plan_id=p.id JOIN task_plan_approvals a ON a.proposal_id=q.id WHERE t.id=?1 AND t.scope_id=?2 AND t.principal_id IS ?3 AND i.task_id=t.id AND i.principal_id IS t.principal_id AND p.principal_id IS t.principal_id AND p.kind='macro' AND p.state='active' AND q.state='applied' AND a.approved=1 AND q.expected_version+1=p.version AND a.request_hash=q.request_hash)",params![task_id,scope,principal],|r|r.get::<_,bool>(0))?)).await?;
        if approved {
            Ok(())
        } else {
            Err(fail(
                "required macro plan has no current durable user approval; propose a plan before file/process effects",
            ))
        }
    }
}
pub(crate) fn proposal_tx(
    c: &Connection,
    scope: &str,
    op: &Value,
    policy: &TaskPolicy,
) -> Result<Value> {
    let plan = ops::resolve(
        c,
        "plan",
        scope,
        ops::text(op, "id")?,
        policy.principal_id.as_deref(),
    )?;
    let current = ops::load(c, "plan", scope, &plan, policy.principal_id.as_deref())?;
    let version = op["expected_version"]
        .as_u64()
        .ok_or_else(|| fail("proposal requires expected_version"))?;
    if current["version"].as_u64() != Some(version) {
        return Err(fail(format!("version conflict: current={current}")));
    }
    let changes = op["ops"].clone();
    ops::validate(scope, &json!({"ops":changes}), policy)?;
    let changed = op["changed"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let (nodes, frozen) = affected(c, scope, changed, policy.principal_id.as_deref())?;
    let id = new_id("prop");
    let key = new_id("proposal-op");
    let hash = digest(changes.to_string().as_bytes());
    let required=policy.autonomy<2||c.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE scope_id=?1 AND principal_id IS ?2 AND level='N3')",params![scope,policy.principal_id],|r|r.get::<_,bool>(0))?;
    c.execute(
        "INSERT INTO task_plan_proposals VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,'proposed',?11)",
        params![
            id,
            plan,
            version,
            key,
            changes.to_string(),
            hash,
            json!(nodes).to_string(),
            json!(frozen).to_string(),
            op["reason"].as_str().unwrap_or("plan proposal"),
            required,
            excalibur_store::unix_seconds()
        ],
    )?;
    Ok(
        json!({"id":id,"entity":"plan_proposal","request_hash":hash,"requires_approval":required,"affected":nodes,"frozen":frozen}),
    )
}

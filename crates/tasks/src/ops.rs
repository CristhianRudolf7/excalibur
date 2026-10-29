use crate::*;
use excalibur_store::rusqlite::{
    Connection, params, params_from_iter,
    types::{Value as SqlValue, ValueRef},
};
use std::collections::BTreeMap;
pub(crate) fn table(entity: &str) -> Result<&'static str> {
    Ok(match entity.trim_end_matches('s') {
        "project" => "projects",
        "task" => "tasks",
        "plan" => "plans",
        "plan_item" => "plan_items",
        "requirement" => "requirements",
        "criterion" | "criteria" => "criteria",
        "check" => "checks",
        "evidence" => "evidence",
        "milestone" => "milestones",
        "assumption" => "assumptions",
        "decision" => "decisions",
        "project_log" | "log" => "project_log",
        "notebook" | "note" => "task_notebook",
        "replan_request" => "task_replan_requests",
        _ => return Err(fail(format!("unknown entity {entity}"))),
    })
}
fn entity_name(entity: &str) -> Result<&'static str> {
    Ok(match table(entity)? {
        "projects" => "project",
        "tasks" => "task",
        "plans" => "plan",
        "plan_items" => "plan_item",
        "requirements" => "requirement",
        "criteria" => "criterion",
        "checks" => "check",
        "evidence" => "evidence",
        "milestones" => "milestone",
        "assumptions" => "assumption",
        "decisions" => "decision",
        "project_log" => "project_log",
        "task_replan_requests" => "replan_request",
        _ => "notebook",
    })
}
pub(crate) fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 65536 && !s.contains('\0'))
        .ok_or_else(|| fail(format!("{key} must be a nonempty string")))
}
fn json_fields() -> &'static [&'static str] {
    &[
        "metadata",
        "request_card",
        "expected",
        "budget",
        "roots",
        "settings",
        "skill_groups",
        "criteria",
        "definition",
        "fingerprints",
        "alternatives",
        "linked_requirements",
        "refs",
        "impact_area",
    ]
}
fn encode(key: &str, value: &Value) -> SqlValue {
    if json_fields().contains(&key) {
        return SqlValue::Text(value.to_string());
    }
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(v) => SqlValue::Integer(i64::from(*v)),
        Value::Number(n) => n
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| n.as_f64().map(SqlValue::Real))
            .unwrap_or(SqlValue::Null),
        Value::String(v) => SqlValue::Text(v.clone()),
        _ => SqlValue::Text(value.to_string()),
    }
}
fn columns(c: &Connection, table: &str) -> Result<Vec<String>> {
    let mut s = c.prepare(&format!("PRAGMA table_info({table})"))?;
    Ok(s.query_map([], |r| r.get(1))?
        .collect::<std::result::Result<_, _>>()?)
}
fn row(r: &rusqlite::Row<'_>, columns: &[String]) -> rusqlite::Result<Value> {
    let mut map = serde_json::Map::new();
    for (i, key) in columns.iter().enumerate() {
        let value = match r.get_ref(i)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => json!(n),
            ValueRef::Real(n) => json!(n),
            ValueRef::Text(t) => {
                let text = String::from_utf8_lossy(t).to_string();
                if json_fields().contains(&key.as_str()) {
                    serde_json::from_str(&text).unwrap_or(json!(text))
                } else {
                    json!(text)
                }
            }
            ValueRef::Blob(_) => Value::Null,
        };
        map.insert(key.clone(), value);
    }
    Ok(Value::Object(map))
}
pub(crate) fn load(
    c: &Connection,
    entity: &str,
    scope: &str,
    id: &str,
    principal: Option<&str>,
) -> Result<Value> {
    let table = table(entity)?;
    let fields = columns(c, table)?;
    let mut value=c.query_row(&format!("SELECT * FROM {table} WHERE id=?1 AND COALESCE(scope_id,'general')=?2 AND principal_id IS ?3"),params![id,scope,principal],|r|row(r,&fields)).optional()?.ok_or_else(||fail(format!("missing {entity} reference {id} in scope {scope}")))?;
    if entity == "task"
        && let Some(run) = value["root_run_id"].as_str()
    {
        let state = crate::control::state(c, run)?;
        value["derived_state"] = json!(if matches!(
            state.as_str(),
            "ready" | "running" | "waiting" | "verifying" | "recovering"
        ) {
            "in_progress"
        } else if matches!(state.as_str(), "created" | "pending") {
            "open"
        } else {
            &state
        });
    }
    Ok(value)
}
pub(crate) fn save_version(
    c: &Connection,
    entity: &str,
    scope: &str,
    id: &str,
    principal: Option<&str>,
) -> Result<Value> {
    let value = load(c, entity, scope, id, principal)?;
    let version = value["version"].as_u64().unwrap_or(1);
    c.execute(
        "INSERT OR IGNORE INTO task_entity_versions VALUES(?1,?2,?3,?4,?5)",
        params![
            entity_name(entity)?,
            id,
            version,
            value.to_string(),
            excalibur_store::unix_seconds()
        ],
    )?;
    Ok(value)
}
pub(crate) fn short_code(c: &Connection, scope: &str, kind: &str) -> Result<String> {
    let next = c
        .query_row(
            "SELECT next_value FROM short_codes WHERE project_id=?1 AND kind=?2",
            params![scope, kind],
            |r| r.get::<_, u64>(0),
        )
        .optional()?
        .unwrap_or(1);
    c.execute("INSERT INTO short_codes VALUES(?1,?2,?3) ON CONFLICT(project_id,kind) DO UPDATE SET next_value=excluded.next_value",params![scope,kind,next+1])?;
    Ok(format!("{kind}{next}"))
}
pub(crate) fn resolve(
    c: &Connection,
    entity: &str,
    scope: &str,
    id: &str,
    principal: Option<&str>,
) -> Result<String> {
    let t = table(entity)?;
    if columns(c, t)?.iter().any(|v| v == "code") {
        let found=c.query_row(&format!("SELECT id FROM {t} WHERE (id=?1 OR code=?1) AND COALESCE(scope_id,'general')=?2 AND principal_id IS ?3"),params![id,scope,principal],|r|r.get::<_,String>(0)).optional()?;
        found.ok_or_else(|| fail(format!("missing {entity} reference {id} in scope {scope}")))
    } else {
        load(c, entity, scope, id, principal)?;
        Ok(id.into())
    }
}
pub(crate) fn validate(scope: &str, args: &Value, policy: &TaskPolicy) -> Result<()> {
    if scope.is_empty() || scope.len() > 256 {
        return Err(fail("invalid scope"));
    }
    if policy
        .allowed_scopes
        .as_ref()
        .is_some_and(|s| !s.contains(scope))
    {
        return Err(fail("task scope is not authorized"));
    }
    let ops = args["ops"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 64)
        .ok_or_else(|| fail("ops must contain 1..64 operations"))?;
    if args
        .as_object()
        .is_none_or(|m| m.keys().any(|k| k != "ops"))
    {
        return Err(fail("batch accepts only ops"));
    }
    for op in ops {
        if !op.is_object() {
            return Err(fail("operation must be an object"));
        }
        let name = text(op, "op")?;
        if !policy.trusted_host && reserved_metadata(op) {
            return Err(fail("reserved host metadata is not writable by task_db"));
        }
        if op.get("scope").is_some_and(|s| s != scope) {
            return Err(fail("operation targets another scope"));
        }
        if let Some(data) = op.get("data")
            && (!data.is_object() || data.to_string().len() > 65536)
        {
            return Err(fail("data must be an object up to 65536 bytes"));
        }
        if name == "query" {
            if op["limit"].as_u64().is_some_and(|n| n == 0 || n > 20) {
                return Err(fail("query limit must be 1..20"));
            }
            if let Some(fields) = op.get("fields")
                && fields
                    .as_array()
                    .is_none_or(|v| v.len() > 30 || v.iter().any(|f| !f.is_string()))
            {
                return Err(fail("fields must contain at most 30 names"));
            }
        } else if !matches!(
            name,
            "create_record"
                | "update_record"
                | "add_dependency"
                | "remove_dependency"
                | "link_requirement"
                | "unlink_requirement"
                | "add_criterion"
                | "add_check"
                | "record_evidence"
                | "activate_plan"
                | "apply_plan"
                | "replan"
                | "propose_plan"
                | "propose_replan"
        ) && !name.starts_with("create_")
            && !name.starts_with("update_")
            && !name.starts_with("archive_")
        {
            return Err(fail(format!("unknown task operation {name}")));
        }
        let entity = op["entity"]
            .as_str()
            .or_else(|| name.strip_prefix("create_"))
            .or_else(|| name.strip_prefix("update_"))
            .or_else(|| name.strip_prefix("archive_"));
        if !policy.allow_projects
            && (entity.is_some_and(|e| e == "project" || e == "projects")
                || name.ends_with("project"))
        {
            return Err(fail("projects are disabled in this task policy"));
        }
        if name == "record_evidence" {
            return Err(fail(
                "trusted evidence must be recorded by the backend check runner; model-authored evidence cannot satisfy criteria",
            ));
        }
        if let Some(id) = op.get("id")
            && id
                .as_str()
                .is_none_or(|s| s.is_empty() || s.len() > 128 || s.chars().any(char::is_control))
        {
            return Err(fail("id must contain 1..128 printable bytes"));
        }
    }
    Ok(())
}
pub(crate) fn apply(
    c: &Connection,
    scope: &str,
    args: &Value,
    policy: &TaskPolicy,
) -> Result<Value> {
    apply_mode(c, scope, args, policy, false)
}
pub(crate) fn apply_plan_diff(
    c: &Connection,
    scope: &str,
    args: &Value,
    policy: &TaskPolicy,
) -> Result<Value> {
    apply_mode(c, scope, args, policy, true)
}
fn apply_mode(
    c: &Connection,
    scope: &str,
    args: &Value,
    policy: &TaskPolicy,
    plan_diff: bool,
) -> Result<Value> {
    let mut results = Vec::new();

    for (index, op) in args["ops"]
        .as_array()
        .ok_or_else(|| fail("ops required"))?
        .iter()
        .enumerate()
    {
        let result = apply_one(c, scope, op, policy, plan_diff)
            .map_err(|e| fail(format!("operation {index}: {e}")))?;
        results.push(result);
    }
    let missing:u64=c.query_row("SELECT count(*) FROM requirements r WHERE r.scope_id=?1 AND r.principal_id IS ?2 AND r.state='accepted' AND NOT EXISTS(SELECT 1 FROM criteria cr WHERE cr.requirement_id=r.id)",params![scope,policy.principal_id],|r|r.get(0))?;
    if missing > 0 {
        return Err(fail("accepted requirement requires at least one criterion"));
    }
    Ok(json!({"scope":scope,"results":results}))
}
fn apply_one(
    c: &Connection,
    scope: &str,
    op: &Value,
    policy: &TaskPolicy,
    plan_diff: bool,
) -> Result<Value> {
    let name = text(op, "op")?;
    let principal = policy.principal_id.as_deref();
    match name {
        "record_evidence" => {
            let check = resolve(c, "check", scope, text(op, "check")?, principal)?;
            let call = op
                .get("tool_call")
                .or_else(|| op.get("tool_call_id"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    fail("record_evidence requires a backend-produced tool call reference")
                })?;
            let found:Option<(String,String)>=c.query_row("SELECT id,result FROM evidence WHERE check_id=?1 AND tool_call_id=?2 AND trusted=1 AND scope_id=?3 AND principal_id IS ?4 ORDER BY rowid DESC LIMIT 1",params![check,call,scope,principal],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let Some((id, result)) = found else {
                return Err(fail(
                    "no trusted backend evidence matches this check and tool call",
                ));
            };
            if op.get("result").is_some_and(|r| r != &result) {
                return Err(fail("record_evidence result differs from backend evidence"));
            }
            return Ok(json!({"id":id,"entity":"evidence","trusted":true,"existing":true}));
        }
        "query" => return query(c, scope, op, policy),
        "create_record" | "update_record" => return draft(c, scope, op, policy),
        "add_dependency" => {
            let from = text(op, "from")?;
            let to = text(op, "to")?;
            node(c, scope, from, principal)?;
            node(c, scope, to, principal)?;
            let kind = op["kind"].as_str().unwrap_or("blocks");
            if !matches!(kind, "blocks" | "informs") {
                return Err(fail("dependency kind must be blocks or informs"));
            }
            if from == to || reachable(c, scope, to, from)? {
                return Err(fail(format!(
                    "dependency {from} -> {to} would create a cycle"
                )));
            }
            let id = op["id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| new_id("dep"));
            c.execute("INSERT INTO dependencies(id,from_id,to_id,kind,scope_id,principal_id) VALUES(?1,?2,?3,?4,?5,?6)",params![id,from,to,kind,scope,principal])?;
            return Ok(json!({"id":id,"entity":"dependency"}));
        }
        "remove_dependency" => {
            let id = text(op, "id")?;
            let n = c.execute(
                "DELETE FROM dependencies WHERE id=?1 AND scope_id=?2 AND principal_id IS ?3",
                params![id, scope, principal],
            )?;
            if n == 0 {
                return Err(fail("dependency does not exist in scope"));
            }
            return Ok(json!({"id":id,"removed":true}));
        }
        "link_requirement" | "unlink_requirement" => {
            let task = resolve(c, "task", scope, text(op, "task")?, principal)?;
            let req = resolve(c, "requirement", scope, text(op, "requirement")?, principal)?;
            if name == "link_requirement" {
                c.execute(
                    "INSERT OR IGNORE INTO task_requirements VALUES(?1,?2)",
                    params![task, req],
                )?;
                edge(
                    c,
                    scope,
                    principal,
                    "task",
                    &task,
                    "requires",
                    "requirement",
                    &req,
                )?;
            } else {
                c.execute(
                    "DELETE FROM task_requirements WHERE task_id=?1 AND requirement_id=?2",
                    params![task, req],
                )?;
            }
            c.execute(
                "UPDATE tasks SET version=version+1,updated_at=unixepoch() WHERE id=?1",
                [&task],
            )?;
            crate::planning::task_changed(c, scope, &task, principal, "Task requirements changed")?;
            return Ok(json!({"task":task,"requirement":req}));
        }
        "apply_plan" | "replan" => return crate::planning::apply_inline(c, scope, op, policy),
        "propose_plan" | "propose_replan" => {
            return crate::planning::proposal_tx(c, scope, op, policy);
        }
        "activate_plan" => return crate::planning::activate(c, scope, op, policy),
        _ => {}
    }
    let (create, entity) = if name == "add_criterion" {
        (true, "criterion")
    } else if name == "add_check" {
        (true, "check")
    } else if let Some(e) = name.strip_prefix("create_") {
        (true, entity_name(e)?)
    } else if let Some(e) = name.strip_prefix("update_") {
        (false, entity_name(e)?)
    } else if let Some(e) = name.strip_prefix("archive_") {
        let mut patch = op.clone();
        patch["op"] = json!(format!("update_{}", entity_name(e)?));
        patch["data"]["state"] = json!("archived");
        return apply_one(c, scope, &patch, policy, plan_diff);
    } else {
        return Err(fail(format!("unsupported operation {name}")));
    };
    if matches!(entity, "evidence" | "replan_request") {
        return Err(fail("entity is backend-only"));
    }
    let table = table(entity)?;
    let fields = columns(c, table)?;
    let mut data = op.get("data").cloned().unwrap_or(json!({}));
    for (key, value) in op.as_object().unwrap() {
        if !matches!(
            key.as_str(),
            "op" | "scope" | "id" | "expected_version" | "data"
        ) {
            data[key] = value.clone();
        }
    }
    for field in [
        "id",
        "version",
        "principal_id",
        "scope_id",
        "created_at",
        "updated_at",
        "root_run_id",
        "macro_item_id",
        "derived_state",
        "trusted",
        "forced_level",
        "stale",
        "code",
    ] {
        if data.get(field).is_some() {
            return Err(fail(format!("{field} is backend managed")));
        }
    }
    let id = if create {
        op["id"].as_str().map(str::to_owned).unwrap_or_else(|| {
            new_id(match entity {
                "task" => "tsk",
                "requirement" => "req",
                "criterion" => "crt",
                "check" => "chk",
                "project" => "prj",
                "plan" => "pln",
                "plan_item" => "pli",
                "milestone" => "mil",
                "assumption" => "asm",
                "decision" => "dec",
                _ => "rec",
            })
        })
    } else {
        resolve(c, entity, scope, text(op, "id")?, principal)?
    };
    if data.get("metadata").is_some_and(|v| !v.is_object()) {
        return Err(fail("metadata must be an object"));
    }
    let mut old = if create {
        json!({})
    } else {
        load(c, entity, scope, &id, principal)?
    };
    if !create {
        let expected = op["expected_version"]
            .as_u64()
            .ok_or_else(|| fail("update requires expected_version"))?;
        if old["version"].as_u64() != Some(expected) {
            return Err(fail(format!("version conflict: current={old}")));
        }
    }
    if entity == "task" {
        if data.get("state").is_some() || data.get("status").is_some() {
            return Err(fail("task state is derived from its root execution"));
        }
        if create {
            data["title"] = json!(text(&data, "title")?);
            data["level"] = data.get("level").cloned().unwrap_or(json!("N1"));
            data["request_card"] = data.get("request_card").cloned().unwrap_or(json!({}));
            data["budget"] = data.get("budget").cloned().unwrap_or(json!({}));
        }
        if !create && data.get("request_card").is_some() && !policy.trusted_host {
            return Err(fail(
                "original request_card is immutable to task_db; use a host instruction revision",
            ));
        }
        if !create
            && let Some(level) = data.get("level").and_then(Value::as_str)
            && level < old["level"].as_str().unwrap_or("N1")
        {
            return Err(fail(
                "task_db cannot lower task level; only explicit host promotion override can",
            ));
        }
        if let Some(level) = data.get("level")
            && !matches!(level.as_str(), Some("N1" | "N2" | "N3"))
        {
            return Err(fail("task level must be N1, N2 or N3"));
        }
    }
    if entity == "criterion" {
        if create {
            let req = resolve(
                c,
                "requirement",
                scope,
                data.get("requirement_id")
                    .or_else(|| data.get("requirement"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail("criterion requires requirement"))?,
                principal,
            )?;
            data["requirement_id"] = json!(req);
            data.as_object_mut().unwrap().remove("requirement");
            text(&data, "statement")?;
        }
        if let Some(method) = data.get("method") {
            if !matches!(
                method.as_str(),
                Some("command" | "test" | "inspection" | "metric" | "user_confirmation")
            ) {
                return Err(fail("criterion method invalid"));
            }
        } else if create {
            return Err(fail("criterion requires method"));
        }
        if let Some(area) = data.get("impact_area") {
            if area
                .as_array()
                .is_none_or(|a| a.iter().any(|s| !s.is_string()))
            {
                return Err(fail("impact_area must be an array of patterns"));
            }
            for pattern in area.as_array().unwrap() {
                globset::Glob::new(pattern.as_str().unwrap())
                    .map_err(|_| fail("invalid impact pattern"))?;
            }
        }
    }
    if entity == "check" && create {
        let criterion = resolve(
            c,
            "criterion",
            scope,
            data.get("criterion_id")
                .or_else(|| data.get("criterion"))
                .and_then(Value::as_str)
                .ok_or_else(|| fail("check requires criterion"))?,
            principal,
        )?;
        data["criterion_id"] = json!(criterion);
        data.as_object_mut().unwrap().remove("criterion");
        if !data["definition"].is_object() {
            return Err(fail("check requires executable definition"));
        }
    }
    if entity == "milestone" {
        if data
            .get("state")
            .is_some_and(|s| matches!(s.as_str(), Some("done" | "completed")))
        {
            return Err(fail(
                "milestone closure requires fresh backend verification; use close_milestone",
            ));
        }
        if let Some(ids) = data["criteria"].as_array() {
            for criterion in ids {
                resolve(
                    c,
                    "criterion",
                    scope,
                    criterion
                        .as_str()
                        .ok_or_else(|| fail("milestone criteria require ids"))?,
                    principal,
                )?;
            }
        } else if create {
            return Err(fail("milestone requires criteria array"));
        }
    }
    if matches!(entity, "assumption" | "notebook") && create {
        text(&data, "text")?;
    }
    if entity == "decision" && create {
        text(&data, "statement")?;
        text(&data, "reason")?;
        if !data["alternatives"].is_array() {
            return Err(fail("decision requires considered alternatives array"));
        }
    }
    if entity == "check" && data.get("definition").is_some() {
        serde_json::from_value::<crate::CheckDefinition>(data["definition"].clone())
            .map_err(|_| fail("unsupported executable check definition"))?;
    }
    if entity == "requirement" {
        if create {
            text(&data, "text")?;
            data["state"] = data.get("state").cloned().unwrap_or(json!("proposed"));
        }
        if data
            .get("state")
            .is_some_and(|s| !matches!(s.as_str(), Some("proposed" | "accepted" | "retired")))
        {
            return Err(fail("requirement state invalid"));
        }
    }
    if entity == "plan_item" {
        let plan = data
            .get("plan_id")
            .or_else(|| old.get("plan_id"))
            .and_then(Value::as_str);
        if let Some(plan) = plan {
            let active: bool = c.query_row(
                "SELECT state='active' FROM plans WHERE id=?1",
                [plan],
                |r| r.get(0),
            )?;
            if active
                && !policy.trusted_host
                && !plan_diff
                && (create || data.as_object().unwrap().keys().any(|k| k != "state"))
            {
                return Err(fail(
                    "active plan items require an atomic versioned apply_plan diff",
                ));
            }
        }
    }
    if entity == "plan_item" {
        if create {
            let plan = resolve(c, "plan", scope, text(&data, "plan_id")?, principal)?;
            data["plan_id"] = json!(plan);
            text(&data, "title")?;
        }
        if data.get("state").is_some_and(|s| {
            !matches!(
                s.as_str(),
                Some("pending" | "in_progress" | "blocked" | "done" | "cancelled")
            )
        }) {
            return Err(fail("plan item state invalid"));
        }
    }
    if entity == "plan" && create {
        if !matches!(data["kind"].as_str(), Some("macro" | "micro")) {
            return Err(fail("plan kind must be macro or micro"));
        }
        if data["kind"] == "micro" {
            let owner = resolve(c, "task", scope, text(&data, "owner_task_id")?, principal)?;
            data["owner_task_id"] = json!(owner);
        }
    }
    if entity == "project" && create {
        data["name"] = data
            .get("name")
            .or_else(|| data.get("title"))
            .cloned()
            .ok_or_else(|| fail("project requires name"))?;
    }
    // Validate all ordinary references against this actor and scope before SQL's FK check.
    for (field, target) in [
        ("origin_message_id", "message"),
        ("macro_item_id", "plan_item"),
        ("plan_id", "plan"),
        ("owner_task_id", "task"),
        ("task_id", "task"),
    ] {
        if let Some(reference) = data[field].as_str()
            && target != "message"
        {
            resolve(c, target, scope, reference, principal)?;
        }
    }
    for (field, target) in [
        ("requirement_id", "requirement"),
        ("criterion_id", "criterion"),
        ("plan_id", "plan"),
        ("task_id", "task"),
        ("owner_task_id", "task"),
        ("macro_item_id", "plan_item"),
    ] {
        if let Some(reference) = data[field].as_str() {
            let resolved = resolve(c, target, scope, reference, principal)?;
            data[field] = json!(resolved);
        }
    }
    for field in match entity {
        "task" => vec!["title"],
        "criterion" => vec!["statement"],
        "requirement" | "assumption" | "notebook" | "project_log" => vec!["text"],
        "decision" => vec!["statement", "reason"],
        _ => Vec::new(),
    } {
        if data.get(field).is_some() {
            text(&data, field)?;
        }
    }
    if let Some(project) = data["project_id"].as_str() {
        let project_scope: Option<String> = c
            .query_row(
                "SELECT COALESCE(scope_id,id) FROM projects WHERE id=?1 AND principal_id IS ?2",
                params![project, principal],
                |r| r.get(0),
            )
            .optional()?;
        let project_scope =
            project_scope.ok_or_else(|| fail("project reference is not authorized"))?;
        if policy
            .allowed_scopes
            .as_ref()
            .is_some_and(|s| !s.contains(&project_scope))
        {
            return Err(fail("project reference targets another authorized scope"));
        }
    }
    if entity == "plan"
        && matches!(data["state"].as_str(), Some("active" | "approved"))
        && !policy.trusted_host
    {
        return Err(fail(
            "activate a plan through validated activate_plan or approved apply_plan",
        ));
    }
    if entity == "milestone" && create {
        resolve(c, "plan", scope, text(&data, "plan_id")?, principal)?;
        if data["criteria"].as_array().is_none_or(Vec::is_empty) {
            return Err(fail("milestone requires nonempty closure criteria"));
        }
    }
    let mut values = BTreeMap::<String, Value>::new();
    let mut metadata = old.get("metadata").cloned().unwrap_or(json!({}));
    if !policy.trusted_host
        && old.pointer("/metadata/independent_verification") == Some(&Value::Bool(true))
        && data
            .pointer("/metadata/independent_verification")
            .is_some_and(|v| v != &Value::Bool(true))
    {
        return Err(fail(
            "task_db cannot revoke required independent verification",
        ));
    }
    if let Some(patch) = data.get("metadata").and_then(Value::as_object) {
        for (key, value) in patch {
            metadata[key] = value.clone();
        }
    }
    if !create
        && entity == "criterion"
        && !policy.trusted_host
        && [
            "statement",
            "method",
            "expected",
            "requirement_id",
            "impact_area",
        ]
        .iter()
        .any(|k| data.get(*k).is_some_and(|v| old.get(*k) != Some(v)))
    {
        metadata
            .as_object_mut()
            .unwrap()
            .remove("implicit_final_response");
    }
    for (key, value) in data.as_object().unwrap() {
        if fields.contains(key) {
            values.insert(key.clone(), value.clone());
        } else if key != "requirements" {
            metadata[key] = value.clone();
        }
    }
    if fields.contains(&"metadata".to_owned()) {
        values.insert("metadata".into(), metadata);
    }
    let now = excalibur_store::unix_seconds();
    values.insert("updated_at".into(), json!(now));
    if create {
        values.insert("id".into(), json!(id));
        values.insert("scope_id".into(), json!(scope));
        values.insert("principal_id".into(), json!(principal));
        values.insert("created_at".into(), json!(now));
        if entity == "task" {
            values.insert("derived_state".into(), json!("open"));
        }
        if matches!(entity, "task" | "requirement" | "milestone") {
            let prefix = match entity {
                "task" => "T",
                "requirement" => "R",
                _ => "M",
            };
            values.insert("code".into(), json!(short_code(c, scope, prefix)?));
            if scope.starts_with("prj_") {
                load(c, "project", scope, scope, principal)?;
                values.insert("project_id".into(), json!(scope));
            }
        }
        if entity == "criterion" {
            let req = values["requirement_id"].as_str().unwrap();
            let code: String = c.query_row(
                "SELECT COALESCE(code,id) FROM requirements WHERE id=?1",
                [req],
                |r| r.get(0),
            )?;
            let count: u64 = c.query_row(
                "SELECT count(*)+1 FROM criteria WHERE requirement_id=?1",
                [req],
                |r| r.get(0),
            )?;
            values.insert(
                "code".into(),
                json!(format!("C{}.{count}", code.trim_start_matches('R'))),
            );
        }
        if entity == "plan_item" {
            values.insert("code".into(), json!(short_code(c, scope, "P")?));
        }
        insert(c, table, &values)?;
    } else {
        values.insert(
            "version".into(),
            json!(old["version"].as_u64().unwrap_or(1) + 1),
        );
        update(c, table, &id, &values)?;
    }
    if entity == "plan_item" && !plan_diff {
        let plan = values
            .get("plan_id")
            .or_else(|| old.get("plan_id"))
            .and_then(Value::as_str)
            .unwrap();
        let active: bool = c.query_row(
            "SELECT state='active' FROM plans WHERE id=?1",
            [plan],
            |r| r.get(0),
        )?;
        if active && (create || data.as_object().unwrap().keys().any(|k| k != "state")) {
            // The old active snapshot was persisted on activation; never reuse its approval.
            c.execute("UPDATE plans SET version=version+1 WHERE id=?1", [plan])?;
            crate::planning::save_plan(c, plan)?;
        }
    }
    let actual_scope = scope;
    if entity == "project" && create {
        c.execute("UPDATE projects SET scope_id=?1 WHERE id=?1", [&id])?;
        let plan = new_id("pln");
        c.execute("INSERT INTO plans(id,kind,title,state,scope_id,principal_id) VALUES(?1,'macro',?2,'draft',?3,?4)",params![plan,data["name"].as_str().unwrap_or("Project plan"),id,principal])?;
        let value = save_version(c, "project", &id, &id, principal)?;
        save_version(c, "plan", &id, &plan, principal)?;
        return Ok(
            json!({"id":id,"entity":"project","version":1,"macro_plan_id":plan,"data":value}),
        );
    }
    if entity == "task" {
        if (create || data.get("level").is_some())
            && (values.get("project_id").and_then(Value::as_str).is_some()
                || matches!(
                    values.get("level").and_then(Value::as_str),
                    Some("N2" | "N3")
                ))
        {
            crate::planning::attach_macro_item(
                c,
                &id,
                values
                    .get("project_id")
                    .and_then(Value::as_str)
                    .or_else(|| old["project_id"].as_str())
                    .unwrap_or(scope),
                principal,
            )?;
        }
        if let Some(requirements) = data.get("requirements").and_then(Value::as_array) {
            for req in requirements {
                let req = resolve(
                    c,
                    "requirement",
                    scope,
                    req.as_str()
                        .ok_or_else(|| fail("requirements must contain ids"))?,
                    principal,
                )?;
                c.execute(
                    "INSERT OR IGNORE INTO task_requirements VALUES(?1,?2)",
                    params![id, req],
                )?;
                edge(
                    c,
                    scope,
                    principal,
                    "task",
                    &id,
                    "requires",
                    "requirement",
                    &req,
                )?;
            }
        }
        if !create {
            crate::planning::task_changed(c, scope, &id, principal, "Task updated")?;
        }
    }
    if entity == "criterion" {
        c.execute("DELETE FROM task_impact_index WHERE criterion_id=?1", [&id])?;
        c.execute("DELETE FROM task_impact_roots WHERE criterion_id=?1", [&id])?;
        let patterns = values
            .get("impact_area")
            .or_else(|| old.get("impact_area"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| vec![json!("**")]);
        let patterns = if patterns.is_empty() {
            vec![json!("**")]
        } else {
            patterns
        };
        for pattern in patterns {
            let pattern = pattern
                .as_str()
                .ok_or_else(|| fail("invalid impact pattern"))?;
            c.execute(
                "INSERT OR IGNORE INTO task_impact_index VALUES(?1,?2)",
                params![id, pattern],
            )?;
            let literal = pattern.split(['*', '?', '[', '{']).next().unwrap_or("");
            let prefix = if literal == pattern {
                literal
            } else {
                literal.rsplit_once('/').map_or("", |(prefix, _)| prefix)
            };
            c.execute(
                "INSERT OR IGNORE INTO task_impact_roots VALUES(?1,?2,?3)",
                params![id, scope, prefix],
            )?;
        }
        let req = values
            .get("requirement_id")
            .or_else(|| old.get("requirement_id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        edge(
            c,
            scope,
            principal,
            "requirement",
            req,
            "acceptance",
            "criterion",
            &id,
        )?;
    }
    if entity == "check" {
        let criterion = values
            .get("criterion_id")
            .or_else(|| old.get("criterion_id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        edge(
            c,
            scope,
            principal,
            "criterion",
            criterion,
            "checked_by",
            "check",
            &id,
        )?;
    }
    if !create && entity == "criterion" {
        c.execute("UPDATE evidence SET stale=1 WHERE check_id IN(SELECT id FROM checks WHERE criterion_id=?1)",[&id])?;
    }
    if !create && entity == "check" {
        c.execute("UPDATE evidence SET stale=1 WHERE check_id=?1", [&id])?;
    }
    if !create && entity == "requirement" {
        crate::planning::requirement_changed(c, scope, &id, principal)?;
    }
    if entity == "assumption"
        && let Some(reqs) = data["linked_requirements"].as_array()
    {
        for req in reqs {
            let req = resolve(
                c,
                "requirement",
                scope,
                req.as_str()
                    .ok_or_else(|| fail("invalid assumption requirement"))?,
                principal,
            )?;
            edge(
                c,
                scope,
                principal,
                "assumption",
                &id,
                "informs",
                "requirement",
                &req,
            )?;
        }
    }
    if entity == "decision"
        && let Some(task) = data["task_id"].as_str()
    {
        let task = resolve(c, "task", scope, task, principal)?;
        edge(
            c, scope, principal, "decision", &id, "affects", "task", &task,
        )?;
    }
    old = save_version(c, entity, actual_scope, &id, principal)?;
    Ok(json!({"id":id,"entity":entity,"version":old["version"],"code":old["code"]}))
}
fn insert(c: &Connection, table: &str, values: &BTreeMap<String, Value>) -> Result<()> {
    let keys = values.keys().map(String::as_str).collect::<Vec<_>>();
    let marks = std::iter::repeat_n("?", keys.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("INSERT INTO {table}({}) VALUES({marks})", keys.join(","));
    c.execute(
        &sql,
        params_from_iter(values.iter().map(|(k, v)| encode(k, v))),
    )?;
    Ok(())
}
fn update(c: &Connection, table: &str, id: &str, values: &BTreeMap<String, Value>) -> Result<()> {
    let sql = format!(
        "UPDATE {table} SET {} WHERE id=?",
        values
            .keys()
            .map(|k| format!("{k}=?"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut params = values.iter().map(|(k, v)| encode(k, v)).collect::<Vec<_>>();
    params.push(SqlValue::Text(id.into()));
    c.execute(&sql, params_from_iter(params))?;
    Ok(())
}
#[allow(clippy::too_many_arguments)] // Both typed graph endpoints plus actor/scope are required.
pub(crate) fn edge(
    c: &Connection,
    scope: &str,
    principal: Option<&str>,
    source_type: &str,
    source: &str,
    relation: &str,
    target_type: &str,
    target: &str,
) -> Result<()> {
    let id = digest(format!("{scope}\0{source_type}\0{source}\0{relation}\0{target}").as_bytes());
    c.execute("INSERT OR IGNORE INTO edges(id,layer,source_type,source_id,relation_type,destination_type,destination_id,principal_id,scope_id) VALUES(?1,'plan',?2,?3,?4,?5,?6,?7,?8)",params![id,source_type,source,relation,target_type,target,principal,scope])?;
    Ok(())
}
pub(crate) fn node(c: &Connection, scope: &str, id: &str, principal: Option<&str>) -> Result<()> {
    for kind in [
        "task",
        "plan_item",
        "requirement",
        "milestone",
        "criterion",
        "check",
    ] {
        if load(c, kind, scope, id, principal).is_ok() {
            return Ok(());
        }
    }
    Err(fail(format!(
        "dependency endpoint {id} does not exist in scope"
    )))
}
pub(crate) fn reachable(c: &Connection, scope: &str, from: &str, to: &str) -> Result<bool> {
    Ok(c.query_row("WITH RECURSIVE paths(id) AS(SELECT ?1 UNION SELECT d.to_id FROM dependencies d JOIN paths p ON d.from_id=p.id WHERE d.scope_id=?2) SELECT EXISTS(SELECT 1 FROM paths WHERE id=?3)",params![from,scope,to],|r|r.get(0))?)
}
fn draft(c: &Connection, scope: &str, op: &Value, policy: &TaskPolicy) -> Result<Value> {
    let create = op["op"] == "create_record";
    let principal = policy.principal_id.clone().unwrap_or_default();
    let id = text(op, "id")?;
    let old:Option<(String,u64,String)>=c.query_row("SELECT entity,version,data FROM task_drafts WHERE scope_id=?1 AND principal_key=?2 AND id=?3",params![scope,principal,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let entity = op["entity"]
        .as_str()
        .or_else(|| old.as_ref().map(|v| v.0.as_str()))
        .ok_or_else(|| fail("record entity required"))?
        .to_owned();
    table(&entity)?;
    if entity == "project" && !policy.allow_projects {
        return Err(fail("projects disabled"));
    }
    let mut data = op.get("data").cloned().unwrap_or(json!({}));
    if data.to_string().len() > 4096 {
        return Err(fail("draft record exceeds 4096-byte data limit"));
    }
    let version = if create {
        if old.is_some() {
            return Err(fail("draft id already exists"));
        }
        1
    } else {
        let old = old.ok_or_else(|| fail("draft does not exist"))?;
        if old.0 != entity {
            return Err(fail("record entity cannot change"));
        }
        if op["expected_version"].as_u64() != Some(old.1) {
            return Err(fail(format!("version conflict: current_version={}", old.1)));
        }
        let patch = data;
        data = serde_json::from_str(&old.2)?;
        for (k, v) in patch.as_object().unwrap() {
            data[k] = v.clone();
        }
        old.1 + 1
    };
    c.execute("INSERT INTO task_drafts VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(scope_id,principal_key,id) DO UPDATE SET version=excluded.version,data=excluded.data",params![scope,principal,id,entity,version,data.to_string()])?;
    Ok(json!({"id":id,"entity":entity,"version":version,"trusted":false}))
}
fn query(c: &Connection, scope: &str, op: &Value, policy: &TaskPolicy) -> Result<Value> {
    let requested_entity = op["entity"].as_str();
    let after = op["after"].as_str().unwrap_or("");
    let limit = op["limit"].as_u64().unwrap_or(20).min(20) as usize;
    let entities = if let Some(e) = requested_entity {
        vec![entity_name(e)?]
    } else {
        vec![
            "task",
            "plan",
            "requirement",
            "criterion",
            "check",
            "evidence",
            "assumption",
            "decision",
            "notebook",
        ]
    };
    let mut values = Vec::new();
    for entity in entities {
        let t = table(entity)?;
        let columns = columns(c, t)?;
        let expression = |field: &str| -> Result<String> {
            if entity == "criterion" {
                match field{"status"=>return Ok("COALESCE(s.state,'unverified')".into()),"status_reason"=>return Ok("COALESCE(s.reason,'no evidence')".into()),"stale"=>return Ok("COALESCE((s.state='unverified' AND (s.reason LIKE '%stale%' OR s.reason LIKE '%changed%')),0)".into()),_=>{}}
            }
            if columns.iter().any(|c| c == field) {
                Ok(format!("t.{field}"))
            } else {
                Err(fail(format!("unknown query field {field} for {entity}")))
            }
        };
        let mut params = vec![
            SqlValue::Text(scope.into()),
            policy
                .principal_id
                .clone()
                .map(SqlValue::Text)
                .unwrap_or(SqlValue::Null),
            SqlValue::Text(after.into()),
        ];
        let mut conditions = vec![
            "COALESCE(t.scope_id,'general')=?".to_owned(),
            "t.principal_id IS ?".into(),
            "t.id>?".into(),
        ];
        if let Some(filter) = op["where"].as_object() {
            for (field, value) in filter {
                let expr = expression(field)?;
                let options = value
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![value.clone()]);
                if options.is_empty() {
                    conditions.push("0".into());
                } else {
                    conditions.push(format!(
                        "{expr} IN({})",
                        std::iter::repeat_n("?", options.len())
                            .collect::<Vec<_>>()
                            .join(",")
                    ));
                    params.extend(options.into_iter().map(|v| encode(field, &v)));
                }
            }
        }
        let mut selected = if op["fields"]
            .as_array()
            .is_some_and(|fields| fields.iter().any(|f| f == "data"))
        {
            columns.clone()
        } else if let Some(fields) = op["fields"].as_array() {
            fields
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| !matches!(*s, "entity" | "data"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        } else {
            columns.clone()
        };
        if !selected.iter().any(|f| f == "id") {
            selected.push("id".into());
        }
        if entity == "criterion" && op.get("fields").is_none() {
            selected.extend(["status".into(), "status_reason".into(), "stale".into()]);
        }
        let selects = selected
            .iter()
            .map(|f| expression(f).map(|expr| format!("{expr} AS {f}")))
            .collect::<Result<Vec<_>>>()?;
        let join = if entity == "criterion" {
            " LEFT JOIN task_criterion_state s ON s.criterion_id=t.id"
        } else {
            ""
        };
        let sql = format!(
            "SELECT {} FROM {t} t{join} WHERE {} ORDER BY t.id LIMIT 21",
            selects.join(","),
            conditions.join(" AND ")
        );
        let mut statement = c.prepare(&sql)?;
        for value in statement.query_map(params_from_iter(params), |r| row(r, &selected))? {
            let mut value = value?;
            value["entity"] = json!(entity);
            if entity == "criterion"
                && let Some(stale) = value["stale"].as_i64()
            {
                value["stale"] = json!(stale != 0);
            }
            if op.get("fields").is_none()
                || op["fields"]
                    .as_array()
                    .is_some_and(|fields| fields.iter().any(|f| f == "data"))
            {
                value["data"] = value.clone();
            }
            values.push(value);
        }
    }
    let principal = policy.principal_id.clone().unwrap_or_default();
    let mut drafts=c.prepare("SELECT id,entity,version,data FROM task_drafts WHERE scope_id=?1 AND principal_key=?2 AND (?3 IS NULL OR entity=?3) AND id>?4 ORDER BY id LIMIT 21")?;
    for r in drafts.query_map(params![scope, principal, requested_entity, after], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, u64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })? {
        let (id, entity, version, data) = r?;
        if op.get("where").is_none() {
            values.push(json!({"id":id,"entity":entity,"version":version,"data":serde_json::from_str::<Value>(&data)?,"trusted":false}));
        }
    }
    values.sort_by_key(|v| v["id"].as_str().unwrap_or_default().to_owned());
    let mut output = Vec::new();
    let mut bytes = 0;
    let mut more = false;
    let mut next = None;
    for mut value in values {
        let id = value["id"].clone();
        if let Some(fields) = op["fields"].as_array() {
            let mut selected = serde_json::Map::new();
            for field in fields {
                let key = field.as_str().unwrap();
                if let Some(v) = value.get(key) {
                    selected.insert(key.into(), v.clone());
                }
            }
            value = Value::Object(selected);
        }
        let mut size = value.to_string().len();
        if output.is_empty() && size > 8000 {
            value = json!({"id":id,"truncated":true,"reason":"requested row exceeds response budget; request fewer fields"});
            size = value.to_string().len();
        }
        if output.len() >= limit || bytes + size > 8000 {
            more = true;
            break;
        }
        bytes += size;
        next = Some(id);
        output.push(value);
    }
    Ok(json!({"rows":output,"more":more,"has_more":more,"next":next}))
}

fn reserved_metadata(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "implicit_final_response" | "user_requested_verification" | "workspace"
            ) || reserved_metadata(value)
        }),
        Value::Array(values) => values.iter().any(reserved_metadata),
        _ => false,
    }
}

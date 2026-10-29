//! Authoritative task facts for deterministic context reconstruction.
use super::*;
use excalibur_context::{ContextFact, StructuredState};

pub(super) async fn load(store: &TaskStore, run: &str) -> Result<Option<StructuredState>> {
    let Some(task) = store.task_for_run(run).await? else {
        return Ok(None);
    };
    let original = if let Some(literal) = task
        .request_card
        .get("literal_original")
        .and_then(Value::as_str)
    {
        literal.to_owned()
    } else {
        literal_request(&task.request_card)?
    };
    let (requirements, criteria) = store.requirements_and_criteria(&task.id).await?;
    let mut state = StructuredState {
        objective: task.title.clone(),
        original_request: original,
        ..Default::default()
    };
    let (task_id, scope, principal) = (
        task.id.clone(),
        task.scope_id.clone(),
        store.policy.principal_id.clone(),
    );
    let (requirement_states,plan,decisions,assumptions,replans)=store.db.read(ReadLane::Interactive,move|c|{
  let mut s=c.prepare("SELECT r.id,r.state FROM requirements r JOIN task_requirements tr ON tr.requirement_id=r.id WHERE tr.task_id=?1 AND r.principal_id IS ?2 ORDER BY r.id")?;
  let requirements=s.query_map(rusqlite::params![task_id,principal],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<std::collections::BTreeMap<_,_>,_>>()?;
  let mut s=c.prepare("SELECT i.id,i.title,COALESCE(i.detail,''),i.state FROM plan_items i JOIN plans p ON p.id=i.plan_id WHERE p.owner_task_id=?1 AND p.kind='micro' AND p.state NOT IN('retired','archived') AND p.scope_id=?2 AND p.principal_id IS ?3 AND i.principal_id IS ?3 ORDER BY p.version DESC,i.priority DESC,i.id")?;
  let plan=s.query_map(rusqlite::params![task_id,scope,principal],|r|Ok(ContextFact{id:r.get(0)?,text:format!("{}\n{}",r.get::<_,String>(1)?,r.get::<_,String>(2)?),state:r.get(3)?}))?.collect::<std::result::Result<Vec<_>,_>>()?;
  let mut s=c.prepare("SELECT id,statement,COALESCE(reason,''),state FROM decisions WHERE scope_id=?1 AND principal_id IS ?2 AND state NOT IN('retired','superseded') ORDER BY id")?;
  let decisions=s.query_map(rusqlite::params![scope,principal],|r|Ok(ContextFact{id:r.get(0)?,text:format!("{}\n{}",r.get::<_,String>(1)?,r.get::<_,String>(2)?),state:r.get(3)?}))?.collect::<std::result::Result<Vec<_>,_>>()?;
  let mut s=c.prepare("SELECT id,text,state FROM assumptions WHERE scope_id=?1 AND principal_id IS ?2 AND state NOT IN('retired','resolved','cancelled') ORDER BY id")?;
  let assumptions=s.query_map(rusqlite::params![scope,principal],|r|Ok(ContextFact{id:r.get(0)?,text:r.get(1)?,state:r.get(2)?}))?.collect::<std::result::Result<Vec<_>,_>>()?;
  let mut s=c.prepare("SELECT id,reason,state FROM task_replan_requests WHERE task_id=?1 AND principal_id IS ?2 AND state='pending' ORDER BY id")?;
  let replans=s.query_map(rusqlite::params![task_id,principal],|r|Ok(ContextFact{id:r.get(0)?,text:r.get(1)?,state:r.get(2)?}))?.collect::<std::result::Result<Vec<_>,_>>()?;
  Ok((requirements,plan,decisions,assumptions,replans))
 }).await?;
    state.requirements = requirements
        .into_iter()
        .map(|r| ContextFact {
            id: r.id.clone(),
            text: r.text,
            state: requirement_states
                .get(&r.id)
                .cloned()
                .unwrap_or_else(|| "pending".into()),
        })
        .collect();
    for criterion in criteria {
        let revision = store.criterion_revision(&criterion.id).await?;
        let assessment = store.assess_criterion(&criterion, revision).await?;
        state.criteria.push(ContextFact {
            id: criterion.id,
            text: criterion.statement,
            state: serde_json::to_value(assessment.state)?
                .as_str()
                .ok_or_else(|| fail("invalid criterion state"))?
                .to_owned(),
        });
    }
    state.next_steps = plan
        .iter()
        .filter(|p| !matches!(p.state.as_str(), "done" | "completed" | "cancelled"))
        .map(|p| format!("{}: {}", p.id, p.text))
        .collect();
    state.next_steps.extend(
        replans
            .iter()
            .map(|p| format!("Pending replan {}: {}", p.id, p.text)),
    );
    state
        .open_ids
        .extend(assumptions.iter().chain(&replans).map(|p| p.id.clone()));
    state.decisions = decisions;
    state.plan = plan;
    // Open assumptions and questions stay explicit; they cannot disappear into a summary.
    state.next_steps.extend(
        assumptions
            .iter()
            .map(|fact| format!("Resolve assumption {}: {}", fact.id, fact.text)),
    );
    for (index, question) in task.request_card["open_questions"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        if matches!(
            question["state"].as_str(),
            Some("resolved" | "closed" | "cancelled")
        ) {
            continue;
        }
        let text = question
            .as_str()
            .or_else(|| question["question"].as_str())
            .or_else(|| question["text"].as_str())
            .ok_or_else(|| fail("invalid original request question"))?;
        let id = question["id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}:question:{index}", task.id));
        state.open_ids.insert(id.clone());
        state
            .next_steps
            .push(format!("Resolve question {id}: {text}"));
    }
    let current = store.task(&task.scope_id, &task.id).await?;
    if current.version != task.version || current.request_card != task.request_card {
        return Err(fail(
            "task changed while reconstructing context; retry with current revision",
        ));
    }
    // Use the same live redactor as task operations, before any prompt or durable view.
    Ok(Some(serde_json::from_value(
        store.redactor.redact_json(&serde_json::to_value(state)?),
    )?))
}

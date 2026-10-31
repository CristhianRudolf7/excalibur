//! Orchestrator-owned delegated context: custom profile identity, inherited
//! natively sized L0 cores and the run budget guard for fixed L0 text.
mod support;
use excalibur_orchestrator::*;
use excalibur_types::runtime::*;
use serde_json::json;
use support::*;

fn lead() -> CustomProfile {
    CustomProfile {
        id: "lead".into(),
        name: "Lead".into(),
        base_profile: "coordinator".into(),
        identity: "Synthetic lead identity: plan carefully.".into(),
        icon: None,
    }
}
fn reader() -> CustomProfile {
    CustomProfile {
        id: "reader".into(),
        name: "Reader".into(),
        base_profile: "explorer".into(),
        identity: "Synthetic reader identity: quote evidence.".into(),
        icon: None,
    }
}
fn header(profile: &CustomProfile) -> String {
    format!("Custom profile {} ({}):", profile.id, profile.name)
}
async fn context(f: &Fixture, agent: &Agent) -> excalibur_context::ContextInput {
    f.host
        .engine()
        .load(agent.run_id.as_ref().unwrap())
        .await
        .unwrap()
        .spec
        .context
}
async fn tool_spawn(f: &Fixture, parent: &Agent, key: &str, profile: &str) -> ToolResult {
    let adapter = OrchestratorTools::new(no_tools());
    adapter.attach(&f.host).unwrap();
    let run = parent.run_id.as_ref().unwrap();
    let call = ToolCall {
        id: key.into(),
        name: "subagent".into(),
        arguments: json!({"action":"spawn","delegation":{"name":key,"profile":profile,"goal":format!("Synthetic {key} goal"),"deliverable":"A bounded summary"}}).to_string(),
    };
    let prepared = adapter.prepare(run, call).await.unwrap();
    adapter.execute(run, key, &prepared).await
}

#[tokio::test]
async fn each_delegation_carries_exactly_its_own_profile_identity() {
    let f = fixture().await;
    f.host.register_profile(lead()).await.unwrap();
    f.host.register_profile(reader()).await.unwrap();
    let mut root = delegation("root", "general", 300);
    root.profile = "lead".into();
    let parent = f.host.spawn(&f.master.id, root).await.unwrap();
    let parent_l1 = context(&f, &parent).await.layers[1].clone();
    assert_eq!(parent_l1.matches(&header(&lead())).count(), 1);

    // Re-preparing the stored delegation keeps one identity block.
    let mut again = parent.delegation.clone().unwrap();
    again.operation = "root-again".into();
    again.name = "root-again".into();
    let respawned = f.host.spawn(&f.master.id, again).await.unwrap();
    assert_eq!(context(&f, &respawned).await.layers[1], parent_l1);

    // A built-in child created through the tool carries no custom identity.
    let explorer = tool_spawn(&f, &parent, "explorer-child", "explorer").await;
    assert!(explorer.success, "{:?}", explorer.output);
    let explorer: Agent = serde_json::from_value(explorer.output).unwrap();
    let l1 = context(&f, &explorer).await.layers[1].clone();
    assert!(!l1.contains("Custom profile"), "{l1}");

    // A custom child carries only its own identity, once.
    let custom = tool_spawn(&f, &parent, "reader-child", "reader").await;
    assert!(custom.success, "{:?}", custom.output);
    let custom: Agent = serde_json::from_value(custom.output).unwrap();
    assert_eq!(custom.profile, "explorer");
    let l1 = context(&f, &custom).await.layers[1].clone();
    assert!(!l1.contains(&header(&lead())), "{l1}");
    assert!(!l1.contains("plan carefully"), "{l1}");
    assert_eq!(l1.matches(&header(&reader())).count(), 1);
    assert_eq!(l1.matches("quote evidence").count(), 1);
}

#[tokio::test]
async fn tool_children_inherit_a_natively_sized_core_within_their_own_budget() {
    let f = fixture().await;
    let core = "Synthetic delegated rule. ".repeat(520);
    let mut root = delegation("root", "general", 300);
    let (sized, sizing) = f
        .host
        .engine()
        .native_context(
            &core,
            "Synthetic task",
            "fixed",
            &root.spec.budget,
            "\n[excalibur-host-lane:1]\n",
        )
        .unwrap();
    assert!(sizing.core_tokens > 4096);
    root.spec.context = sized;
    let parent = f.host.spawn(&f.master.id, root).await.unwrap();
    let parent_context = context(&f, &parent).await;

    let result = tool_spawn(&f, &parent, "sized-child", "explorer").await;
    assert!(result.success, "{:?}", result.output);
    let child: Agent = serde_json::from_value(result.output).unwrap();
    let child_context = context(&f, &child).await;
    assert_eq!(child_context.layers[0], parent_context.layers[0]);
    assert_eq!(
        child_context.token_budgets[0],
        parent_context.token_budgets[0]
    );
    assert_eq!(
        &child_context.token_budgets[1..],
        &excalibur_context::ContextInput::new("", "", "").token_budgets[1..]
    );
}

#[tokio::test]
async fn a_core_larger_than_the_child_run_budget_is_rejected_at_spawn() {
    let f = fixture().await;
    // Parent affords a core the default child budget (at most 30,000) cannot.
    let core = "Synthetic delegated rule. ".repeat(4_000);
    let mut root = delegation("root", "general", 300);
    let (sized, sizing) = f
        .host
        .engine()
        .native_context(
            &core,
            "Synthetic task",
            "fixed",
            &root.spec.budget,
            "\n[excalibur-host-lane:1]\n",
        )
        .unwrap();
    assert!(sizing.core_tokens > 30_000);
    root.spec.context = sized;
    let parent = f.host.spawn(&f.master.id, root).await.unwrap();
    let result = tool_spawn(&f, &parent, "oversized-child", "explorer").await;
    assert!(!result.success);
    assert!(
        result.output.to_string().contains("exceeds the run"),
        "{:?}",
        result.output
    );
    assert!(
        !f.host
            .list_agents()
            .await
            .unwrap()
            .iter()
            .any(|agent| agent.name == "oversized-child")
    );

    // A fixed L0 that the run's own max_tokens cannot afford is rejected
    // clearly before any durable spawn state, not later in the run.
    let mut fixed = delegation("fixed", "general", 300);
    fixed.profile = "coordinator".into();
    fixed.spec.budget.max_tokens = 64;
    let error = f.host.spawn(&f.master.id, fixed).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("L0 core exceeds the child run token budget"),
        "{error}"
    );
    let mut retry = delegation("fixed", "general", 300);
    retry.profile = "coordinator".into();
    assert!(f.host.spawn(&f.master.id, retry).await.is_ok());
}

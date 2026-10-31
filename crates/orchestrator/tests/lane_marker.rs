//! Lane-marker preparation must be idempotent and match native core sizing.
mod support;
use excalibur_orchestrator::*;
use support::*;

const CORE: &str = "Synthetic delegated rules.";

fn marker(lane: u8) -> String {
    format!("\n[excalibur-host-lane:{lane}]\n")
}

fn marker_count(layer: &str) -> usize {
    (0..=3)
        .map(|lane| layer.matches(&marker(lane)).count())
        .sum()
}

async fn layer0(f: &Fixture, agent: &Agent) -> (String, usize) {
    let context = f
        .host
        .engine()
        .load(agent.run_id.as_ref().unwrap())
        .await
        .unwrap()
        .spec
        .context;
    (context.layers[0].clone(), context.token_budgets[0])
}

/// A delegation whose L0 was sized natively with the P1 lane suffix, as the
/// daemon does for master-created delegations.
fn natively_sized(f: &Fixture, operation: &str) -> Delegation {
    let mut delegation = delegation(operation, "general", 300);
    let (context, sizing) = f
        .host
        .engine()
        .native_context(
            CORE,
            "Synthetic task",
            "fixed",
            &delegation.spec.budget,
            &marker(1),
        )
        .unwrap();
    assert_eq!(context.token_budgets[0], sizing.core_tokens);
    delegation.spec.context = context;
    delegation
}

#[tokio::test]
async fn first_spawn_matches_native_sizing_and_stored_delegations_respawn_without_growth() {
    let f = fixture().await;
    let parent = f
        .host
        .spawn(&f.master.id, natively_sized(&f, "root"))
        .await
        .unwrap();
    let (parent_l0, parent_budget) = layer0(&f, &parent).await;
    // The first preparation produces exactly the text native sizing counted.
    assert_eq!(parent_l0, format!("{CORE}{}", marker(1)));
    let stored = parent.delegation.clone().unwrap();
    assert_eq!(stored.spec.context.layers[0], parent_l0);

    // Re-prepare the stored parent delegation, then the delegation stored for
    // that child: two successive preparations must leave L0 byte-identical.
    let mut layers = Vec::new();
    let mut source = stored;
    for operation in ["respawn-a", "respawn-b"] {
        let mut request = source;
        request.operation = operation.into();
        request.name = operation.into();
        request.profile = "implementer".into();
        request.token.budget = 10;
        request.spec.budget.max_tokens = 40_000;
        request.spec.budget.max_steps = 20;
        request.spec.budget.max_tool_calls = 10;
        request.spec.budget.max_wall_seconds = 60;
        let child = f.host.spawn(&parent.id, request).await.unwrap();
        let (child_l0, child_budget) = layer0(&f, &child).await;
        assert_eq!(child_budget, parent_budget);
        layers.push(child_l0);
        source = child.delegation.unwrap();
    }
    assert_eq!(layers, vec![parent_l0.clone(), parent_l0]);
}

#[tokio::test]
async fn a_priority_change_swaps_the_marker_without_growth() {
    let f = fixture().await;
    let parent = f
        .host
        .spawn(&f.master.id, natively_sized(&f, "root"))
        .await
        .unwrap();
    let (p1, _) = layer0(&f, &parent).await;
    for (operation, priority) in [("p2", 2), ("p3", 3)] {
        let mut request = parent.delegation.clone().unwrap();
        request.operation = operation.into();
        request.name = operation.into();
        request.profile = "implementer".into();
        request.token.budget = 10;
        request.spec.budget.max_tokens = 40_000;
        request.spec.budget.max_steps = 20;
        request.spec.budget.max_tool_calls = 10;
        request.spec.budget.max_wall_seconds = 60;
        request.priority = priority;
        let child = f.host.spawn(&parent.id, request).await.unwrap();
        let (layer, _) = layer0(&f, &child).await;
        assert_eq!(layer, format!("{CORE}{}", marker(priority)));
        assert_eq!(layer.len(), p1.len());
        assert_eq!(marker_count(&layer), 1);
    }
}

#[tokio::test]
async fn foreign_markers_and_trailing_remnants_cannot_survive_preparation() {
    let f = fixture().await;
    let mut request = delegation("root", "general", 300);
    request.spec.context.layers[0] = format!(
        "{CORE}{}middle{}\n[excalibur-host-lane:3]",
        marker(3),
        marker(2)
    );
    let agent = f.host.spawn(&f.master.id, request).await.unwrap();
    let (layer, _) = layer0(&f, &agent).await;
    assert_eq!(layer, format!("{CORE}\nmiddle\n{}", marker(1)));
    assert_eq!(marker_count(&layer), 1);
}

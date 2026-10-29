//! Conservative, durable tree-budget reservations; no child orchestrator.
use crate::{Budget, Error, Result, Snapshot, Status};
use excalibur_journal::Journal;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AllocatedUsage {
    #[serde(default)]
    pub estimated: bool,
    pub steps: u64,
    pub tokens: u64,
    pub cost_microusd: u64,
    pub wall_seconds: u64,
    pub tool_calls: u64,
    #[serde(default)]
    pub processes: u64,
    #[serde(default)]
    pub terminal_sessions: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChildBudgetAllocation {
    pub child_run: String,
    pub budget: Budget,
    #[serde(default)]
    pub settled: Option<AllocatedUsage>,
}
impl ChildBudgetAllocation {
    pub fn held(&self) -> AllocatedUsage {
        self.settled.clone().unwrap_or(AllocatedUsage {
            estimated: true,
            steps: self.budget.max_steps,
            tokens: self.budget.max_tokens,
            cost_microusd: self.budget.max_cost_microusd.unwrap_or(0),
            wall_seconds: self.budget.max_wall_seconds,
            tool_calls: self.budget.max_tool_calls,
            processes: self.budget.max_processes.unwrap_or(0),
            terminal_sessions: self.budget.max_terminal_sessions.unwrap_or(0),
        })
    }
}
pub(crate) async fn snapshot(journal: &Journal, run: &str) -> Result<Snapshot> {
    let first = journal
        .entries(run, 0, 1)
        .await?
        .pop()
        .ok_or(Error::Invalid("run missing"))?;
    let mut state = Snapshot::started(&first.payload)?;
    for event in journal.replay(run, first.sequence).await? {
        state.apply(&event)?;
    }
    Ok(state)
}
pub fn remaining_budget(state: &Snapshot) -> Budget {
    let mut used = AllocatedUsage {
        estimated: state.usage.estimated,
        steps: state.step.max(state.model_calls),
        tokens: state
            .usage
            .input_tokens
            .saturating_add(state.usage.output_tokens),
        cost_microusd: state.usage.cost_microusd,
        wall_seconds: excalibur_store::unix_seconds()
            .saturating_sub(state.started_at)
            .max(0) as u64,
        tool_calls: state.tool_calls,
        processes: 0,
        terminal_sessions: 0,
    };
    for allocation in state.child_allocations.values() {
        let held = allocation.held();
        used.steps = used.steps.saturating_add(held.steps);
        used.tokens = used.tokens.saturating_add(held.tokens);
        used.cost_microusd = used.cost_microusd.saturating_add(held.cost_microusd);
        used.wall_seconds = used.wall_seconds.saturating_add(held.wall_seconds);
        used.tool_calls = used.tool_calls.saturating_add(held.tool_calls);
        used.processes = used.processes.saturating_add(held.processes);
        used.terminal_sessions = used
            .terminal_sessions
            .saturating_add(held.terminal_sessions);
    }
    let b = &state.spec.budget;
    Budget {
        max_steps: b.max_steps.saturating_sub(used.steps),
        max_tokens: b.max_tokens.saturating_sub(used.tokens),
        max_cost_microusd: b
            .max_cost_microusd
            .map(|max| max.saturating_sub(used.cost_microusd)),
        max_wall_seconds: b.max_wall_seconds.saturating_sub(used.wall_seconds),
        max_tool_calls: b.max_tool_calls.saturating_sub(used.tool_calls),
        tool_timeout_seconds: b.tool_timeout_seconds,
        max_processes: b
            .max_processes
            .map(|cap| cap.saturating_sub(used.processes)),
        max_terminal_sessions: b
            .max_terminal_sessions
            .map(|cap| cap.saturating_sub(used.terminal_sessions)),
    }
}
pub async fn remaining_budget_for(journal: &Journal, run: &str) -> Result<Budget> {
    Ok(remaining_budget(&snapshot(journal, run).await?))
}
/// The host binds a real child before dispatch. Reservations also reduce the
/// parent's local budget, so parent and child cannot both spend the same share.
pub async fn reserve_child_budget(
    journal: &Journal,
    parent: &str,
    id: &str,
    child: &str,
    budget: Budget,
) -> Result<Budget> {
    if id.is_empty() || id.len() > 128 || parent == child {
        return Err(Error::Invalid("invalid child allocation identity"));
    }
    for _ in 0..64 {
        let state = snapshot(journal, parent).await?;
        if let Some(known) = state.child_allocations.get(id) {
            return if known.child_run == child && known.budget == budget {
                Ok(budget)
            } else {
                Err(Error::Invalid(
                    "allocation key reused with different budget",
                ))
            };
        }
        let child_state = snapshot(journal, child).await?;
        if child_state.spec.budget != budget || child_state.status != Status::Ready {
            return Err(Error::Invalid(
                "child must be ready with exactly the allocated budget",
            ));
        }
        if state.request.is_some() && state.response.is_none() {
            return Err(Error::Invalid(
                "cannot reserve child budget during an unaccounted model call",
            ));
        }
        if !matches!(
            state.status,
            Status::Ready | Status::Running | Status::Verifying
        ) {
            return Err(Error::Invalid("parent cannot allocate in current state"));
        }
        let remaining = remaining_budget(&state);
        if budget.max_steps == 0
            || budget.max_tokens == 0
            || budget.max_wall_seconds == 0
            || budget.tool_timeout_seconds == 0
            || budget.max_steps > remaining.max_steps
            || budget.max_tokens > remaining.max_tokens
            || budget.max_wall_seconds > remaining.max_wall_seconds
            || budget.max_tool_calls > remaining.max_tool_calls
            || budget.tool_timeout_seconds > remaining.tool_timeout_seconds
            || remaining
                .max_cost_microusd
                .is_some_and(|limit| budget.max_cost_microusd.is_none_or(|cost| cost > limit))
            || remaining
                .max_processes
                .is_some_and(|limit| budget.max_processes.is_none_or(|cap| cap > limit))
            || remaining
                .max_terminal_sessions
                .is_some_and(|limit| budget.max_terminal_sessions.is_none_or(|cap| cap > limit))
        {
            return Err(Error::Invalid(
                "child budget exceeds unallocated parent budget",
            ));
        }
        let payload = json!({"allocation_id":id,"child_sequence":child_state.last_sequence,"allocation":ChildBudgetAllocation{child_run:child.into(),budget:budget.clone(),settled:None}});
        if journal
            .append_child_budget_reservation(
                parent,
                state.last_sequence,
                &format!("allocation:{id}"),
                payload,
            )
            .await?
            .is_some()
        {
            return Ok(budget);
        }
    }
    Err(Error::Invalid(
        "parent budget changed repeatedly; retry allocation",
    ))
}
/// Replace a reservation by confirmed terminal consumption, retaining any nested
/// reservations conservatively. Repeated settlement is an immutable no-op.
pub async fn settle_child_budget(
    journal: &Journal,
    parent: &str,
    id: &str,
) -> Result<AllocatedUsage> {
    for _ in 0..64 {
        let state = snapshot(journal, parent).await?;
        let allocation = state
            .child_allocations
            .get(id)
            .ok_or(Error::Invalid("allocation missing"))?;
        if let Some(usage) = &allocation.settled {
            return Ok(usage.clone());
        }
        let child = snapshot(journal, &allocation.child_run).await?;
        if !matches!(
            child.status,
            Status::Completed | Status::CompletedWithPending | Status::Cancelled | Status::Failed
        ) {
            return Err(Error::Invalid("only terminal child consumption can settle"));
        }
        let mut used = AllocatedUsage {
            estimated: child.usage.estimated,
            steps: child.step.max(child.model_calls),
            tokens: if child.usage.estimated {
                allocation.budget.max_tokens.max(
                    child
                        .usage
                        .input_tokens
                        .saturating_add(child.usage.output_tokens),
                )
            } else {
                child
                    .usage
                    .input_tokens
                    .saturating_add(child.usage.output_tokens)
            },
            cost_microusd: if child.usage.estimated {
                allocation
                    .budget
                    .max_cost_microusd
                    .unwrap_or(child.usage.cost_microusd)
            } else {
                child.usage.cost_microusd
            },
            wall_seconds: excalibur_store::unix_seconds()
                .saturating_sub(child.started_at)
                .max(0) as u64,
            tool_calls: child.tool_calls,
            processes: 0,
            terminal_sessions: 0,
        };
        for nested in child.child_allocations.values() {
            let held = nested.held();
            used.estimated |= held.estimated;
            used.steps = used.steps.saturating_add(held.steps);
            used.tokens = used.tokens.saturating_add(held.tokens);
            used.cost_microusd = used.cost_microusd.saturating_add(held.cost_microusd);
            used.wall_seconds = used.wall_seconds.saturating_add(held.wall_seconds);
            used.tool_calls = used.tool_calls.saturating_add(held.tool_calls);
            used.processes = used.processes.saturating_add(held.processes);
            used.terminal_sessions = used
                .terminal_sessions
                .saturating_add(held.terminal_sessions);
        }
        if journal
            .append_if_sequence(
                parent,
                state.last_sequence,
                "child_budget_settled",
                &format!("allocation:{id}:settled"),
                json!({"allocation_id":id,"usage":used}),
            )
            .await?
            .is_some()
        {
            return Ok(used);
        }
    }
    Err(Error::Invalid(
        "parent budget changed repeatedly; retry settlement",
    ))
}

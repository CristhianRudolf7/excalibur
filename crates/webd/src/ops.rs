//! Server-side `op.cancel` decision helper (arch 16.5, 16.10; NB-13).
//!
//! Client polling stop already exists in the harness adapter; this module is
//! the webd-side half of dedicated dispatch cancellation for in-flight
//! operations. It validates the `op.cancel` parameters with the same bounds
//! the JSON-RPC boundary enforces (`run_id` non-empty up to 256 bytes,
//! `op_key` non-empty up to 512 bytes, `deadline_ms` in `1..=300_000`) and
//! classifies the stored operation state so the dispatcher can answer
//! without ever resending an effect:
//!
//! * `received` (no effect dispatched yet) is safe to mark `cancelled`;
//! * `dispatched` with a live worker asks the worker to stop cooperatively
//!   and stays `working` until the worker settles;
//! * `dispatched` with no live worker means webd restarted after dispatch:
//!   the effect is unknown and is never resent (`uncertain`, same mapping as
//!   `op.get`);
//! * `completed`/`failed` return the stored outcome and change nothing.
//!
//! Persisting the `cancelled` mark into `web_ops` and interrupting the live
//! dispatch worker happen in the browser-crate `Protocol` dispatch, which is
//! main-lane work outside this closure lane; the harness adapter already
//! tolerates `method_not_found` there and keeps the existing polling-stop
//! plus ephemeral-cleanup fallback.

use serde_json::{Value, json};

/// Maximum `run_id` length accepted by the JSON-RPC boundary.
pub const MAX_RUN_ID_LEN: usize = 256;
/// Maximum `op_key` length accepted by the JSON-RPC boundary.
pub const MAX_OP_KEY_LEN: usize = 512;
/// Maximum `deadline_ms` accepted by the JSON-RPC boundary.
pub const MAX_DEADLINE_MS: u64 = 300_000;

/// Validate `op.cancel` parameters with the boundary bounds.
pub fn validate_cancel(run: &str, op_key: &str, deadline_ms: u64) -> Result<(), String> {
    if run.is_empty() || run.len() > MAX_RUN_ID_LEN {
        return Err("invalid_params: run_id required".into());
    }
    if op_key.is_empty() || op_key.len() > MAX_OP_KEY_LEN {
        return Err("invalid_params: op_key required".into());
    }
    if deadline_ms == 0 || deadline_ms > MAX_DEADLINE_MS {
        return Err("invalid_params: deadline_ms must be 1..300000".into());
    }
    Ok(())
}

/// Server-side cancellation outcome for a stored operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelDecision {
    /// `received`: no effect dispatched yet, safe to mark `cancelled`.
    CancelReceived,
    /// `dispatched` with a live worker: request cooperative stop; the
    /// operation stays `working` until the worker settles.
    RequestWorkerCancel,
    /// `dispatched` with no live worker (webd restarted after dispatch):
    /// effect unknown, never resend.
    UncertainRestart,
    /// `completed` or `failed`: return the stored outcome, change nothing.
    AlreadyTerminal,
}

/// Classify the stored operation state for `op.cancel`.
///
/// `owner` reports whether the stored `run_id` matches the caller; `live`
/// reports whether the dispatch worker is still registered. Unknown owners
/// are denied and unknown states are reported as unknown operations; a
/// cancelled operation is already terminal.
pub fn decide_cancel(state: &str, owner: bool, live: bool) -> Result<CancelDecision, String> {
    if !owner {
        return Err("denied: operation_owner".into());
    }
    match state {
        "received" => Ok(CancelDecision::CancelReceived),
        "dispatched" if live => Ok(CancelDecision::RequestWorkerCancel),
        "dispatched" => Ok(CancelDecision::UncertainRestart),
        "completed" | "failed" | "cancelled" => Ok(CancelDecision::AlreadyTerminal),
        _ => Err("unknown_operation: op_key".into()),
    }
}

/// Build the `op.cancel` result payload for a non-terminal decision.
///
/// Terminal operations have no new payload: the dispatcher returns the
/// stored `op.get` outcome instead, so this returns `None` for
/// [`CancelDecision::AlreadyTerminal`].
pub fn cancel_result(decision: CancelDecision, op_key: &str) -> Option<Value> {
    match decision {
        CancelDecision::CancelReceived => Some(json!({"state": "cancelled", "op_key": op_key})),
        CancelDecision::RequestWorkerCancel => {
            Some(json!({"state": "working", "op_key": op_key, "cancel_requested": true}))
        }
        CancelDecision::UncertainRestart => Some(
            json!({"state": "uncertain", "op_key": op_key, "reason": "webd_restarted_after_dispatch"}),
        ),
        CancelDecision::AlreadyTerminal => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_params_reject_empty_and_oversized_values() {
        assert!(validate_cancel("run", "op", 5_000).is_ok());
        assert!(validate_cancel("", "op", 5_000).is_err());
        assert!(validate_cancel("run", "", 5_000).is_err());
        assert!(validate_cancel("run", "op", 0).is_err());
        assert!(validate_cancel("run", "op", MAX_DEADLINE_MS + 1).is_err());
        assert!(validate_cancel(&"r".repeat(MAX_RUN_ID_LEN + 1), "op", 5_000).is_err());
        assert!(validate_cancel("run", &"k".repeat(MAX_OP_KEY_LEN + 1), 5_000).is_err());
    }

    #[test]
    fn cancel_decision_denies_foreign_owners_and_unknown_states() {
        assert_eq!(
            decide_cancel("received", false, false).unwrap_err(),
            "denied: operation_owner"
        );
        assert_eq!(
            decide_cancel("bogus", true, false).unwrap_err(),
            "unknown_operation: op_key"
        );
    }

    #[test]
    fn cancel_decision_maps_every_stored_state() {
        assert_eq!(
            decide_cancel("received", true, false),
            Ok(CancelDecision::CancelReceived)
        );
        assert_eq!(
            decide_cancel("dispatched", true, true),
            Ok(CancelDecision::RequestWorkerCancel)
        );
        assert_eq!(
            decide_cancel("dispatched", true, false),
            Ok(CancelDecision::UncertainRestart)
        );
        for terminal in ["completed", "failed", "cancelled"] {
            assert_eq!(
                decide_cancel(terminal, true, true),
                Ok(CancelDecision::AlreadyTerminal),
                "{terminal}"
            );
        }
    }

    #[test]
    fn cancel_result_never_resends_an_effect() {
        let cancelled = cancel_result(CancelDecision::CancelReceived, "op-1").unwrap();
        assert_eq!(cancelled["state"], json!("cancelled"));
        let working = cancel_result(CancelDecision::RequestWorkerCancel, "op-1").unwrap();
        assert_eq!(working["state"], json!("working"));
        assert_eq!(working["cancel_requested"], json!(true));
        let uncertain = cancel_result(CancelDecision::UncertainRestart, "op-1").unwrap();
        assert_eq!(uncertain["state"], json!("uncertain"));
        assert_eq!(uncertain["reason"], json!("webd_restarted_after_dispatch"));
        assert!(cancel_result(CancelDecision::AlreadyTerminal, "op-1").is_none());
    }
}

//! Installed-agent error labels from a pinned Harbor policy. The external
//! adapter decides how to raise/classify them; this library never runs Harbor.
use serde::{Deserialize, Serialize};
pub const HARBOR_VERSION: &str = "0.25.0";
pub const HARBOR_COMMIT: &str = "509b119db0d97b141dcd9c80f4f7ab861c47c2d6";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledFailure {
    pub harbor_version: String,
    pub harbor_commit: String,
    pub exception_type: String,
    pub failure_class: Option<String>,
    pub root_exit_code: i32,
    pub source: String,
}
pub fn installed_failure(
    class: Option<&str>,
    exit_code: i32,
    reason: Option<&str>,
) -> InstalledFailure {
    let kind = if exit_code == 130 {
        "CancelledError"
    } else {
        match class {
            Some("authentication") => "AgentAuthenticationError",
            Some("permission") => "PermissionDeniedError",
            Some("rate_limit") => "ApiRateLimitError",
            Some("quota") => "ApiUsageLimitError",
            Some("overloaded") => "ApiOverloadedError",
            Some("context_window_exceeded") => "ContextWindowExceededError",
            Some("timeout") => "Timeout",
            Some("agent_timeout") => "AgentTimeoutError",
            Some("server") => "ApiInternalServerError",
            Some("transport") => "ApiConnectionError",
            Some("invalid_request") => "BadRequestError",
            Some("invalid_response") => "APIResponseValidationError",
            _ if exit_code == 4 && reason.is_some_and(|r| r.contains("wall time")) => {
                "AgentTimeoutError"
            }
            _ => "NonZeroAgentExitCodeError",
        }
    };
    InstalledFailure {
        harbor_version: HARBOR_VERSION.into(),
        harbor_commit: HARBOR_COMMIT.into(),
        exception_type: kind.into(),
        failure_class: class.map(str::to_owned),
        root_exit_code: exit_code,
        source: "excalibur_installed_contract".into(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_distinguish_provider_limits_and_preserve_unknown_failures() {
        for (class, label) in [
            ("quota", "ApiUsageLimitError"),
            ("rate_limit", "ApiRateLimitError"),
            ("overloaded", "ApiOverloadedError"),
            ("context_window_exceeded", "ContextWindowExceededError"),
            ("authentication", "AgentAuthenticationError"),
        ] {
            assert_eq!(
                installed_failure(Some(class), 5, None).exception_type,
                label
            );
        }
        assert_eq!(
            installed_failure(Some("unknown"), 5, None).exception_type,
            "NonZeroAgentExitCodeError"
        );
        assert_eq!(
            installed_failure(None, 130, None).exception_type,
            "CancelledError"
        );
        assert_eq!(
            installed_failure(None, 4, Some("wall time budget")).exception_type,
            "AgentTimeoutError"
        );
        assert_eq!(
            installed_failure(None, 4, Some("budget")).exception_type,
            "NonZeroAgentExitCodeError"
        );
    }
}

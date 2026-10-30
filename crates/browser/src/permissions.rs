//! Denied browser permissions for every context (12.6).
//!
//! Chrome grants powerful capabilities through `Browser.setPermission`. Every
//! browser context starts with the sensitive set denied; individual sites may
//! grant concrete permissions later. This module only declares the default
//! denials and builds the CDP parameters deterministically.
use serde_json::{Value, json};

/// Permissions denied by default in every browser context.
pub const DENIED_PERMISSIONS: &[&str] = &[
    "geolocation",
    "notifications",
    "camera",
    "microphone",
    "midi",
    "clipboard-read",
    "clipboard-write",
    "accelerometer",
    "gyroscope",
    "magnetometer",
    "idle-detection",
    "local-network-access",
    "display-capture",
    "window-management",
    "nfc",
];

/// Build one `Browser.setPermission` parameter object per denied permission.
///
/// `origin` scopes the denial to that committed origin when supplied; an empty
/// origin denies the permission for the whole browser context.
pub fn denial_params(origin: Option<&str>, browser_context_id: Option<&str>) -> Vec<Value> {
    DENIED_PERMISSIONS
        .iter()
        .map(|name| {
            let mut params = json!({
                "permission": {"name": name},
                "setting": "denied",
            });
            if let Some(origin) = origin {
                params["origin"] = json!(origin);
            }
            if let Some(context) = browser_context_id {
                params["browserContextId"] = json!(context);
            }
            params
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_the_full_sensitive_set() {
        assert_eq!(DENIED_PERMISSIONS.len(), 15);
        for required in [
            "geolocation",
            "camera",
            "microphone",
            "local-network-access",
            "display-capture",
        ] {
            assert!(DENIED_PERMISSIONS.contains(&required), "{required}");
        }
    }

    #[test]
    fn denial_params_use_browser_set_permission_shape() {
        let params = denial_params(Some("https://example.com"), Some("CTX"));
        assert_eq!(params.len(), DENIED_PERMISSIONS.len());
        for value in &params {
            assert_eq!(value["setting"], json!("denied"));
            assert!(value["permission"]["name"].is_string());
            assert_eq!(value["origin"], json!("https://example.com"));
            assert_eq!(value["browserContextId"], json!("CTX"));
        }
        let scoped = denial_params(None, None);
        assert!(scoped[0].get("origin").is_none());
        assert!(scoped[0].get("browserContextId").is_none());
    }
}

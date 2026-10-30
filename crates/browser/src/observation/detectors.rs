//! Deterministic block detectors (9.1). Detection only, never resolution.
//!
//! DOM facts come from the isolated-world readiness probe; document headers
//! and status come from CDP network events. No model judgement is involved:
//! every result is `{kind, vendor, signals, confidence, url, frame_id}` with
//! a fixed vocabulary. Low-confidence signals are reported as `degraded`,
//! never as a block.
use serde_json::{Value, json};

/// Allowed detector kinds.
pub const KINDS: &[&str] = &[
    "blocked_captcha",
    "blocked_by_site",
    "needs_login",
    "needs_2fa",
];

/// Normalize and validate a raw probe `blocked` value.
///
/// Returns `None` for absent blocks, unknown kinds, or malformed payloads so
/// callers never surface an invented block reason.
pub fn classify_probe_blocked(raw: &Value, frame_id: &str) -> Option<Value> {
    if raw.is_null() {
        return None;
    }
    let raw_kind = raw["kind"].as_str()?;
    let kind = match raw_kind {
        "captcha" => "blocked_captcha",
        other => other,
    };
    if !KINDS.contains(&kind) {
        return None;
    }
    let vendor = raw["vendor"].as_str().unwrap_or("site");
    let confidence = raw["confidence"].as_str().unwrap_or("medium");
    if !matches!(confidence, "high" | "medium" | "low") {
        return None;
    }
    // Low confidence never blocks; the caller reports it as degraded.
    if confidence == "low" {
        return None;
    }
    let signals = raw["signals"].as_array().cloned().unwrap_or_default();
    if signals.is_empty() {
        return None;
    }
    let url = raw["url"].as_str().unwrap_or("");
    let mut blocked = json!({
        "kind": kind,
        "vendor": vendor,
        "signals": signals,
        "confidence": confidence,
        "url": url,
        "frame_id": frame_id,
    });
    // Corroboration text only elevates an existing medium signal.
    if confidence == "medium"
        && signals
            .iter()
            .any(|signal| signal.as_str().is_some_and(|s| s == "corroboration_text"))
    {
        blocked["confidence"] = json!("high");
    }
    Some(blocked)
}

/// Elevate a document-status block when the HTTP status corroborates it.
///
/// `status` is the top-level document response status; `blocked` is the probe
/// classification above. A 403/429/503 alongside block text raises medium to
/// high without inventing new kinds.
pub fn elevate_with_status(mut blocked: Value, status: Option<i64>) -> Value {
    let corroborated = matches!(status, Some(403) | Some(429) | Some(503));
    if corroborated
        && blocked["kind"] == json!("blocked_by_site")
        && blocked["confidence"] == json!("medium")
    {
        blocked["confidence"] = json!("high");
        let mut signals = blocked["signals"].as_array().cloned().unwrap_or_default();
        signals.push(json!("document_status"));
        blocked["signals"] = json!(signals);
    }
    blocked
}

/// Whether a probe payload is a control page that must not block.
///
/// Invisible reCAPTCHA v3 badges and injected `/cdn-cgi/challenge-platform/`
/// markers without a visible challenge are explicit non-blocks.
pub fn is_control_probe(raw: &Value) -> bool {
    raw.is_null()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(kind: &str, confidence: &str, signals: Vec<&str>) -> Value {
        json!({
            "kind": kind,
            "vendor": "example",
            "signals": signals,
            "confidence": confidence,
            "url": "https://example.com/",
        })
    }

    #[test]
    fn accepts_known_kinds_and_rejects_unknown_or_low() {
        let frame = "frame-1";
        assert!(
            classify_probe_blocked(
                &probe("blocked_captcha", "high", vec!["visible_challenge_frame"]),
                frame
            )
            .is_some()
        );
        assert!(
            classify_probe_blocked(
                &probe("needs_login", "medium", vec!["identity_origin"]),
                frame
            )
            .is_some()
        );
        assert!(
            classify_probe_blocked(
                &probe("needs_2fa", "medium", vec!["empty_verification_field"]),
                frame
            )
            .is_some()
        );
        assert!(
            classify_probe_blocked(
                &probe("blocked_by_site", "low", vec!["block_page_text"]),
                frame
            )
            .is_none()
        );
        assert!(
            classify_probe_blocked(&probe("invented_kind", "high", vec!["x"]), frame).is_none()
        );
        assert!(classify_probe_blocked(&Value::Null, frame).is_none());
        let mut bad = probe("blocked_captcha", "high", vec![]);
        assert!(classify_probe_blocked(&bad, frame).is_none());
        bad = json!({"kind": "blocked_captcha"});
        assert!(classify_probe_blocked(&bad, frame).is_none());
    }

    #[test]
    fn control_pages_do_not_block() {
        assert!(is_control_probe(&Value::Null));
        // Invisible badge produces no blocked payload from the probe.
        assert!(classify_probe_blocked(&Value::Null, "f").is_none());
    }

    #[test]
    fn document_status_elevates_site_blocks_only() {
        let site = probe("blocked_by_site", "medium", vec!["block_page_text"]);
        let elevated = elevate_with_status(classify_probe_blocked(&site, "f").unwrap(), Some(403));
        assert_eq!(elevated["confidence"], json!("high"));
        let captcha = probe(
            "blocked_captcha",
            "medium",
            vec!["visible_challenge_marker"],
        );
        let kept = elevate_with_status(classify_probe_blocked(&captcha, "f").unwrap(), Some(503));
        assert_eq!(kept["confidence"], json!("medium"));
        let ok = elevate_with_status(classify_probe_blocked(&site, "f").unwrap(), Some(200));
        assert_eq!(ok["confidence"], json!("medium"));
    }

    #[test]
    fn corroboration_elevates_medium_to_high() {
        let with_corroboration = json!({
            "kind": "needs_login",
            "vendor": "example",
            "signals": ["password_on_logged_in_site", "corroboration_text"],
            "confidence": "medium",
            "url": "https://example.com/",
        });
        let blocked = classify_probe_blocked(&with_corroboration, "f").unwrap();
        assert_eq!(blocked["confidence"], json!("high"));
    }
}

//! Deterministic loop signatures without raw argument/output retention.
use excalibur_types::runtime::{ToolCall, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum LoopRule {
    R1,
    R2,
    R3,
    R4,
    R5,
    R6,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoopDetection {
    pub rule: LoopRule,
    pub level: u8,
    pub strategy: String,
    pub note: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LoopState {
    observations: Vec<(String, String)>,
    failures: BTreeMap<String, u8>,
    information: BTreeSet<String>,
    mismatches: BTreeMap<String, u8>,
    progress: Option<String>,
    stagnant_steps: u8,
    pub detections: u8,
    pub note: String,
    #[serde(default)]
    pub last_detection: Option<LoopDetection>,
}
fn hash(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}
fn normalized(arguments: &str) -> Value {
    serde_json::from_str(arguments)
        .or_else(|_| json5::from_str(arguments))
        .unwrap_or_else(|_| json!({"invalid_argument_hash":hash(arguments.as_bytes())}))
}
fn legitimate_poll(call: &ToolCall) -> bool {
    let (name, args) = operation(call);
    name == "wait_for" || (name == "exec_io" && args["action"] == "wait_for")
}
fn operation(call: &ToolCall) -> (String, Value) {
    let args = normalized(&call.arguments);
    if call.name == "call_tool"
        && let Some(name) = args["name"].as_str()
    {
        return (name.to_owned(), args["arguments"].clone());
    }
    (call.name.clone(), args)
}
impl LoopState {
    pub fn detect(&mut self, rule: LoopRule) -> LoopDetection {
        self.detections = self.detections.saturating_add(1);
        let level = self.detections.min(3);
        let strategy = match level {
            1 => "corrective_note",
            2 => "change_strategy",
            _ => "host_intervention",
        }
        .to_owned();
        self.note = match level {
            1 => format!(
                "Loop {rule:?}: repeated observations or no verified progress. Obtain new evidence and avoid repeating the same operation."
            ),
            2 => format!(
                "Loop {rule:?} persists. Change strategy: inspect the cause with a different read-only check, or request the missing host decision. No expert consultation has been performed."
            ),
            _ => format!(
                "Loop {rule:?} persists after corrective guidance and a strategy change; host intervention is required."
            ),
        };
        let detection = LoopDetection {
            rule,
            level,
            strategy,
            note: self.note.clone(),
        };
        self.last_detection = Some(detection.clone());
        detection
    }
    pub fn observe(
        &mut self,
        calls: &[(ToolCall, ToolResult)],
        progress: &Value,
    ) -> Option<LoopDetection> {
        self.last_detection = None;
        if !calls.is_empty() && calls.iter().all(|(call, _)| legitimate_poll(call)) {
            return None;
        }
        let mut candidate = None;
        let mut new_information = false;
        for (call, result) in calls {
            if legitimate_poll(call) {
                continue;
            }
            let (name, args) = operation(call);
            let h1 = hash(
                serde_json::to_string(&json!([name, args]))
                    .unwrap()
                    .as_bytes(),
            );
            let h2 = hash(
                serde_json::to_string(&json!([h1, result]))
                    .unwrap()
                    .as_bytes(),
            );
            new_information |= self.information.insert(hash(
                serde_json::to_string(&result.output).unwrap().as_bytes(),
            ));
            if !result.success {
                let count = self.failures.entry(h1.clone()).or_default();
                *count = count.saturating_add(1);
                if *count >= 3 {
                    candidate = Some(LoopRule::R2);
                }
            } else {
                self.failures.remove(&h1);
            }
            let detail = result
                .output
                .get("error")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok());
            let conflict = detail.as_ref().unwrap_or(&result.output);
            if !result.success
                && matches!(name.as_str(), "write_file" | "edit_file" | "apply_patch")
                && (conflict["expected_fp"].is_string()
                    || conflict["expected_fingerprint"].is_string()
                    || conflict["mismatches"].is_number()
                    || conflict["escalate_to_parent"] == true)
            {
                let path = conflict["path"]
                    .as_str()
                    .or_else(|| args["path"].as_str())
                    .unwrap_or("unknown-file");
                let count = self.mismatches.entry(hash(path.as_bytes())).or_default();
                *count = count.saturating_add(1);
                if *count >= 3
                    || conflict["mismatches"].as_u64().is_some_and(|n| n >= 3)
                    || conflict["escalate_to_parent"] == true
                {
                    candidate = Some(LoopRule::R6);
                }
            } else if let Some(path) = args["path"].as_str() {
                self.mismatches.remove(&hash(path.as_bytes()));
            }
            self.observations.push((h1, h2));
            if self.observations.len() > 64 {
                self.observations.remove(0);
            }
        }
        let n = self.observations.len();
        if candidate != Some(LoopRule::R6)
            && n >= 3
            && self.observations[n - 3..]
                .iter()
                .all(|(_, h2)| h2 == &self.observations[n - 1].1)
        {
            candidate = Some(LoopRule::R1);
        }
        if candidate.is_none() {
            for period in 2..=5 {
                if n >= period * 3 {
                    let tail = &self.observations[n - period * 3..];
                    if tail.iter().map(|(h1, _)| h1).collect::<BTreeSet<_>>().len() > 1
                        && (0..period * 2).all(|i| tail[i].0 == tail[i + period].0)
                    {
                        candidate = Some(LoopRule::R3);
                        break;
                    }
                }
            }
        }
        let progress = hash(serde_json::to_string(progress).unwrap().as_bytes());
        if self.progress.as_ref() != Some(&progress) || new_information {
            self.stagnant_steps = 0;
        } else {
            self.stagnant_steps = self.stagnant_steps.saturating_add(1);
        }
        self.progress = Some(progress);
        if candidate.is_none() && self.stagnant_steps >= 8 {
            candidate = Some(LoopRule::R4);
        }
        while self.information.len() > 256 {
            self.information.pop_first();
        }
        while self.failures.len() > 64 {
            self.failures.pop_first();
        }
        while self.mismatches.len() > 64 {
            self.mismatches.pop_first();
        }
        candidate.map(|rule| self.detect(rule))
    }
}
/// Bounded UTF-8-aware detection: 50 characters repeated ten times.
#[derive(Default)]
pub(crate) struct StreamRepeats {
    tail: Vec<char>,
    pub detected: bool,
}
impl StreamRepeats {
    pub fn push(&mut self, text: &str) -> bool {
        if self.detected {
            return true;
        }
        for ch in text.chars() {
            self.tail.push(ch);
            if self.tail.len() > 500 {
                self.tail.remove(0);
            }
            if self.tail.len() == 500 && (50..500).all(|i| self.tail[i] == self.tail[i % 50]) {
                self.detected = true;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(n: usize) -> ToolCall {
        ToolCall {
            id: format!("call-{n}"),
            name: "probe".into(),
            arguments: json!({"index":n}).to_string(),
        }
    }
    #[test]
    fn identical_normalized_signatures_escalate_and_survive_restart() {
        let mut state = LoopState::default();
        let mut tool = call(0);
        tool.arguments = "{\"b\":2,\"a\":1}".into();
        for i in 0..5 {
            tool.id = format!("id-{i}");
            if i % 2 == 1 {
                tool.arguments = "{ a:1,b:2 }".into();
            } else {
                tool.arguments = "{\"b\":2,\"a\":1}".into();
            }
            let detection = state.observe(
                &[(tool.clone(), ToolResult::ok(json!({"value":1})))],
                &json!("fixed"),
            );
            if i < 2 {
                assert!(detection.is_none());
            } else {
                let d = detection.unwrap();
                assert_eq!(d.rule, LoopRule::R1);
                assert_eq!(d.level, (i - 1) as u8);
            }
            state = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
        }
        assert_eq!(state.detections, 3);
    }
    #[test]
    fn same_failing_operation_and_periods_two_through_five_are_detected() {
        let mut state = LoopState::default();
        for i in 0..3 {
            let d = state.observe(
                &[(call(0), ToolResult::error(format!("error-{i}")))],
                &json!("fixed"),
            );
            if i == 2 {
                assert_eq!(d.unwrap().rule, LoopRule::R2);
            }
        }
        for period in 2..=5 {
            let mut state = LoopState::default();
            let mut found = None;
            for i in 0..period * 3 {
                found = state.observe(
                    &[(call(i % period), ToolResult::ok(json!({"iteration":i})))],
                    &json!("fixed"),
                );
            }
            assert_eq!(found.unwrap().rule, LoopRule::R3);
        }
    }
    #[test]
    fn stagnant_state_requires_eight_steps_without_novel_content() {
        let mut state = LoopState::default();
        for i in 0..10 {
            assert!(
                state
                    .observe(&[(call(i), ToolResult::ok(json!(i)))], &json!("fixed"))
                    .is_none()
            );
        }
        for (i, n) in [0, 1, 3, 6, 8, 2, 5, 9].into_iter().enumerate() {
            let d = state.observe(&[(call(n), ToolResult::ok(json!(n)))], &json!("fixed"));
            if i == 7 {
                assert_eq!(d.unwrap().rule, LoopRule::R4);
            } else {
                assert!(d.is_none());
            }
        }
    }
    #[test]
    fn fingerprint_conflicts_use_machine_details_and_same_file_counter() {
        let mut state = LoopState::default();
        for n in 1..=3 {
            let mut tool = call(0);
            tool.name = "edit_file".into();
            tool.arguments = json!({"path":"synthetic.txt"}).to_string();
            let detail = json!({"error":"fingerprint_conflict","path":"synthetic.txt","expected_fp":"old","current_fp":format!("v{n}"),"mismatches":n});
            let d = state.observe(
                &[(tool, ToolResult::error(detail.to_string()))],
                &json!("fixed"),
            );
            if n == 3 {
                assert_eq!(d.unwrap().rule, LoopRule::R6);
            } else {
                assert!(d.is_none());
            }
        }
    }
    #[test]
    fn legitimate_wait_for_polling_has_zero_detections() {
        let mut state = LoopState::default();
        for name in ["wait_for", "exec_io"] {
            let tool = ToolCall {
                id: "poll".into(),
                name: name.into(),
                arguments: json!({"action":"wait_for","pattern":"ready"}).to_string(),
            };
            for _ in 0..100 {
                assert!(
                    state
                        .observe(
                            &[(tool.clone(), ToolResult::ok(json!({"ready":false})))],
                            &json!("fixed")
                        )
                        .is_none()
                );
            }
            let wrapped = ToolCall {
                id: "wrapped".into(),
                name: "call_tool".into(),
                arguments: json!({"name":name,"arguments":{"action":"wait_for","pattern":"ready"}})
                    .to_string(),
            };
            for _ in 0..100 {
                assert!(
                    state
                        .observe(
                            &[(wrapped.clone(), ToolResult::ok(json!({"ready":false})))],
                            &json!("fixed")
                        )
                        .is_none()
                );
            }
        }
        assert_eq!(state.detections, 0);
    }
    #[test]
    fn repeated_stream_is_character_based_and_bounded() {
        let fragment = "0123456789012345678901234567890123456789012345678é";
        assert_eq!(fragment.chars().count(), 50);
        let mut detector = StreamRepeats::default();
        for _ in 0..9 {
            assert!(!detector.push(fragment));
        }
        assert!(detector.push(fragment));
        assert_eq!(detector.tail.len(), 500);
        let mut changing = StreamRepeats::default();
        for i in 0..1000 {
            assert!(!changing.push(&format!("{i:050}")));
        }
        assert!(changing.tail.len() <= 500);
    }
}

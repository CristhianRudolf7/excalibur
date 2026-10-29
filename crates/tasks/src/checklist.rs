//! Checklist projection through the existing fingerprint, lease and edit-history pipeline.
use crate::{Result, TaskStore, fail, new_id};
use excalibur_types::runtime::{ToolCall, ToolRuntime};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{path::PathBuf, sync::Arc};
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChecklistEntry {
    pub check_id: String,
    pub label: String,
    pub passed: bool,
    pub fresh: bool,
    pub recorded_at: Option<u64>,
    pub commit: Option<String>,
    pub evidence_id: Option<String>,
}
pub const CHECKLIST_MARKER: &str = "<!-- excalibur:checklist -->";
const END_MARKER: &str = "<!-- /excalibur:checklist -->";
fn span(text: &str) -> Result<Option<(usize, usize)>> {
    let starts = text
        .match_indices(CHECKLIST_MARKER)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let ends = text
        .match_indices(END_MARKER)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([start, end], []) => Ok(Some((*start, *end + CHECKLIST_MARKER.len()))),
        ([start], [end]) if end > start => Ok(Some((*start, *end + END_MARKER.len()))),
        _ => Err(fail(
            "ambiguous or incomplete checklist markers; host repair required",
        )),
    }
}
/// Normalize exactly the host-owned section. User text outside it remains evidence.
/// If absent, appending the managed block is canonicalized away as well.
pub fn canonical_content(bytes: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(bytes).map_err(|_| fail("checklist must be UTF-8"))?;
    match span(text)? {
        Some((start, end)) => {
            let mut out = text.as_bytes()[..start].to_vec();
            out.extend_from_slice(&text.as_bytes()[end..]);
            Ok(out)
        }
        None => Ok(bytes.to_vec()),
    }
}
pub fn render(existing: &[u8], entries: &[ChecklistEntry]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(existing).map_err(|_| fail("checklist must be UTF-8"))?;
    let mut section = format!("{CHECKLIST_MARKER}\n");
    for e in entries {
        let safe = |s: &str| s.replace(['\r', '\n'], " ").replace(['[', ']'], "");
        let state = if !e.fresh {
            "not executed with the current version"
        } else if e.passed {
            "passed"
        } else {
            "failed"
        };
        section.push_str(&format!(
            "- [{}] {} — {}",
            if e.passed && e.fresh { "x" } else { " " },
            safe(&e.label),
            state
        ));
        if let Some(at) = e.recorded_at {
            section.push_str(&format!(" · unix {at}"));
        }
        if let Some(commit) = &e.commit {
            section.push_str(&format!(" · commit {}", safe(commit)));
        }
        if let Some(id) = &e.evidence_id {
            section.push_str(&format!(" · evidence {}", safe(id)));
        }
        section.push('\n');
    }
    section.push_str(END_MARKER);
    match span(text)? {
        Some((start, end)) => {
            Ok(format!("{}{}{}", &text[..start], section, &text[end..]).into_bytes())
        }
        None => {
            // The separator is part of the managed block. No pre-existing byte changes.
            Ok(format!("{text}{section}").into_bytes())
        }
    }
}
#[derive(Clone)]
pub struct ChecklistProjection {
    pub runtime: Arc<dyn ToolRuntime>,
    pub path: PathBuf,
}
impl ChecklistProjection {
    pub fn new(runtime: Arc<dyn ToolRuntime>, path: PathBuf) -> Self {
        Self { runtime, path }
    }
    /// Discover the trusted adapter's actual resource path without reading content.
    pub async fn absolute_path(&self, run: &str) -> Result<PathBuf> {
        let call = ToolCall {
            id: new_id("checklist-path"),
            name: "read_file".into(),
            arguments: json!({"path":self.path}).to_string(),
        };
        let prepared = self.runtime.prepare(run, call).await.map_err(fail)?;
        prepared
            .resources
            .iter()
            .find(|r| !r.write)
            .map(|r| PathBuf::from(&r.key))
            .filter(|p| p.is_absolute())
            .ok_or_else(|| fail("checklist adapter must expose a canonical absolute read resource"))
    }
    /// Every replacement reads a verified preimage and supplies its exact fp.
    /// The LocalTools write pipeline preserves that preimage in file_versions.
    pub async fn update(
        &self,
        store: &TaskStore,
        run: &str,
        entries: &[ChecklistEntry],
    ) -> Result<bool> {
        for attempt in 0..2 {
            let read = ToolCall {
                id: new_id("checklist-read"),
                name: "read_file".into(),
                arguments: json!({"path":self.path}).to_string(),
            };
            let prepared = self.runtime.prepare(run, read).await.map_err(fail)?;
            let result = self
                .runtime
                .execute(run, &new_id("checklist-read"), &prepared)
                .await;
            let (old, fp) = if result.success {
                let fp = result.output["fp"]
                    .as_str()
                    .ok_or_else(|| fail("checklist read lacks verified fingerprint"))?
                    .to_owned();
                let bytes = store
                    .database()
                    .read_blob(fp.parse()?, 64 * 1024 * 1024)
                    .await?;
                if let Some(mut inspector) = self.runtime.durable_blob_inspector() {
                    for chunk in bytes.chunks(65536) {
                        inspector.inspect(chunk).map_err(fail)?;
                    }
                    inspector.finish().map_err(fail)?;
                }
                (bytes, Some(fp))
            } else if result.output["error"]
                .as_str()
                .is_some_and(|e| e.contains("file does not exist"))
            {
                (vec![], None)
            } else {
                return Err(fail(format!("checklist read failed: {}", result.output)));
            };
            if entries.is_empty() && old.is_empty() {
                return Ok(false);
            }
            let next = render(&old, entries)?;
            if next == old {
                return Ok(false);
            }
            let content =
                String::from_utf8(next).map_err(|_| fail("checklist content is not UTF-8"))?;
            let mut args = json!({"path":self.path,"content":content});
            if let Some(fp) = fp {
                args["fp"] = json!(fp);
            }
            let call = ToolCall {
                id: new_id("checklist-write"),
                name: "write_file".into(),
                arguments: args.to_string(),
            };
            let prepared = match self.runtime.prepare(run, call).await {
                Ok(v) => v,
                Err(e) if attempt == 0 && e.contains("fingerprint") => continue,
                Err(e) => return Err(fail(e)),
            };
            let result = self
                .runtime
                .execute(run, &new_id("checklist-write"), &prepared)
                .await;
            if result.success {
                return Ok(true);
            }
            if attempt != 0 || !result.output.to_string().contains("fingerprint") {
                return Err(fail(format!("checklist write failed: {}", result.output)));
            }
        }
        Err(fail(
            "checklist changed concurrently; preserved user content, no overwrite",
        ))
    }
}

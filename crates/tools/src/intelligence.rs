//! Host-configured project graph and language services use the file tool authority.
use crate::{
    LocalTools, Permission,
    files::{hash, required_str},
    monitor::{ChangeObserver, ProjectChange},
};
use excalibur_code_intelligence::{CodeGraph, GraphQuery, Language};
use excalibur_types::runtime::{BoxFuture, ToolCall};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Clone)]
pub struct ProjectIntelligence {
    pub graph: CodeGraph,
    pub project_id: String,
    pub lsp: Option<Arc<excalibur_code_intelligence::LspManager>>,
    pub root: PathBuf,
    redactor: excalibur_vault::Redactor,
}
impl ProjectIntelligence {
    pub fn new(graph: CodeGraph, project_id: String, root: PathBuf) -> Result<Self, String> {
        let canonical = root.canonicalize().map_err(|e| e.to_string())?;
        if canonical != root || project_id.is_empty() {
            return Err("canonical project root and project identity required".into());
        }
        Ok(Self {
            graph,
            project_id,
            root,
            lsp: None,
            redactor: excalibur_vault::Redactor::default(),
        })
    }
    pub fn with_lsp(mut self, lsp: Arc<excalibur_code_intelligence::LspManager>) -> Self {
        self.lsp = Some(lsp);
        self
    }
    pub async fn index(&self, path: &Path, content: Option<&[u8]>) -> Result<Value, String> {
        let path = if path.is_absolute() {
            path.strip_prefix(&self.root)
                .map_err(|_| "index path outside project")?
        } else {
            path
        };
        if Language::from_path(path).is_none() {
            return Ok(json!({"indexed":false,"reason":"unsupported_language"}));
        }
        match content {
            Some(bytes) => {
                if self.redactor.contains_secret(bytes) {
                    self.graph
                        .remove_file(path)
                        .await
                        .map_err(|e| e.to_string())?;
                    return Ok(json!({"indexed":false,"reason":"protected_content"}));
                }
                if bytes.len() > 8 * 1024 * 1024 {
                    self.graph
                        .remove_file(path)
                        .await
                        .map_err(|e| e.to_string())?;
                    return Ok(json!({"indexed":false,"reason":"source_file_exceeds_8_mib"}));
                }
                let Ok(text) = std::str::from_utf8(bytes) else {
                    self.graph
                        .remove_file(path)
                        .await
                        .map_err(|e| e.to_string())?;
                    return Ok(json!({"indexed":false,"reason":"binary_content"}));
                };
                let report = self
                    .graph
                    .index_path_content(path, text)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut result = json!({"indexed":true,"graph":report});
                if let Some(lsp) = &self.lsp {
                    let language = Language::from_path(path).unwrap().id();
                    if lsp.configured(&self.project_id, language).await {
                        result["lsp"] = match lsp
                            .sync_document(&self.project_id, language, &self.root.join(path), text)
                            .await
                        {
                            Ok(diagnostics) => json!(diagnostics),
                            Err(error) => json!({"fresh":false,"error":error.to_string()}),
                        };
                        if report.mode != excalibur_code_intelligence::ParseMode::Unchanged {
                            result["semantic_enrichment"] = match self
                                .graph
                                .enrich_lsp(lsp, &self.project_id, language, &self.root, path)
                                .await
                            {
                                Ok(report) => json!(report),
                                Err(error) => json!({"unavailable":error.to_string()}),
                            };
                        }
                    }
                }
                Ok(result)
            }
            None => {
                Ok(json!({"removed":self.graph.remove_file(path).await.map_err(|e|e.to_string())?}))
            }
        }
    }
}
impl ChangeObserver for ProjectIntelligence {
    fn changed<'a>(
        &'a self,
        change: &'a ProjectChange,
        content: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.index(&change.path, content.as_deref()).await?;
            Ok(())
        })
    }
}
impl LocalTools {
    pub fn with_intelligence(mut self, services: Arc<ProjectIntelligence>) -> Result<Self, String> {
        if services.root != self.root {
            return Err("language services belong to another project".into());
        }
        let mut services = (*services).clone();
        services.redactor = self.policy.redactor.clone();
        services.graph = services
            .graph
            .clone()
            .with_redactor(self.policy.redactor.clone());
        self.intelligence = Some(Arc::new(services));
        Ok(self)
    }
    pub fn project_intelligence(&self) -> Option<Arc<ProjectIntelligence>> {
        self.intelligence.clone()
    }
    pub(crate) async fn refresh_graph(&self) -> Result<(), String> {
        let services = self
            .intelligence
            .as_ref()
            .ok_or("project code intelligence is not active")?;
        let files = self.authorized_project_files()?;
        for path in services
            .graph
            .indexed_paths()
            .await
            .map_err(|e| e.to_string())?
        {
            if !files.contains_key(Path::new(&path)) {
                services.index(Path::new(&path), None).await?;
            }
        }
        for (path, bytes) in files {
            services.index(&path, Some(&bytes)).await?;
        }
        Ok(())
    }
    pub(crate) async fn query_graph(&self, args: &Value) -> Result<Value, String> {
        self.check_permission(Permission::Read)?;
        self.refresh_graph().await?;
        let services = self
            .intelligence
            .as_ref()
            .ok_or("project code intelligence is not active")?;
        if args["operation"] == "impacted_tests" {
            let paths = args["paths"]
                .as_array()
                .ok_or("impacted tests require paths")?
                .iter()
                .map(|p| self.resolve(p.as_str().ok_or("invalid path")?))
                .collect::<Result<Vec<_>, String>>()?;
            return Ok(
                json!({"tests":services.graph.impacted_tests(&paths).await.map_err(|e|e.to_string())?}),
            );
        }
        let mut query = args.clone();
        if query.get("limit").is_none() {
            query["limit"] = json!(100);
        }
        if query["operation"] == "symbols" && query.get("query").is_none() {
            query["query"] = json!("");
        }
        if let Some(path) = args["path"].as_str() {
            query["path"] = json!(self.resolve(path)?);
        }
        let query: GraphQuery =
            serde_json::from_value(query).map_err(|e| format!("invalid graph query: {e}"))?;
        Ok(json!({"graph":services.graph.query(query).await.map_err(|e|e.to_string())?}))
    }
    pub(crate) async fn diagnostics_before(
        &self,
        edits: &[crate::files::FileEdit],
    ) -> Result<Value, String> {
        let mut baseline = serde_json::Map::new();
        if let Some(services) = &self.intelligence {
            for edit in edits {
                let bytes = match self.current_entry(&edit.path)? {
                    Some(entry) if entry.kind == excalibur_workspace::EntryKind::File => {
                        Some(entry.content)
                    }
                    _ => None,
                };
                let report = services
                    .index(Path::new(&edit.path), bytes.as_deref())
                    .await?;
                baseline.insert(edit.path.clone(), report);
            }
        }
        Ok(Value::Object(baseline))
    }
    pub(crate) async fn intelligence_after(
        &self,
        path: &str,
        baseline: &Value,
    ) -> Result<Value, String> {
        let Some(services) = &self.intelligence else {
            return Ok(json!({"status":"not_configured"}));
        };
        let bytes = match self.current_entry(path)? {
            Some(entry) if entry.kind == excalibur_workspace::EntryKind::File => {
                Some(entry.content)
            }
            _ => None,
        };
        let mut report = services.index(Path::new(path), bytes.as_deref()).await?;
        let old = baseline
            .pointer("/graph/syntax_errors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let current = report
            .pointer("/graph/syntax_errors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let new_syntax = new_diagnostics(&old, &current);
        report["graph"]["syntax_errors"] = json!(new_syntax);
        let old_lsp = baseline
            .pointer("/lsp/diagnostics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let current_lsp = report
            .pointer("/lsp/diagnostics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let errors = new_diagnostics(&old_lsp, &current_lsp)
            .into_iter()
            .filter(|d| d["severity"] == 1)
            .collect::<Vec<_>>();
        if report["lsp"].is_object() {
            report["lsp"]["diagnostics"] = json!(errors);
            report["lsp"]["new_errors"] = json!(errors);
        }
        report["diagnostics"] = json!({"syntax":new_syntax,"lsp":errors});
        Ok(report)
    }
}
/// Diagnostics are a multiset of message/code/source, independent of shifting line positions.
fn new_diagnostics(old: &[Value], current: &[Value]) -> Vec<Value> {
    fn key(v: &Value) -> String {
        json!([
            v["message"],
            v["code"],
            v["source"],
            v["kind"],
            v["severity"]
        ])
        .to_string()
    }
    let mut counts = BTreeMap::<String, usize>::new();
    for item in old {
        *counts.entry(key(item)).or_default() += 1
    }
    current
        .iter()
        .filter(|item| {
            let count = counts.entry(key(item)).or_default();
            if *count > 0 {
                *count -= 1;
                false
            } else {
                true
            }
        })
        .cloned()
        .collect()
}
/// Apply LSP UTF-16 ranges without splitting Unicode scalars or accepting overlapping edits.
pub fn apply_text_edits(text: &str, edits: &[Value]) -> Result<String, String> {
    fn offset(text: &str, pos: &Value) -> Result<usize, String> {
        let line = pos["line"].as_u64().ok_or("position line required")? as usize;
        let character = pos["character"]
            .as_u64()
            .ok_or("position character required")? as usize;
        let mut start = 0;
        for _ in 0..line {
            start += text[start..]
                .find('\n')
                .ok_or("position line outside document")?
                + 1;
        }
        let tail = &text[start..];
        let content = tail.split('\n').next().unwrap_or("").trim_end_matches('\r');
        let mut units = 0;
        for (index, ch) in content.char_indices() {
            if units == character {
                return Ok(start + index);
            }
            units += ch.len_utf16();
            if units > character {
                return Err("position splits UTF-16 scalar".into());
            }
        }
        if units == character {
            Ok(start + content.len())
        } else {
            Err("position outside line".into())
        }
    }
    let mut ranges = edits
        .iter()
        .map(|edit| {
            let range = &edit["range"];
            let begin = offset(text, &range["start"])?;
            let end = offset(text, &range["end"])?;
            if begin > end {
                return Err("reversed LSP range".into());
            }
            Ok((begin, end, required_str(edit, "newText")?.to_owned()))
        })
        .collect::<Result<Vec<_>, String>>()?;
    ranges.sort_by_key(|(begin, end, _)| (*begin, *end));
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 || pair[0].0 == pair[1].0 {
            return Err("overlapping or ambiguous LSP edits".into());
        }
    }
    let mut result = text.to_owned();
    for (begin, end, replacement) in ranges.into_iter().rev() {
        result.replace_range(begin..end, &replacement);
    }
    Ok(result)
}
impl LocalTools {
    async fn lsp_document(
        &self,
        path: &str,
    ) -> Result<(Arc<ProjectIntelligence>, String, String), String> {
        let services = self
            .intelligence
            .as_ref()
            .ok_or("project code intelligence is not active")?
            .clone();
        let language = Language::from_path(Path::new(path))
            .ok_or("language has no registered source adapter")?
            .id()
            .to_owned();
        let manager = services
            .lsp
            .as_ref()
            .ok_or("language server is not configured")?;
        if !manager.configured(&services.project_id, &language).await {
            return Err("language server is not configured for this project/language".into());
        }
        let bytes = self
            .content(path)?
            .ok_or("language server file does not exist")?;
        if self.policy.redactor.contains_secret(&bytes) {
            return Err("protected content cannot enter language server".into());
        }
        let text =
            String::from_utf8(bytes).map_err(|_| "language server requires UTF-8 document")?;
        manager
            .sync_document(
                &services.project_id,
                &language,
                &self.root.join(path),
                &text,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok((services, language, text))
    }
    pub(crate) async fn query_lsp(&self, args: &Value) -> Result<Value, String> {
        use excalibur_code_intelligence::{LspOperation, Position};
        self.check_permission(Permission::Read)?;
        if args["action"] == "workspace_symbols" {
            let services = self
                .intelligence
                .as_ref()
                .ok_or("project code intelligence is not active")?;
            let language = if let Some(language) = args["language"].as_str() {
                language.to_owned()
            } else {
                Language::from_path(Path::new(required_str(args, "path")?))
                    .ok_or("workspace symbol query requires a configured language")?
                    .id()
                    .to_owned()
            };
            let lsp = services
                .lsp
                .as_ref()
                .ok_or("language server is not configured")?;
            let reply = lsp
                .request(
                    &services.project_id,
                    &language,
                    LspOperation::WorkspaceSymbols {
                        query: args["query"].as_str().unwrap_or("").into(),
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            return Ok(json!({"lsp":reply}));
        }
        let path = self.resolve(required_str(args, "path")?)?;
        let (services, language, _) = self.lsp_document(&path).await?;
        let position = Position {
            line: u32::try_from(args["line"].as_u64().unwrap_or(0))
                .map_err(|_| "position overflow")?,
            character: u32::try_from(args["character"].as_u64().unwrap_or(0))
                .map_err(|_| "position overflow")?,
        };
        let full = self.root.join(&path);
        let operation = match args["action"].as_str() {
            Some("definition") => LspOperation::Definition {
                path: full,
                position,
            },
            Some("references") => LspOperation::References {
                path: full,
                position,
                include_declaration: true,
            },
            Some("document_symbols") => LspOperation::DocumentSymbols { path: full },
            Some("workspace_symbols") => LspOperation::WorkspaceSymbols {
                query: args["query"].as_str().unwrap_or("").into(),
            },
            Some("diagnostics") => LspOperation::Diagnostics { path: full },
            _ => return Err("unsupported LSP read operation".into()),
        };
        let reply = services
            .lsp
            .as_ref()
            .unwrap()
            .request(&services.project_id, &language, operation)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({"lsp":reply}))
    }
    pub(crate) async fn prepare_lsp_rename(
        &self,
        run: &str,
        call: &ToolCall,
        args: &Value,
    ) -> Result<Vec<crate::files::FileEdit>, String> {
        use excalibur_code_intelligence::{LspOperation, Position};
        let path = self.resolve(required_str(args, "path")?)?;
        let (mut allowed, mut current) = (BTreeSet::from([path.clone()]), BTreeMap::new());
        if let Some(paths) = args["paths"].as_array() {
            for path in paths {
                allowed.insert(self.resolve(path.as_str().ok_or("invalid rename path")?)?);
            }
        }
        let (services, language, _) = self.lsp_document(&path).await?;
        let manager = services.lsp.as_ref().unwrap();
        for path in &allowed {
            self.check_write_path(path)?;
            let bytes = self.content(path)?.ok_or("rename target does not exist")?;
            let text = std::str::from_utf8(&bytes).map_err(|_| "rename requires UTF-8 target")?;
            manager
                .sync_document(&services.project_id, &language, &self.root.join(path), text)
                .await
                .map_err(|e| e.to_string())?;
            current.insert(path.clone(), bytes);
        }
        let position = Position {
            line: u32::try_from(args["line"].as_u64().unwrap_or(0))
                .map_err(|_| "position overflow")?,
            character: u32::try_from(args["character"].as_u64().unwrap_or(0))
                .map_err(|_| "position overflow")?,
        };
        let reply = manager
            .request(
                &services.project_id,
                &language,
                LspOperation::Rename {
                    path: self.root.join(&path),
                    position,
                    new_name: required_str(args, "new_name")?.into(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        if reply.result.is_null() {
            return Err("language server returned no rename edit".into());
        }
        let mut edits = BTreeMap::<String, Vec<Value>>::new();
        if let Some(changes) = reply.result["changes"].as_object() {
            for (uri, value) in changes {
                edits.insert(
                    uri.clone(),
                    value
                        .as_array()
                        .ok_or("invalid LSP text edit list")?
                        .clone(),
                );
            }
        }
        if let Some(changes) = reply.result["documentChanges"].as_array() {
            for item in changes {
                if item.get("kind").is_some() {
                    return Err("server resource operations were not authorized; only text edits are supported".into());
                }
                let uri = required_str(&item["textDocument"], "uri")?;
                if let Some(version) = item["textDocument"]["version"].as_i64()
                    && reply.document_versions.get(uri).copied().map(i64::from) != Some(version)
                {
                    return Err("rename document version changed".into());
                }
                if edits.contains_key(uri) {
                    return Err("duplicate rename document edits".into());
                }
                edits.insert(
                    uri.into(),
                    item["edits"]
                        .as_array()
                        .ok_or("invalid LSP text edit list")?
                        .clone(),
                );
            }
        }
        let mut plan = excalibur_workspace::RevertPlan::default();
        for (uri, changes) in edits {
            let absolute = url::Url::parse(&uri)
                .map_err(|_| "invalid LSP URI")?
                .to_file_path()
                .map_err(|_| "LSP URI is not a local file")?;
            let path = self.resolve(absolute.to_str().ok_or("non UTF-8 rename path")?)?;
            if !allowed.contains(&path) {
                return Err("rename exceeds explicitly authorized paths".into());
            }
            let bytes = current
                .get(&path)
                .ok_or("rename target was not synchronized")?;
            if reply.document_hashes.get(&uri) != Some(&hash(bytes)) {
                return Err("rename target hash differs from synchronized document".into());
            }
            let text = std::str::from_utf8(bytes).map_err(|_| "rename requires UTF-8")?;
            let post = apply_text_edits(text, &changes)?.into_bytes();
            plan.files.push(excalibur_workspace::PlannedFile {
                path: absolute,
                pre: Some(hash(bytes)),
                post: Some(post),
                ..Default::default()
            });
        }
        self.prepare_restore(run, call, plan, &allowed).await
    }
}

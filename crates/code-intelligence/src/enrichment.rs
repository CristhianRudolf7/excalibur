//! Hash-guarded semantic enrichment; remote/out-of-root locations never enter a project graph.
use crate::*;
use excalibur_store::{ReadLane, rusqlite::params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrichmentReport {
    pub requests: usize,
    pub enriched_edges: usize,
    pub skipped_locations: usize,
    pub budget_exhausted: bool,
    pub unavailable: Vec<String>,
}
impl CodeGraph {
    /// At most32 semantic requests. Only synchronized, hash-current indexed targets qualify.
    pub async fn enrich_lsp(
        &self,
        manager: &LspManager,
        project: &str,
        language: &str,
        root: &Path,
        path: &Path,
    ) -> Result<EnrichmentReport> {
        let root = root.canonicalize()?;
        if manager.project_root(project, language).await? != root {
            return Err(invalid(
                "semantic enrichment root differs from LSP authority",
            ));
        }
        let source = manager.document_snapshot(project, language, path).await?;
        let relative = crate::graph::relative(path)?;
        let lookup = relative.clone();
        let (expected,refs,symbols)=self.db.read(ReadLane::Background,move|c|{
            let digest:String=c.query_row("SELECT hash FROM files WHERE path=?1",[&lookup],|r|r.get(0))?;
            let mut q=c.prepare("SELECT r.source_id,r.kind,r.start_byte FROM ci_references r JOIN nodes n ON n.id=r.source_id JOIN files f ON f.id=n.file_id WHERE f.path=?1 AND r.kind IN('call','reference') ORDER BY r.kind='call' DESC,r.start_byte,r.id LIMIT 32")?;
            let refs=q.query_map([&lookup],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,usize>(2)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let mut q=c.prepare("SELECT n.id,n.name,n.start_offset,n.end_offset FROM nodes n JOIN files f ON f.id=n.file_id WHERE f.path=?1 AND n.kind IN('function','method','type','test') ORDER BY n.start_offset LIMIT 16")?;
            let symbols=q.query_map([lookup],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,usize>(2)?,r.get::<_,usize>(3)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;Ok((digest,refs,symbols))
        }).await?;
        if source.hash != expected
            || verified_hash(root.clone(), relative.clone()).await? != expected
        {
            return Err(invalid("source changed before semantic enrichment"));
        }
        let mut report = EnrichmentReport::default();
        for (source_id, kind, offset) in refs {
            if report.requests == 32 {
                report.budget_exhausted = true;
                break;
            }
            let position = utf16_position(&source.text, offset)?;
            report.requests += 1;
            let reply = match manager
                .request(
                    project,
                    language,
                    LspOperation::Definition {
                        path: path.into(),
                        position,
                    },
                )
                .await
            {
                Ok(reply) => reply,
                Err(_) => {
                    report
                        .unavailable
                        .push("definition enrichment unavailable".into());
                    break;
                }
            };
            for (uri, position) in locations(&reply.result) {
                match self.semantic_target(&root, &uri, position, &reply).await? {
                    Some((target, target_hash)) => {
                        self.enrich_edge(&source_id, &target, "reference", &expected, &target_hash)
                            .await?;
                        if kind == "call" {
                            self.enrich_edge(&source_id, &target, "call", &expected, &target_hash)
                                .await?;
                        }
                        report.enriched_edges += 1;
                    }
                    None => report.skipped_locations += 1,
                }
            }
        }
        for (target_id, name, start, end) in symbols {
            if report.requests == 32 {
                report.budget_exhausted = true;
                break;
            }
            let Some(found) = source
                .text
                .get(start..end)
                .and_then(|text| text.find(&name))
            else {
                continue;
            };
            let position = utf16_position(&source.text, start + found)?;
            report.requests += 1;
            let reply = match manager
                .request(
                    project,
                    language,
                    LspOperation::References {
                        path: path.into(),
                        position,
                        include_declaration: false,
                    },
                )
                .await
            {
                Ok(reply) => reply,
                Err(_) => {
                    report
                        .unavailable
                        .push("reference enrichment unavailable".into());
                    break;
                }
            };
            for (uri, position) in locations(&reply.result) {
                match self.semantic_target(&root, &uri, position, &reply).await? {
                    Some((caller, caller_hash)) if caller != target_id => {
                        self.enrich_edge(&caller, &target_id, "reference", &caller_hash, &expected)
                            .await?;
                        report.enriched_edges += 1;
                    }
                    _ => report.skipped_locations += 1,
                }
            }
        }
        report.unavailable.sort();
        report.unavailable.dedup();
        Ok(report)
    }
    async fn semantic_target(
        &self,
        root: &Path,
        uri: &str,
        position: Position,
        reply: &LspReply,
    ) -> Result<Option<(String, String)>> {
        let Ok(url) = url::Url::parse(uri) else {
            return Ok(None);
        };
        let Ok(path) = url.to_file_path() else {
            return Ok(None);
        };
        let Ok(relative) = path.strip_prefix(root) else {
            return Ok(None);
        };
        let Ok(relative) = crate::graph::relative(relative) else {
            return Ok(None);
        };
        let lookup = relative.clone();
        let target=self.db.read(ReadLane::Background,move|c|{use excalibur_store::rusqlite::OptionalExtension;Ok(c.query_row("SELECT n.id,f.hash FROM nodes n JOIN files f ON f.id=n.file_id JOIN ci_node_meta m ON m.node_id=n.id WHERE f.path=?1 AND n.kind IN('function','method','type','test','module','section') AND(m.start_line<?2 OR(m.start_line=?2 AND m.start_character<=?3)) AND(m.end_line>?2 OR(m.end_line=?2 AND m.end_character>=?3)) ORDER BY n.end_offset-n.start_offset,n.id LIMIT 1",params![lookup,position.line,position.character],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?) }).await?;
        let Some((node, digest)) = target else {
            return Ok(None);
        };
        if reply.document_hashes.get(uri) != Some(&digest) {
            return Ok(None);
        }
        match verified_hash(root.into(), relative).await {
            Ok(actual) if actual == digest => Ok(Some((node, digest))),
            _ => Ok(None),
        }
    }
}
fn locations(value: &Value) -> Vec<(String, Position)> {
    if let Some(values) = value.as_array() {
        return values.iter().flat_map(locations).collect();
    }
    let Some(uri) = value["uri"]
        .as_str()
        .or_else(|| value["targetUri"].as_str())
    else {
        return vec![];
    };
    let range = value
        .get("targetSelectionRange")
        .or_else(|| value.get("range"));
    let Some(position) = range
        .and_then(|r| r.get("start"))
        .and_then(|p| serde_json::from_value(p.clone()).ok())
    else {
        return vec![];
    };
    vec![(uri.into(), position)]
}
fn utf16_position(text: &str, offset: usize) -> Result<Position> {
    let text = text
        .get(..offset)
        .ok_or_else(|| invalid("semantic source offset is stale"))?;
    Ok(Position {
        line: text.bytes().filter(|&b| b == b'\n').count() as u32,
        character: text
            .rsplit('\n')
            .next()
            .unwrap_or("")
            .encode_utf16()
            .count() as u32,
    })
}
async fn verified_hash(root: PathBuf, relative: String) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        if excalibur_sandbox::file_authority::credential_path(Path::new(&relative))
            || Path::new(&relative).components().any(|c| {
                excalibur_sandbox::PROTECTED_DIRECTORIES
                    .iter()
                    .any(|name| c.as_os_str() == *name)
            })
        {
            return Err(invalid(
                "protected source cannot be read for semantic enrichment",
            ));
        }
        let directory = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority())?;
        let mut file = directory.open(&relative)?;
        excalibur_sandbox::file_authority::ReadAuthority::excluding_credential_names()
            .check(&directory, &root, &[], &file)
            .map_err(invalid)?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(invalid("semantic source exceeds bounded read limit"));
        }
        Ok(hash(&bytes))
    })
    .await
    .map_err(|_| invalid("semantic source reader failed"))?
}

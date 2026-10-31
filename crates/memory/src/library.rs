//! Document library with versioned originals, structure-aware chunks, citable search and
//! deterministic citation rendering and validation (architecture 10.9).
use crate::{
    BoxFuture, Error, MemoryGrant, MemoryStore, Result, digest, invalid, new_id, placeholders,
    retrieval::{Notice, RetrievalQuery, RetrievedItem, Sources},
    text::normalize,
};
use excalibur_store::{
    Database, Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Heading(u8),
    Paragraph,
    Table,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedBlock {
    pub kind: BlockKind,
    pub text: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedPage {
    pub number: u32,
    pub blocks: Vec<ExtractedBlock>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedSegment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

/// Security control applied to every file before ingestion (20.7 host hook).
pub trait IngestGuard: Send + Sync {
    fn inspect(&self, media_type: &str, bytes: &[u8]) -> std::result::Result<(), String>;
}
/// `vision_extract` contract: pages or images to text blocks per page.
pub trait VisionExtractor: Send + Sync {
    fn extract<'a>(
        &'a self,
        media_type: &'a str,
        bytes: &'a [u8],
    ) -> BoxFuture<'a, std::result::Result<Vec<ExtractedPage>, String>>;
}
/// Voice capability contract: timestamped transcription.
pub trait Transcriber: Send + Sync {
    fn transcribe<'a>(
        &'a self,
        media_type: &'a str,
        bytes: &'a [u8],
    ) -> BoxFuture<'a, std::result::Result<Vec<TimedSegment>, String>>;
}
/// One-time context sentence for ambiguous chunks of documents marked as library.
pub trait ContextWriter: Send + Sync {
    fn context<'a>(
        &'a self,
        title: &'a str,
        section: Option<&'a str>,
        chunk: &'a str,
    ) -> BoxFuture<'a, std::result::Result<String, String>>;
}

/// Built-in guard. Delegates to the shared content guard (`excalibur_ingest::ContentGuard`):
/// size bound, UTF-8 text, content-signature type detection against the declared media type,
/// refusal of executable code and bounded inspection of zip, gzip, zstd and tar containers.
/// It has no scanner and no quarantine; hosts that need them install a configured
/// `ContentGuard` directly, which implements [`IngestGuard`] as well.
pub struct BasicIngestGuard {
    pub max_bytes: usize,
}
impl Default for BasicIngestGuard {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
        }
    }
}
impl IngestGuard for BasicIngestGuard {
    fn inspect(&self, media_type: &str, bytes: &[u8]) -> std::result::Result<(), String> {
        let mut policy = excalibur_ingest::Policy::default();
        policy.limits.max_input_bytes = self.max_bytes as u64;
        let guard = excalibur_ingest::ContentGuard::new(policy);
        IngestGuard::inspect(&guard, media_type, bytes)
    }
}
impl IngestGuard for excalibur_ingest::ContentGuard {
    fn inspect(&self, media_type: &str, bytes: &[u8]) -> std::result::Result<(), String> {
        self.examine(None, media_type, bytes)
            .map(|_| ())
            .map_err(|rejection| rejection.to_string())
    }
}

#[derive(Clone)]
pub struct LibraryServices {
    pub guard: Arc<dyn IngestGuard>,
    pub vision: Option<Arc<dyn VisionExtractor>>,
    pub transcriber: Option<Arc<dyn Transcriber>>,
    pub context: Option<Arc<dyn ContextWriter>>,
}
impl Default for LibraryServices {
    fn default() -> Self {
        Self {
            guard: Arc::new(BasicIngestGuard::default()),
            vision: None,
            transcriber: None,
            context: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestRequest {
    pub scope: String,
    pub title: String,
    pub media_type: String,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(skip)]
    pub bytes: Vec<u8>,
    /// Existing document receiving a new version.
    #[serde(default)]
    pub document_id: Option<String>,
    /// Marked by the user as library: enables one-time context sentences.
    #[serde(default)]
    pub library: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestResult {
    pub document_id: String,
    pub version: i64,
    pub chunks: usize,
    pub paginated: bool,
    pub unchanged: bool,
    pub queued_for_indexing: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChunkDraft {
    text: String,
    page: Option<u32>,
    paragraph: u32,
    section: Option<String>,
    start: usize,
    end: usize,
    line_start: Option<usize>,
    line_end: Option<usize>,
    time_start: Option<i64>,
    time_end: Option<i64>,
    kind: &'static str,
    context: Option<String>,
}

const MAX_CHUNK: usize = 1500;

fn split_long(text: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= MAX_CHUNK {
        return vec![(0, text.to_string())];
    }
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut end = (start + 1200).min(chars.len());
        if end < chars.len() {
            let window_end = (start + MAX_CHUNK).min(chars.len());
            if let Some(cut) = (end..window_end)
                .find(|&i| matches!(chars[i - 1], '.' | '!' | '?') && chars[i].is_whitespace())
            {
                end = cut;
            } else {
                end = window_end;
            }
        }
        pieces.push((start, chars[start..end].iter().collect()));
        start = end;
    }
    pieces
}

fn section_label(stack: &[(u8, String)]) -> Option<String> {
    (!stack.is_empty()).then(|| {
        stack
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join(" > ")
    })
}
fn push_heading(stack: &mut Vec<(u8, String)>, level: u8, title: String) {
    while stack.last().is_some_and(|(l, _)| *l >= level) {
        stack.pop();
    }
    stack.push((level, title));
}

/// Markdown/plain text: headings, paragraphs and tables with offsets and line ranges.
pub(crate) fn chunk_text(text: &str) -> Vec<ChunkDraft> {
    let mut out = Vec::new();
    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut paragraph = 0u32;
    let mut block: Vec<&str> = Vec::new();
    let (mut block_start, mut block_line, mut block_kind) = (0usize, 0usize, "paragraph");
    let mut offset = 0usize;
    let flush = |block: &mut Vec<&str>,
                 start: usize,
                 line: usize,
                 kind: &'static str,
                 stack: &[(u8, String)],
                 paragraph: &mut u32,
                 out: &mut Vec<ChunkDraft>| {
        if block.is_empty() {
            return;
        }
        let joined = block.join("\n");
        *paragraph += 1;
        for (rel, piece) in split_long(&joined) {
            let before: String = joined.chars().take(rel).collect();
            let first_line = line + before.matches('\n').count();
            let len = piece.chars().count();
            out.push(ChunkDraft {
                line_start: Some(first_line),
                line_end: Some(first_line + piece.matches('\n').count()),
                text: piece,
                paragraph: *paragraph,
                section: section_label(stack),
                start: start + rel,
                end: start + rel + len,
                kind,
                ..Default::default()
            });
        }
        block.clear();
    };
    for (index, line) in text.split('\n').enumerate() {
        let number = index + 1;
        let trimmed = line.trim();
        let heading = trimmed.starts_with('#') && trimmed.trim_start_matches('#').starts_with(' ');
        let table = trimmed.starts_with('|');
        if trimmed.is_empty()
            || heading
            || (table && block_kind != "table")
            || (!table && block_kind == "table")
        {
            flush(
                &mut block,
                block_start,
                block_line,
                block_kind,
                &stack,
                &mut paragraph,
                &mut out,
            );
        }
        if heading {
            let level = trimmed.chars().take_while(|c| *c == '#').count().min(6) as u8;
            push_heading(
                &mut stack,
                level,
                trimmed.trim_start_matches('#').trim().to_string(),
            );
            paragraph = 0;
        } else if !trimmed.is_empty() {
            if block.is_empty() {
                block_start = offset;
                block_line = number;
                block_kind = if table { "table" } else { "paragraph" };
            }
            block.push(line);
        }
        offset += line.chars().count() + 1;
    }
    flush(
        &mut block,
        block_start,
        block_line,
        block_kind,
        &stack,
        &mut paragraph,
        &mut out,
    );
    out
}

/// Paginated extraction: paragraph numbers restart on every page.
pub(crate) fn chunk_pages(pages: &[ExtractedPage]) -> Vec<ChunkDraft> {
    let mut out = Vec::new();
    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut offset = 0usize;
    for page in pages {
        let mut paragraph = 0u32;
        for block in &page.blocks {
            let len = block.text.chars().count();
            match block.kind {
                BlockKind::Heading(level) => {
                    push_heading(&mut stack, level.max(1), block.text.trim().to_string())
                }
                BlockKind::Paragraph | BlockKind::Table if !block.text.trim().is_empty() => {
                    paragraph += 1;
                    for (rel, piece) in split_long(&block.text) {
                        let piece_len = piece.chars().count();
                        out.push(ChunkDraft {
                            text: piece,
                            page: Some(page.number),
                            paragraph,
                            section: section_label(&stack),
                            start: offset + rel,
                            end: offset + rel + piece_len,
                            kind: if block.kind == BlockKind::Table {
                                "table"
                            } else {
                                "paragraph"
                            },
                            ..Default::default()
                        });
                    }
                }
                _ => {}
            }
            offset += len + 2;
        }
    }
    out
}

pub(crate) fn chunk_timed(segments: &[TimedSegment]) -> Vec<ChunkDraft> {
    let mut out: Vec<ChunkDraft> = Vec::new();
    let mut offset = 0usize;
    for segment in segments.iter().filter(|s| !s.text.trim().is_empty()) {
        let len = segment.text.chars().count();
        match out.last_mut() {
            Some(last) if last.text.chars().count() + len < 600 => {
                last.text.push(' ');
                last.text.push_str(&segment.text);
                last.end = offset + len;
                last.time_end = Some(segment.end_ms);
            }
            _ => out.push(ChunkDraft {
                text: segment.text.clone(),
                paragraph: out.len() as u32 + 1,
                start: offset,
                end: offset + len,
                time_start: Some(segment.start_ms),
                time_end: Some(segment.end_ms),
                kind: "transcript",
                ..Default::default()
            }),
        }
        offset += len + 1;
    }
    out
}

fn ambiguous(text: &str) -> bool {
    let n = normalize(text);
    const LEADS: &[&str] = &[
        "this ",
        "it ",
        "they ",
        "these ",
        "those ",
        "he ",
        "she ",
        "however",
        "also ",
        "esto ",
        "este ",
        "esta ",
        "estos ",
        "ello ",
        "ademas",
        "sin embargo",
        "el mismo",
        "la misma",
    ];
    n.chars().count() < 120 || LEADS.iter().any(|l| n.starts_with(l))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRow {
    pub id: String,
    pub document_id: String,
    pub version: i64,
    pub ordinal: i64,
    pub principal: String,
    pub scope: String,
    pub title: String,
    pub paginated: bool,
    pub current: bool,
    pub text: String,
    pub page: Option<i64>,
    pub paragraph: Option<i64>,
    pub section: Option<String>,
    pub start_offset: Option<i64>,
    pub end_offset: Option<i64>,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
    pub time_start_ms: Option<i64>,
    pub time_end_ms: Option<i64>,
    pub recorded_at: i64,
}
fn clock(ms: i64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}
impl ChunkRow {
    pub fn cite_token(&self) -> String {
        format!("[[doc:{}#c{}]]", self.document_id, self.ordinal)
    }
    /// Location from metadata only; pages are never taken from model output.
    pub fn location(&self, locale: &str) -> String {
        let es = locale.starts_with("es");
        if self.paginated
            && let Some(page) = self.page
        {
            let para = self
                .paragraph
                .map(|p| {
                    if es {
                        format!(", párr. {p}")
                    } else {
                        format!(", para. {p}")
                    }
                })
                .unwrap_or_default();
            return format!("{}, p. {page}{para}", self.title);
        }
        if let (Some(a), Some(b)) = (self.time_start_ms, self.time_end_ms) {
            return format!("{}, {}–{}", self.title, clock(a), clock(b));
        }
        let mut parts = vec![self.title.clone()];
        if let Some(section) = &self.section {
            parts.push(format!("§ {section}"));
        }
        if let Some(p) = self.paragraph {
            parts.push(if es {
                format!("párr. {p}")
            } else {
                format!("para. {p}")
            });
        }
        if let (Some(a), Some(b)) = (self.line_start, self.line_end) {
            parts.push(if es {
                format!("líneas {a}–{b}")
            } else {
                format!("lines {a}–{b}")
            });
        }
        parts.push(if es {
            "sin paginación".into()
        } else {
            "no pagination".into()
        });
        parts.join(", ")
    }
}

pub(crate) fn load_chunk(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<ChunkRow>> {
    c.query_row(
        "SELECT ch.id,ch.document_id,ch.version,ch.ordinal,COALESCE(ch.principal_id,''),COALESCE(ch.scope_id,''),d.title,d.paginated,d.version=ch.version,ch.text,ch.page,ch.paragraph,ch.section,ch.start_offset,ch.end_offset,ch.line_start,ch.line_end,ch.time_start_ms,ch.time_end_ms,ch.created_at FROM doc_chunks ch JOIN documents d ON d.id=ch.document_id WHERE ch.id=?1",
        [id],
        |r| {
            Ok(ChunkRow {
                id: r.get(0)?,
                document_id: r.get(1)?,
                version: r.get(2)?,
                ordinal: r.get(3)?,
                principal: r.get(4)?,
                scope: r.get(5)?,
                title: r.get(6)?,
                paginated: r.get(7)?,
                current: r.get(8)?,
                text: r.get(9)?,
                page: r.get(10)?,
                paragraph: r.get(11)?,
                section: r.get(12)?,
                start_offset: r.get(13)?,
                end_offset: r.get(14)?,
                line_start: r.get(15)?,
                line_end: r.get(16)?,
                time_start_ms: r.get(17)?,
                time_end_ms: r.get(18)?,
                recorded_at: r.get(19)?,
            })
        },
    )
    .optional()
}

impl MemoryStore {
    pub fn with_library(mut self, services: LibraryServices) -> Self {
        self.library = services;
        self
    }
    /// `library_ingest`: guard, extract, chunk, store the versioned original and index.
    pub async fn library_ingest(
        &self,
        grant: &MemoryGrant,
        request: IngestRequest,
        now: i64,
    ) -> Result<IngestResult> {
        grant.require(&request.scope)?;
        let title = request.title.trim().to_string();
        if title.is_empty() || title.len() > 512 {
            return Err(invalid("document title required"));
        }
        self.library
            .guard
            .inspect(&request.media_type, &request.bytes)
            .map_err(Error::Rejected)?;
        let media = request.media_type.as_str();
        let (paginated, mut drafts) =
            if media.starts_with("text/") || media == "application/json" {
                let text = String::from_utf8(request.bytes.clone())
                    .map_err(|_| invalid("text is not UTF-8"))?;
                (false, chunk_text(&self.redactor.redact_text(&text)))
            } else if media == "application/pdf" || media.starts_with("image/") {
                let vision = self.library.vision.clone().ok_or_else(|| {
                    Error::Unavailable("vision extraction is not connected".into())
                })?;
                let mut pages = vision
                    .extract(media, &request.bytes)
                    .await
                    .map_err(Error::Unavailable)?;
                for page in &mut pages {
                    for block in &mut page.blocks {
                        block.text = self.redactor.redact_text(&block.text);
                    }
                }
                (media == "application/pdf", chunk_pages(&pages))
            } else if media.starts_with("audio/") {
                let transcriber =
                    self.library.transcriber.clone().ok_or_else(|| {
                        Error::Unavailable("transcription is not connected".into())
                    })?;
                let mut segments = transcriber
                    .transcribe(media, &request.bytes)
                    .await
                    .map_err(Error::Unavailable)?;
                for s in &mut segments {
                    s.text = self.redactor.redact_text(&s.text);
                }
                (false, chunk_timed(&segments))
            } else {
                return Err(Error::Unsupported(format!(
                    "{media} extraction is not connected"
                )));
            };
        if drafts.is_empty() {
            return Err(invalid("document produced no text"));
        }
        if request.library
            && let Some(writer) = self.library.context.clone()
        {
            for draft in drafts.iter_mut().filter(|d| ambiguous(&d.text)) {
                if let Ok(sentence) = writer
                    .context(&title, draft.section.as_deref(), &draft.text)
                    .await
                {
                    let sentence = sentence.trim().chars().take(400).collect::<String>();
                    if !sentence.is_empty() {
                        draft.context = Some(self.redactor.redact_text(&sentence));
                    }
                }
            }
        }
        self.ensure_principal(&grant.principal).await?;
        let content_hash = digest(&request.bytes);
        let prepared = self.db.blobs().prepare(&request.bytes)?;
        let (principal, scope, existing, kind, origin, library, title2, hash2) = (
            grant.principal.clone(),
            request.scope.clone(),
            request.document_id.clone(),
            request.media_type.clone(),
            request.origin.clone(),
            request.library,
            title.clone(),
            content_hash.clone(),
        );
        let stored = self
            .db
            .write_with_blob(prepared, Priority::Normal, move |c, blob| {
                let (id, version) = match existing {
                    Some(id) => {
                        let row: Option<(String, String, i64, Option<String>)> = c
                            .query_row("SELECT COALESCE(principal_id,''),COALESCE(scope_id,''),version,content_hash FROM documents WHERE id=?1", [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                            .optional()?;
                        let Some((owner, owner_scope, version, previous)) = row else {
                            return Ok(Err("document not found".to_string()));
                        };
                        if owner != principal || owner_scope != scope {
                            return Ok(Err("document belongs to another scope".to_string()));
                        }
                        if previous.as_deref() == Some(hash2.as_str()) {
                            return Ok(Ok((id, version, true, Vec::new())));
                        }
                        let next = version + 1;
                        c.execute("UPDATE documents SET version=?2,blob_hash=?3,kind=?4,origin=?5,title=?6,content_hash=?7,paginated=?8,library=?9,updated_at=?10 WHERE id=?1", rusqlite::params![id, next, blob.as_str(), kind, origin, title2, hash2, paginated, library, now])?;
                        (id, next)
                    }
                    None => {
                        let id = new_id("doc");
                        c.execute("INSERT INTO documents(id,version,blob_hash,kind,origin,principal_id,scope_id,title,content_hash,paginated,library,created_at,updated_at) VALUES(?1,1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11)", rusqlite::params![id, blob.as_str(), kind, origin, principal, scope, title2, hash2, paginated, library, now])?;
                        (id, 1)
                    }
                };
                c.execute("INSERT INTO document_versions(document_id,version,blob_hash,content_hash,kind,origin,title,paginated,recorded_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", rusqlite::params![id, version, blob.as_str(), hash2, kind, origin, title2, paginated, now])?;
                let mut objects = Vec::new();
                for (ordinal, d) in drafts.iter().enumerate() {
                    let chunk_id = format!("{id}@v{version}#c{}", ordinal + 1);
                    c.execute(
                        "INSERT INTO doc_chunks(id,document_id,page,paragraph,start_offset,end_offset,section,text,principal_id,scope_id,version,ordinal,line_start,line_end,time_start_ms,time_end_ms,block_kind,context,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?19)",
                        rusqlite::params![chunk_id, id, d.page, d.paragraph, d.start as i64, d.end as i64, d.section, d.text, principal, scope, version, ordinal as i64 + 1, d.line_start.map(|v| v as i64), d.line_end.map(|v| v as i64), d.time_start, d.time_end, d.kind, d.context, now],
                    )?;
                    let text = match &d.context {
                        Some(ctx) => format!("{ctx} {}", d.text),
                        None => d.text.clone(),
                    };
                    objects.push((chunk_id, text));
                }
                Ok(Ok((id, version, false, objects)))
            })
            .await?
            .map_err(invalid)?;
        let (document_id, version, unchanged, objects) = stored;
        let mut queued = 0;
        let chunks = objects.len();
        for (chunk, text) in objects {
            let outcome = self
                .embeddings
                .index(
                    &grant.principal,
                    &request.scope,
                    &format!("chunk:{chunk}"),
                    &text,
                    now,
                )
                .await?;
            if !outcome.queued.is_empty() {
                queued += 1;
            }
        }
        Ok(IngestResult {
            document_id,
            version,
            chunks,
            paginated,
            unchanged,
            queued_for_indexing: queued,
        })
    }

    /// `library_search`: citable chunks; retrievals are recorded for the calling run.
    pub async fn library_search(
        &self,
        grant: &MemoryGrant,
        text: &str,
        k: usize,
        now: i64,
    ) -> Result<(Vec<RetrievedItem>, Vec<Notice>)> {
        let mut query = RetrievalQuery::new(text);
        query.k = k;
        query.sources = Sources {
            memories: false,
            messages: false,
            chunks: true,
        };
        query.token_budget = 8000;
        let result = self.retrieve(grant, query, now).await?;
        if let Some(run) = grant.run_id.clone() {
            let rows: Vec<(String, String)> = result
                .items
                .iter()
                .map(|i| (i.id.clone(), i.scope.clone()))
                .collect();
            self.db
                .write(Priority::Normal, move |c| {
                    for (chunk, scope) in rows {
                        c.execute("INSERT INTO chunk_retrievals(run_id,chunk_id,scope_id,retrieved_at) VALUES(?1,?2,?3,?4) ON CONFLICT(run_id,chunk_id) DO UPDATE SET retrieved_at=excluded.retrieved_at", rusqlite::params![run, chunk, scope, now])?;
                    }
                    Ok(())
                })
                .await?;
        }
        Ok((result.items, result.notices))
    }

    /// Versions of a document (originals stay as blobs).
    pub async fn document_versions(
        &self,
        grant: &MemoryGrant,
        document_id: &str,
    ) -> Result<Vec<(i64, String, i64)>> {
        grant.validate()?;
        let (id, principal, scopes) = (
            document_id.to_string(),
            grant.principal.clone(),
            grant.scopes.clone(),
        );
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                let owner: Option<(String, String)> = c
                    .query_row("SELECT COALESCE(principal_id,''),COALESCE(scope_id,'') FROM documents WHERE id=?1", [&id], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?;
                if !owner.is_some_and(|(p, s)| p == principal && scopes.contains(&s)) {
                    return Ok(Vec::new());
                }
                let mut s = c.prepare("SELECT version,blob_hash,recorded_at FROM document_versions WHERE document_id=?1 ORDER BY version")?;
                let rows = s.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }

    /// Record a web page actually read during a run (the only admissible web sources).
    #[allow(clippy::too_many_arguments)]
    pub async fn record_source_read(
        &self,
        grant: &MemoryGrant,
        scope: &str,
        search_id: Option<&str>,
        url: &str,
        title: &str,
        content: &[u8],
        now: i64,
    ) -> Result<String> {
        grant.require(scope)?;
        let run = grant
            .run_id
            .clone()
            .ok_or_else(|| invalid("a source read needs its run"))?;
        self.ensure_principal(&grant.principal).await?;
        let id = new_id("src");
        let (id2, principal, scope, search, url, title, hash) = (
            id.clone(),
            grant.principal.clone(),
            scope.to_string(),
            search_id.map(str::to_string),
            url.to_string(),
            title.to_string(),
            digest(content),
        );
        self.db
            .write(Priority::Normal, move |c| {
                c.execute("INSERT INTO source_events(id,run_id,search_id,url,title,observed_at,hash,principal_id,scope_id,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?6,?6)", rusqlite::params![id2, run, search, url, title, now, hash, principal, scope])?;
                Ok(())
            })
            .await?;
        Ok(id)
    }

    /// Validate citations against the task tree of the responding run and render them.
    pub async fn validate_citations(
        &self,
        grant: &MemoryGrant,
        responding_run: &str,
        tree: &dyn RunTree,
        answer: &str,
        locale: &str,
    ) -> Result<CitedAnswer> {
        grant.validate()?;
        let runs: Vec<String> = tree
            .subtree(responding_run)
            .await
            .map_err(Error::Unavailable)?
            .into_iter()
            .collect();
        if runs.is_empty() {
            return Err(invalid("run tree is empty"));
        }
        let token = regex::Regex::new(r"\[\[(doc|web):([^\]\s]{1,300})\]\]").unwrap();
        let doc_ref = regex::Regex::new(r"^([A-Za-z0-9_\-]+)#c(\d{1,9})$").unwrap();
        let mut docs = BTreeSet::new();
        let mut webs = BTreeSet::new();
        for c in token.captures_iter(answer) {
            if &c[1] == "doc" {
                if let Some(d) = doc_ref.captures(&c[2]) {
                    docs.insert((d[1].to_string(), d[2].parse::<i64>().unwrap_or(0)));
                }
            } else {
                webs.insert(c[2].to_string());
            }
        }
        let (principal, scopes) = (grant.principal.clone(), grant.scopes.clone());
        let runs2 = runs.clone();
        let (resolved_docs, resolved_webs) = self
            .db
            .read(ReadLane::Interactive, move |c| {
                let marks = placeholders(4, runs2.len());
                let mut doc_map = BTreeMap::new();
                let mut s = c.prepare(&format!("SELECT ch.id FROM chunk_retrievals cr JOIN doc_chunks ch ON ch.id=cr.chunk_id WHERE ch.document_id=?1 AND ch.ordinal=?2 AND ch.principal_id=?3 AND cr.run_id IN ({marks}) ORDER BY cr.retrieved_at DESC,ch.version DESC LIMIT 1"))?;
                for (doc, ordinal) in docs {
                    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&doc, &ordinal, &principal];
                    for r in &runs2 {
                        params.push(r);
                    }
                    let chunk: Option<String> = s.query_row(params.as_slice(), |r| r.get(0)).optional()?;
                    if let Some(chunk) = chunk
                        && let Some(row) = load_chunk(c, &chunk)?
                        && scopes.contains(&row.scope)
                    {
                        doc_map.insert(format!("{doc}#c{ordinal}"), row);
                    }
                }
                let mut web_map = BTreeMap::new();
                let marks = placeholders(3, runs2.len());
                let mut s = c.prepare(&format!("SELECT url,COALESCE(title,''),COALESCE(observed_at,0),COALESCE(scope_id,'') FROM source_events WHERE id=?1 AND principal_id=?2 AND run_id IN ({marks})"))?;
                for id in webs {
                    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&id, &principal];
                    for r in &runs2 {
                        params.push(r);
                    }
                    let row: Option<(String, String, i64, String)> = s.query_row(params.as_slice(), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
                    if let Some((url, title, at, scope)) = row
                        && scopes.contains(&scope)
                    {
                        web_map.insert(id, (url, title, at));
                    }
                }
                Ok((doc_map, web_map))
            })
            .await?;
        let unsourced = if locale.starts_with("es") {
            " [sin fuente]"
        } else {
            " [unsourced]"
        };
        let mut out = String::new();
        let mut citations = Vec::new();
        let mut removed = Vec::new();
        let mut sources: Vec<WebSource> = Vec::new();
        let mut unsourced_sentences = 0;
        for sentence in sentences(answer) {
            let mut valid = 0;
            let mut seen = 0;
            let rendered = token.replace_all(sentence, |c: &regex::Captures<'_>| {
                seen += 1;
                let whole = c[0].to_string();
                if &c[1] == "doc" {
                    if let Some(row) = resolved_docs.get(&c[2]) {
                        valid += 1;
                        let label = row.location(locale);
                        citations.push(ResolvedCitation {
                            token: whole,
                            chunk_id: Some(row.id.clone()),
                            source_event: None,
                            label: label.clone(),
                        });
                        return format!("[{label}]");
                    }
                } else if let Some((url, title, at)) = resolved_webs.get(&c[2]) {
                    valid += 1;
                    let position = match sources.iter().position(|s| s.source_event == c[2]) {
                        Some(p) => p + 1,
                        None => {
                            sources.push(WebSource {
                                source_event: c[2].to_string(),
                                url: url.clone(),
                                title: title.clone(),
                                read_at: *at,
                            });
                            sources.len()
                        }
                    };
                    citations.push(ResolvedCitation {
                        token: whole,
                        chunk_id: None,
                        source_event: Some(c[2].to_string()),
                        label: format!("[{position}]"),
                    });
                    return format!("[{position}]");
                }
                removed.push(whole);
                String::new()
            });
            let mut rendered = rendered.into_owned();
            if seen > 0 && valid == 0 {
                unsourced_sentences += 1;
                let trimmed_len = rendered.trim_end().len();
                let tail = rendered[trimmed_len..].to_string();
                rendered.truncate(trimmed_len);
                let body = rendered
                    .trim_end_matches(['.', '!', '?'])
                    .trim_end()
                    .to_string();
                let punct = rendered[body.len()..].trim().to_string();
                rendered = format!("{body}{unsourced}{punct}{tail}");
            }
            out.push_str(&rendered.replace("  ", " ").replace(" .", "."));
        }
        Ok(CitedAnswer {
            text: out,
            citations,
            removed,
            unsourced_sentences,
            sources,
        })
    }
}

fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for (i, (at, c)) in chars.iter().enumerate() {
        let next_ws = chars.get(i + 1).is_none_or(|(_, n)| n.is_whitespace());
        if *c == '\n' || (matches!(c, '.' | '!' | '?') && next_ws) {
            let mut end = at + c.len_utf8();
            // Keep following whitespace with the sentence.
            for (j, w) in chars.iter().skip(i + 1) {
                if w.is_whitespace() && *w != '\n' {
                    end = j + w.len_utf8();
                } else {
                    break;
                }
            }
            if end > start {
                out.push(&text[start..end]);
                start = end;
            }
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedCitation {
    pub token: String,
    pub chunk_id: Option<String>,
    pub source_event: Option<String>,
    pub label: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSource {
    pub source_event: String,
    pub url: String,
    pub title: String,
    pub read_at: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitedAnswer {
    pub text: String,
    pub citations: Vec<ResolvedCitation>,
    pub removed: Vec<String>,
    pub unsourced_sentences: usize,
    /// Only pages read in the task tree and cited by the answer.
    pub sources: Vec<WebSource>,
}

/// Task tree of runs: the responding run and all its descendants.
pub trait RunTree: Send + Sync {
    fn subtree<'a>(
        &'a self,
        root: &'a str,
    ) -> BoxFuture<'a, std::result::Result<BTreeSet<String>, String>>;
}
#[derive(Clone, Debug, Default)]
pub struct StaticRunTree {
    children: BTreeMap<String, Vec<String>>,
}
impl StaticRunTree {
    pub fn with_child(mut self, parent: &str, child: &str) -> Self {
        self.children
            .entry(parent.into())
            .or_default()
            .push(child.into());
        self
    }
}
impl RunTree for StaticRunTree {
    fn subtree<'a>(
        &'a self,
        root: &'a str,
    ) -> BoxFuture<'a, std::result::Result<BTreeSet<String>, String>> {
        Box::pin(async move {
            let mut out = BTreeSet::from([root.to_string()]);
            let mut stack = vec![root.to_string()];
            while let Some(node) = stack.pop() {
                for child in self.children.get(&node).into_iter().flatten() {
                    if out.insert(child.clone()) {
                        stack.push(child.clone());
                    }
                }
            }
            Ok(out)
        })
    }
}
/// Reads parent links from the task schema (`task_run_links`) when present.
#[derive(Clone)]
pub struct TaskRunLinkTree {
    pub db: Database,
}
impl RunTree for TaskRunLinkTree {
    fn subtree<'a>(
        &'a self,
        root: &'a str,
    ) -> BoxFuture<'a, std::result::Result<BTreeSet<String>, String>> {
        let root = root.to_string();
        Box::pin(async move {
            self.db
                .read(ReadLane::Interactive, move |c| {
                    let mut out = BTreeSet::from([root.clone()]);
                    let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='task_run_links')", [], |r| r.get(0))?;
                    if !exists {
                        return Ok(out);
                    }
                    let mut s = c.prepare("SELECT run_id FROM task_run_links WHERE parent_run_id=?1")?;
                    let mut stack = vec![root];
                    while let Some(node) = stack.pop() {
                        for child in s.query_map([&node], |r| r.get::<_, String>(0))? {
                            let child = child?;
                            if out.insert(child.clone()) {
                                stack.push(child);
                            }
                        }
                    }
                    Ok(out)
                })
                .await
                .map_err(|e| e.to_string())
        })
    }
}

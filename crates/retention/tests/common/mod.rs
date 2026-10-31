#![allow(dead_code)]
//! Synthetic principal data with unique markers planted in every store that keeps
//! derived copies, plus an exhaustive marker scanner over tables, shadow tables,
//! blobs (raw and decompressed) and telemetry artifacts.
use excalibur_journal::Journal;
use excalibur_memory::{
    IngestRequest, MemoryGrant, MemoryInput, MemoryKind, MemoryPurgeParticipant, MemoryStore,
    SourceRef, Trust, testing::HashEmbedder,
};
use excalibur_retention::{PurgeReason, PurgeRequest, PurgeService, SubjectKind};
use excalibur_store::{
    Database, DatabaseKind, Priority, StorageConfig,
    rusqlite::{self, Connection, OpenFlags, types::ValueRef},
    schema,
};
use excalibur_telemetry::{RunMetadata, Telemetry, TelemetryConfig};
use excalibur_vault::Redactor;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub const PRINCIPAL: &str = "owner";
pub const SCOPE: &str = "general";
pub const OTHER: &str = "project:other";
/// 2026-10-30T12:00:00Z
pub const NOW: i64 = 1_793_361_600;
pub const CONVERSATION_MARK: &str = "kestrelconvmark";
pub const DOCUMENT_MARK: &str = "basaltdocmark";
pub const MEMORY_MARK: &str = "fjordmemmark";
pub const MARKERS: [&str; 3] = [CONVERSATION_MARK, DOCUMENT_MARK, MEMORY_MARK];
/// Distinctive float32 probe vector stored for the purged memory.
pub const PROBE: [f32; 4] = [0.123_456_7, -0.765_432_1, 0.314_159_3, 0.271_828_2];

pub struct World {
    pub root: PathBuf,
    pub main: Database,
    pub memory: MemoryStore,
    pub journal: Journal,
    pub telemetry: Telemetry,
}
impl World {
    pub async fn open(root: &Path) -> Self {
        let main = Database::open(
            root.join("excalibur.db"),
            DatabaseKind::Main,
            StorageConfig::default(),
        )
        .unwrap();
        let memory = MemoryStore::open(main.clone()).await.unwrap();
        let journal = Journal::new(main.clone()).await.unwrap();
        let telemetry = Telemetry::open(TelemetryConfig::new(
            root.join("telemetry"),
            RunMetadata::default(),
            Redactor::default(),
        ))
        .await
        .unwrap();
        Self {
            root: root.to_path_buf(),
            main,
            memory,
            journal,
            telemetry,
        }
    }
    pub async fn service(&self) -> PurgeService {
        PurgeService::open(PRINCIPAL, self.main.clone(), Some(self.telemetry.clone()))
            .await
            .unwrap()
            .with_participant(Arc::new(MemoryPurgeParticipant))
            .with_clock(Arc::new(|| NOW + 60))
    }
    pub async fn shutdown(self) {
        self.telemetry.shutdown().await.unwrap();
        self.main.shutdown().await.unwrap();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Planted {
    pub conversation: String,
    pub messages: Vec<String>,
    pub document: String,
    pub memory: String,
    pub run: String,
    pub bystander_message: String,
}
impl Planted {
    pub fn requests(&self, scope: &str) -> Vec<PurgeRequest> {
        [
            (SubjectKind::Conversation, &self.conversation),
            (SubjectKind::Document, &self.document),
            (SubjectKind::Memory, &self.memory),
        ]
        .into_iter()
        .map(|(kind, id)| PurgeRequest {
            operation_key: format!("purge:{}:{id}", kind.as_str()),
            scope: scope.into(),
            kind,
            subject_id: id.clone(),
            reason: PurgeReason::UserRequest,
            requested_at: NOW,
        })
        .collect()
    }
}
pub fn grant(scopes: &[&str]) -> MemoryGrant {
    MemoryGrant::new(PRINCIPAL, scopes.iter().copied()).with_max_trust(Trust::User)
}
pub fn conversation_text(tag: &str) -> String {
    format!(
        "Remember that the {CONVERSATION_MARK} vault opens at dawn {tag}. Bring the brass key and say nothing."
    )
}
pub fn document_text(tag: &str) -> String {
    format!(
        "The {DOCUMENT_MARK} reactor schedule is confidential {tag}.\nSecond line about cooling loops and the pumps."
    )
}
pub fn memory_text(tag: &str) -> String {
    format!("My locker code is {MEMORY_MARK} 4471 {tag}")
}

/// Plant one conversation, one document and one memory in `scope`, with copies in the
/// journal (inline and blob payload), telemetry, exports, FTS, vectors, blobs, a
/// referencing child table, notifications and edges.
pub async fn plant(world: &World, scope: &str, tag: &str) -> Planted {
    let embedder = Arc::new(HashEmbedder::new("hash-a", 64));
    world
        .memory
        .embeddings()
        .attach(embedder, 64, NOW)
        .await
        .unwrap();
    let conversation = format!("cnv_{tag}");
    let (m1, m2, other_conv, bystander) = (
        format!("msg_{tag}_1"),
        format!("msg_{tag}_2"),
        format!("cnv_{tag}_bystander"),
        format!("msg_{tag}_bystander"),
    );
    let user_text = conversation_text(tag);
    let attachment = world
        .main
        .blobs()
        .prepare(format!("attachment that repeats {CONVERSATION_MARK} verbatim").as_bytes())
        .unwrap();
    {
        let (conversation, m1, m2, other_conv, bystander, scope, text) = (
            conversation.clone(),
            m1.clone(),
            m2.clone(),
            other_conv.clone(),
            bystander.clone(),
            scope.to_string(),
            user_text.clone(),
        );
        world
            .main
            .write_with_blob(attachment, Priority::Normal, move |c, hash| {
                c.execute("INSERT OR IGNORE INTO principals(id,name) VALUES(?1,?1)", [PRINCIPAL])?;
                c.execute("INSERT INTO conversations(id,channel,active_scope,principal_id,scope_id) VALUES(?1,'local',?2,?3,?2)", rusqlite::params![conversation, scope, PRINCIPAL])?;
                c.execute("INSERT INTO conversations(id,channel,active_scope,principal_id,scope_id) VALUES(?1,'local',?2,?3,?2)", rusqlite::params![other_conv, scope, PRINCIPAL])?;
                let attachments = json!([hash.as_str()]).to_string();
                c.execute("INSERT INTO messages(id,conversation_id,role,text,attachments,principal_id,scope_id) VALUES(?1,?2,'user',?3,?4,?5,?6)", rusqlite::params![m1, conversation, text, attachments, PRINCIPAL, scope])?;
                c.execute("INSERT INTO messages(id,conversation_id,role,text,reply_to,principal_id,scope_id) VALUES(?1,?2,'assistant',?3,?4,?5,?6)", rusqlite::params![m2, conversation, format!("Understood: the {CONVERSATION_MARK} detail is noted."), m1, PRINCIPAL, scope])?;
                c.execute("INSERT INTO messages(id,conversation_id,role,text,reply_to,principal_id,scope_id) VALUES(?1,?2,'user',?3,?4,?5,?6)", rusqlite::params![bystander, other_conv, "An unrelated bystander message that stays.", m1, PRINCIPAL, scope])?;
                c.execute("INSERT INTO client_messages(client_msg_id,client_id,message_id,principal_id,scope_id) VALUES(?1,'desktop',?2,?3,?4)", rusqlite::params![format!("client-{m1}"), m1, PRINCIPAL, scope])?;
                c.execute("CREATE TABLE IF NOT EXISTS test_inbox(message_id TEXT NOT NULL REFERENCES messages(id),preanalysis TEXT NOT NULL)", [])?;
                c.execute("INSERT INTO test_inbox(message_id,preanalysis) VALUES(?1,?2)", rusqlite::params![m1, json!({"summary": text}).to_string()])?;
                c.execute("INSERT INTO memory_keys(id,principal_id,scope_id,message_id,key_text) VALUES(?1,?2,?3,?4,?5)", rusqlite::params![format!("key_{tag}"), PRINCIPAL, scope, m1, format!("{CONVERSATION_MARK} vault opens at dawn")])?;
                Ok(())
            })
            .await
            .unwrap();
    }
    let embeddings = world.memory.embeddings();
    embeddings
        .index(PRINCIPAL, scope, &format!("message:{m1}"), &user_text, NOW)
        .await
        .unwrap();
    embeddings
        .index(
            PRINCIPAL,
            scope,
            &format!("key:key_{tag}"),
            &format!("{CONVERSATION_MARK} vault opens at dawn"),
            NOW,
        )
        .await
        .unwrap();
    let g = grant(&[scope]);
    let mut input = MemoryInput::new(scope, MemoryKind::Fact, memory_text(tag));
    input.trust = Trust::User;
    input.sources = vec![SourceRef::message(&m1)];
    let memory = world
        .memory
        .memory_write(&g, input, NOW)
        .await
        .unwrap()
        .memory_id;
    let doc_text = document_text(tag);
    let document = world
        .memory
        .library_ingest(
            &g,
            IngestRequest {
                scope: scope.into(),
                title: format!("Plan {DOCUMENT_MARK} {tag}"),
                media_type: "text/plain".into(),
                origin: None,
                bytes: doc_text.clone().into_bytes(),
                document_id: None,
                library: false,
            },
            NOW,
        )
        .await
        .unwrap()
        .document_id;
    {
        let (memory, scope, tag) = (memory.clone(), scope.to_string(), tag.to_string());
        world
            .main
            .write(Priority::Normal, move |c| {
                schema::create_vector_space(c, "probe", 4)?;
                schema::put_vector(c, "probe", &scope, &format!("memory:{memory}"), &json!(PROBE).to_string())?;
                c.execute("INSERT INTO notifications(id,class,content,principal_id,scope_id) VALUES(?1,'normal',?2,?3,?4)", rusqlite::params![format!("ntf_{memory}"), format!("Reminder: {}", memory_text(&tag)), PRINCIPAL, scope])?;
                c.execute("INSERT INTO edges(id,layer,source_type,source_id,relation_type,destination_type,destination_id,principal_id,scope_id) VALUES(?1,'memory','memory',?2,'about','entity','ent_x',?3,?4)", rusqlite::params![format!("edg_{memory}"), memory, PRINCIPAL, scope])?;
                Ok(())
            })
            .await
            .unwrap();
    }
    // Journal copies: run seed, inline event, >64 KiB blob event, terminal state.
    let run = world
        .journal
        .create_run(
            &format!("op-{tag}"),
            json!({"instruction": user_text, "scope": scope}),
        )
        .await
        .unwrap();
    world
        .journal
        .append(
            &run,
            "tool_result",
            "tool:1",
            json!({"memory": memory_text(tag), "quote": format!("user wrote: {user_text}")}),
        )
        .await
        .unwrap();
    world
        .journal
        .append(
            &run,
            "document_read",
            "tool:2",
            json!({"text": doc_text, "padding": "x".repeat(70 * 1024)}),
        )
        .await
        .unwrap();
    world
        .journal
        .set_state(&run, "completed", None, "done")
        .await
        .unwrap();
    // A consumer that hashes observed journal events (as the tasks crate does).
    {
        let events = world.journal.replay(&run, 0).await.unwrap();
        let run = run.clone();
        world
            .main
            .write(Priority::Normal, move |c| {
                c.execute("CREATE TABLE IF NOT EXISTS task_observed_events(run_id TEXT NOT NULL,sequence INTEGER NOT NULL,payload_hash TEXT NOT NULL,PRIMARY KEY(run_id,sequence))", [])?;
                for event in &events {
                    c.execute("INSERT INTO task_observed_events VALUES(?1,?2,?3)", rusqlite::params![run, event.sequence, sha256(&serde_json::to_vec(event).unwrap())])?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }
    world
        .telemetry
        .register_run(
            &run,
            RunMetadata {
                instruction: user_text.clone(),
                ..RunMetadata::default()
            },
        )
        .await
        .unwrap();
    world.telemetry.reconcile(&world.journal, &run).await.unwrap();
    Planted {
        conversation,
        messages: vec![m1, m2],
        document,
        memory,
        run,
        bystander_message: bystander,
    }
}
pub fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Every place a marker was found, as `location: marker`.
pub fn scan(root: &Path, markers: &[&str]) -> Vec<String> {
    let mut hits = Vec::new();
    for db in [root.join("excalibur.db"), root.join("telemetry/telemetry.db")] {
        scan_database(&db, markers, &mut hits);
    }
    for dir in [root.join("blobs"), root.join("telemetry")] {
        scan_files(&dir, markers, &mut hits);
    }
    hits.sort();
    hits.dedup();
    hits
}
fn contains(haystack: &[u8], markers: &[&str]) -> Option<String> {
    let lower = haystack.to_ascii_lowercase();
    markers
        .iter()
        .find(|m| lower.windows(m.len()).any(|w| w == m.as_bytes()))
        .map(|m| m.to_string())
}
fn scan_database(path: &Path, markers: &[&str], hits: &mut Vec<String>) {
    excalibur_store::initialize_sqlite().unwrap();
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let tables: Vec<(String, String)> = c
        .prepare("SELECT name,coalesce(sql,'') FROM sqlite_schema WHERE type='table'")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    for (table, sql) in tables {
        // Virtual tables are read through their shadow tables, which are regular tables.
        if sql.to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE") {
            continue;
        }
        let mut statement = c
            .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
            .unwrap();
        let width = statement.column_count();
        let mut rows = statement.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for index in 0..width {
                let bytes = match row.get_ref(index).unwrap() {
                    ValueRef::Text(b) | ValueRef::Blob(b) => b.to_vec(),
                    _ => continue,
                };
                if let Some(marker) = contains(&bytes, markers) {
                    hits.push(format!("{}:{table}: {marker}", path.file_name().unwrap().to_string_lossy()));
                }
            }
        }
    }
}
fn scan_files(dir: &Path, markers: &[&str], hits: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_files(&path, markers, hits);
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // SQLite files are scanned table by table above; raw pages are reported separately.
        if name.starts_with("telemetry.db") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if let Some(marker) = contains(&bytes, markers) {
            hits.push(format!("file:{}: {marker}", path.display()));
        }
        if let Ok(decoded) = zstd::decode_all(bytes.as_slice())
            && let Some(marker) = contains(&decoded, markers)
        {
            hits.push(format!("blob:{}: {marker}", path.display()));
        }
    }
}
/// Raw bytes of the live database and WAL files (free pages, freeblocks, frames).
pub fn raw_file_hits(root: &Path, markers: &[&str]) -> Vec<String> {
    let mut hits = Vec::new();
    for file in [
        "excalibur.db",
        "excalibur.db-wal",
        "telemetry/telemetry.db",
        "telemetry/telemetry.db-wal",
    ] {
        if let Ok(bytes) = std::fs::read(root.join(file))
            && let Some(marker) = contains(&bytes, markers)
        {
            hits.push(format!("{file}: {marker}"));
        }
    }
    hits
}
/// Little-endian bytes of the probe vector, as stored by sqlite-vec.
pub fn probe_bytes() -> Vec<u8> {
    PROBE.iter().flat_map(|v| v.to_le_bytes()).collect()
}
pub fn probe_present(root: &Path) -> bool {
    let c = Connection::open_with_flags(root.join("excalibur.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    let needle = probe_bytes();
    let tables: Vec<String> = c
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name LIKE 'scoped_vectors_probe_%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    for table in tables {
        let mut statement = c.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
        let width = statement.column_count();
        let mut rows = statement.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for index in 0..width {
                if let ValueRef::Blob(b) = row.get_ref(index).unwrap()
                    && b.windows(needle.len()).any(|w| w == needle.as_slice())
                {
                    return true;
                }
            }
        }
    }
    false
}
/// FTS5 MATCH hits for every marker over every external-content index.
pub fn fts_hits(root: &Path, markers: &[&str]) -> Vec<String> {
    excalibur_store::initialize_sqlite().unwrap();
    let c = Connection::open_with_flags(root.join("excalibur.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    let indexes: Vec<String> = c
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND sql LIKE 'CREATE VIRTUAL TABLE%fts5%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut hits = Vec::new();
    for index in indexes {
        for marker in markers {
            let count: i64 = c
                .query_row(
                    &format!("SELECT count(*) FROM \"{index}\" WHERE \"{index}\" MATCH ?1"),
                    [format!("\"{marker}\"")],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if count > 0 {
                hits.push(format!("{index}: {marker}"));
            }
        }
    }
    hits
}

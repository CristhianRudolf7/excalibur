#![allow(dead_code)]
use excalibur_memory::{testing::HashEmbedder, *};
use excalibur_store::{Database, DatabaseKind, Priority, StorageConfig, rusqlite};
use std::sync::Arc;

/// 2026-10-30T12:00:00Z
pub const NOW: i64 = 1_793_361_600;
pub const DAY: i64 = 86_400;

pub struct Fixture {
    pub _dir: tempfile::TempDir,
    pub db: Database,
    pub store: MemoryStore,
    pub embedder: Arc<HashEmbedder>,
}

pub async fn fixture() -> Fixture {
    fixture_with(MemoryServices::default()).await
}
pub async fn fixture_with(services: MemoryServices) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let store = MemoryStore::open(db.clone())
        .await
        .unwrap()
        .with_services(services);
    let embedder = Arc::new(HashEmbedder::new("hash-a", 256));
    store
        .embeddings()
        .attach(embedder.clone(), 512, NOW)
        .await
        .unwrap();
    Fixture {
        _dir: dir,
        db,
        store,
        embedder,
    }
}

pub fn grant(scopes: &[&str]) -> MemoryGrant {
    MemoryGrant::new("owner", scopes.iter().copied()).with_max_trust(Trust::User)
}

pub async fn message(
    db: &Database,
    id: &str,
    conversation: &str,
    scope: &str,
    role: &str,
    text: &str,
    at: i64,
) {
    message_for(db, "owner", id, conversation, scope, role, text, at).await
}
#[allow(clippy::too_many_arguments)]
pub async fn message_for(
    db: &Database,
    principal: &str,
    id: &str,
    conversation: &str,
    scope: &str,
    role: &str,
    text: &str,
    at: i64,
) {
    let (p, id, conv, scope, role, text) = (
        principal.to_string(),
        id.to_string(),
        conversation.to_string(),
        scope.to_string(),
        role.to_string(),
        text.to_string(),
    );
    db.write(Priority::Normal, move |c| {
        c.execute("INSERT OR IGNORE INTO principals(id,name) VALUES(?1,?1)", [&p])?;
        c.execute("INSERT OR IGNORE INTO conversations(id,principal_id,active_scope,scope_id) VALUES(?1,?2,?3,?3)", rusqlite::params![conv, p, scope])?;
        c.execute("INSERT INTO messages(id,conversation_id,role,text,sent_at,principal_id,scope_id) VALUES(?1,?2,?3,?4,?5,?6,?7)", rusqlite::params![id, conv, role, text, at, p, scope])?;
        c.execute("CREATE TABLE IF NOT EXISTS master_user_sources(id TEXT PRIMARY KEY,text TEXT NOT NULL)",[])?;
        if role == "user" {
            c.execute("INSERT INTO master_user_sources(id,text) VALUES(?1,?2)",rusqlite::params![id,text])?;
        }
        Ok(())
    })
    .await
    .unwrap();
}

pub async fn run(db: &Database, id: &str) {
    let id = id.to_string();
    db.write(Priority::Normal, move |c| {
        c.execute("INSERT OR IGNORE INTO runs(id) VALUES(?1)", [&id])?;
        Ok(())
    })
    .await
    .unwrap();
}

pub fn input(scope: &str, kind: MemoryKind, statement: &str) -> MemoryInput {
    let mut i = MemoryInput::new(scope, kind, statement);
    i.trust = Trust::User;
    i.sources = vec![SourceRef::message("msg-src")];
    i
}

pub async fn write(
    store: &MemoryStore,
    g: &MemoryGrant,
    i: MemoryInput,
    at: i64,
) -> AdmissionResult {
    store.memory_write(g, i, at).await.unwrap()
}

pub async fn search(store: &MemoryStore, g: &MemoryGrant, text: &str, now: i64) -> RetrievalResult {
    store
        .retrieve(g, RetrievalQuery::new(text), now)
        .await
        .unwrap()
}
pub fn ids(result: &RetrievalResult) -> Vec<String> {
    result.items.iter().map(|i| i.id.clone()).collect()
}

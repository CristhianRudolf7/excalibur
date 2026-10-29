//! SQL-neutral core contract. SQLite command closures remain on the Database adapter.
use crate::{BlobHash, Database, Priority, ReadLane, Result, ShortCodeKind};
use excalibur_types::{ConversationId, MessageId, ProjectId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageRecord {
    pub id: MessageId,
    pub conversation_id: Option<ConversationId>,
    pub text: String,
    pub scope_id: String,
    pub role: String,
    pub language: String,
    pub user_timezone: String,
}
#[derive(Clone, Debug)]
pub struct BlobOwner {
    pub kind: String,
    pub id: String,
    pub slot: String,
}
/// Core consumers use domain values, independent of SQLite or filesystem layouts.
/// Lifecycle, migration and trusted low-level SQL belong to the Database adapter.
pub trait Store: Clone + Send + Sync {
    fn append_message(
        &self,
        message: MessageRecord,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
    fn message(
        &self,
        id: MessageId,
    ) -> impl std::future::Future<Output = Result<Option<MessageRecord>>> + Send;
    fn search_messages(
        &self,
        query: String,
        scopes: Vec<String>,
        limit: u32,
    ) -> impl std::future::Future<Output = Result<Vec<MessageRecord>>> + Send;
    fn reserve_code(
        &self,
        project: ProjectId,
        kind: ShortCodeKind,
    ) -> impl std::future::Future<Output = Result<String>> + Send;
    fn put_blob(
        &self,
        bytes: Vec<u8>,
        owner: BlobOwner,
    ) -> impl std::future::Future<Output = Result<BlobHash>> + Send;
    fn get_blob(
        &self,
        hash: BlobHash,
        max_bytes: u64,
    ) -> impl std::future::Future<Output = Result<Vec<u8>>> + Send;
}
fn message_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRecord> {
    fn parse<T: std::str::FromStr>(text: String) -> rusqlite::Result<T>
    where
        T::Err: std::error::Error + Send + Sync + 'static,
    {
        text.parse().map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
    }
    Ok(MessageRecord {
        id: parse(row.get(0)?)?,
        conversation_id: row.get::<_, Option<String>>(1)?.map(parse).transpose()?,
        text: row.get(2)?,
        role: row.get(3)?,
        language: row.get(4)?,
        user_timezone: row.get(5)?,
        scope_id: row.get(6)?,
    })
}
impl Store for Database {
    async fn append_message(&self, message: MessageRecord) -> Result<()> {
        if message.scope_id.trim().is_empty() {
            return Err(crate::Error::Invalid("message scope required"));
        }
        self.write(Priority::Interactive,move|c|{
            c.execute("INSERT INTO messages(id,conversation_id,text,role,language,user_timezone,scope_id) VALUES(?1,?2,?3,?4,?5,?6,?7)",rusqlite::params![message.id.to_string(),message.conversation_id.map(|id|id.to_string()),message.text,message.role,message.language,message.user_timezone,message.scope_id])?;
            Ok(())
        }).await
    }
    async fn message(&self, id: MessageId) -> Result<Option<MessageRecord>> {
        use rusqlite::OptionalExtension;
        self.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT id,conversation_id,text,role,language,user_timezone,coalesce(scope_id,'legacy:unscoped') FROM messages WHERE id=?1",[id.to_string()],message_row).optional()?)).await
    }
    async fn search_messages(
        &self,
        query: String,
        scopes: Vec<String>,
        limit: u32,
    ) -> Result<Vec<MessageRecord>> {
        if limit == 0
            || limit > 1000
            || scopes.is_empty()
            || scopes.len() > 256
            || scopes.iter().any(|s| s.trim().is_empty())
        {
            return Err(crate::Error::Invalid(
                "explicit nonempty scopes and bounded search limit required",
            ));
        }
        self.read(ReadLane::Background,move|c|{
            let mut statement=c.prepare("SELECT m.id,m.conversation_id,m.text,m.role,m.language,m.user_timezone,coalesce(m.scope_id,'legacy:unscoped') FROM messages_fts f JOIN messages m ON m.rowid=f.rowid WHERE messages_fts MATCH ?1 AND coalesce(m.scope_id,'legacy:unscoped') IN (SELECT value FROM json_each(?2)) ORDER BY bm25(messages_fts),m.id LIMIT ?3")?;
            Ok(statement.query_map(rusqlite::params![query,serde_json::to_string(&scopes).unwrap(),limit],message_row)?.collect::<std::result::Result<Vec<_>,_>>()?)
        }).await
    }
    async fn reserve_code(&self, project: ProjectId, kind: ShortCodeKind) -> Result<String> {
        self.allocate_short_id(project.to_string(), kind).await
    }
    async fn put_blob(&self, bytes: Vec<u8>, owner: BlobOwner) -> Result<BlobHash> {
        let blobs = self.blobs();
        let prepared = tokio::task::spawn_blocking(move || blobs.prepare(&bytes))
            .await
            .map_err(|_| crate::Error::Closed)?;
        if let Err(ref error) = prepared {
            self.report_storage_error(error);
        }
        let prepared = prepared?;
        self.attach_blob(prepared, owner.kind, owner.id, owner.slot)
            .await
    }
    async fn get_blob(&self, hash: BlobHash, max_bytes: u64) -> Result<Vec<u8>> {
        self.read_blob(hash, max_bytes).await
    }
}

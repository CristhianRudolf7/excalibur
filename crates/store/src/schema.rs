//! Embedded, additive schema migrations. No application workflow is implemented here.
use rusqlite::{Connection, OptionalExtension, Result};
use sha2::{Digest, Sha256};

/// Physical database roles have independent schemas and migration histories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatabaseKind {
    Main,
    Telemetry,
    CodeGraph,
}

pub const CURRENT_VERSION: u32 = 4;

fn migrations(kind: DatabaseKind) -> [&'static str; 4] {
    match kind {
        DatabaseKind::Main => [
            include_str!("../migrations/main/001.sql"),
            include_str!("../migrations/main/002.sql"),
            include_str!("../migrations/main/003.sql"),
            include_str!("../migrations/main/004.sql"),
        ],
        DatabaseKind::Telemetry => [
            include_str!("../migrations/telemetry/001.sql"),
            include_str!("../migrations/telemetry/002.sql"),
            include_str!("../migrations/telemetry/003.sql"),
            include_str!("../migrations/telemetry/004.sql"),
        ],
        DatabaseKind::CodeGraph => [
            include_str!("../migrations/codegraph/001.sql"),
            include_str!("../migrations/codegraph/002.sql"),
            include_str!("../migrations/codegraph/003.sql"),
            include_str!("../migrations/codegraph/004.sql"),
        ],
    }
}

pub fn migrate(connection: &mut Connection, kind: DatabaseKind) -> Result<()> {
    migrate_to(connection, kind, CURRENT_VERSION)
}

/// Apply known migrations through `version`; known older schemas can read newer files.
/// Unknown future migrations are retained, never reversed or rewritten.
pub fn migrate_to(connection: &mut Connection, kind: DatabaseKind, version: u32) -> Result<()> {
    if version > CURRENT_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(
            "unknown migration version".into(),
        ));
    }
    let scripts = migrations(kind);
    let selected: Vec<_> = scripts
        .iter()
        .take(version as usize)
        .map(|s| s.to_string())
        .collect();
    apply_migrations(connection, &selected, Some(kind))
}

fn apply_migrations(
    connection: &mut Connection,
    scripts: &[String],
    kind: Option<DatabaseKind>,
) -> Result<()> {
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY CHECK(version>0), checksum TEXT NOT NULL, applied_at INTEGER NOT NULL DEFAULT(unixepoch()));",
    )?;
    for (index, script) in scripts.iter().enumerate() {
        let version = index as u32 + 1;
        let checksum = format!("{:x}", Sha256::digest(script.as_bytes()));
        let existing: Option<String> = transaction
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version=?1",
                [version],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing != checksum {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "migration checksum mismatch for version {version}"
                )));
            }
        } else {
            transaction.execute_batch(script)?;
            if version == 3 && kind == Some(DatabaseKind::Main) {
                migrate_legacy_vectors_v3(&transaction)?;
            }
            transaction.execute(
                "INSERT INTO schema_migrations(version,checksum) VALUES(?1,?2)",
                rusqlite::params![version, checksum],
            )?;
        }
    }
    let applied_version: u32 = transaction.query_row(
        "SELECT coalesce(max(version),0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    transaction.pragma_update(None, "user_version", applied_version)?;
    transaction.commit()
}

/// Storage representation for an embedding space; callers supply synthetic or real vectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorEncoding {
    Float32,
    Int8,
    Bit,
}
impl VectorEncoding {
    fn label(self) -> &'static str {
        match self {
            Self::Float32 => "float32",
            Self::Int8 => "int8",
            Self::Bit => "bit",
        }
    }
    fn sql_type(self) -> &'static str {
        match self {
            Self::Float32 => "float",
            Self::Int8 => "int8",
            Self::Bit => "bit",
        }
    }
    fn constructor(self) -> &'static str {
        match self {
            Self::Float32 => "vec_f32",
            Self::Int8 => "vec_int8",
            Self::Bit => "vec_quantize_binary",
        }
    }
    fn parse(label: &str) -> Result<Self> {
        match label {
            "float32" => Ok(Self::Float32),
            "int8" => Ok(Self::Int8),
            "bit" => Ok(Self::Bit),
            _ => Err(invalid("unknown vector encoding")),
        }
    }
}
fn invalid(reason: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(reason.into())
}
fn valid_space(id: &str) -> bool {
    !id.is_empty() && id.len() <= 80 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
fn valid_scope(scope: &str) -> bool {
    !scope.trim().is_empty() && scope.len() <= 4096 && !scope.contains('\0')
}

/// Create a float32 index with a mandatory scope partition. Returns `scoped_vectors_<id>`.
/// Legacy `vectors_<id>` tables remain intact for older readers and are migrated separately.
pub fn create_vector_space(connection: &Connection, id: &str, dimensions: u32) -> Result<String> {
    create_quantized_vector_space(connection, id, dimensions, VectorEncoding::Float32)
}
/// Encoding and dimensions are immutable; bit vectors require a multiple of eight dimensions.
pub fn create_quantized_vector_space(
    connection: &Connection,
    id: &str,
    dimensions: u32,
    encoding: VectorEncoding,
) -> Result<String> {
    if !valid_space(id)
        || !(1..=8192).contains(&dimensions)
        || (encoding == VectorEncoding::Bit && !dimensions.is_multiple_of(8))
    {
        return Err(invalid("invalid vector space ID, encoding or dimensions"));
    }
    let table = format!("scoped_vectors_{id}");
    let known: Option<(u32, String)> = connection
        .query_row(
            "SELECT dimensions,element_type FROM vector_indexes WHERE space_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((known_dimensions, known_encoding)) = known {
        if dimensions != known_dimensions || encoding.label() != known_encoding {
            return Err(invalid(
                "vector space dimensions and encoding cannot change",
            ));
        }
        return Ok(table);
    }
    connection.execute_batch("SAVEPOINT create_vector_space")?;
    let result = (|| {
        connection.execute_batch(&format!("CREATE VIRTUAL TABLE {table} USING vec0(vector_key TEXT PRIMARY KEY, scope_id TEXT PARTITION KEY, object_id TEXT, embedding {}[{dimensions}]);",encoding.sql_type()))?;
        connection.execute("INSERT INTO vector_indexes(space_id,table_name,dimensions,element_type) VALUES(?1,?2,?3,?4)",rusqlite::params![id,table,dimensions,encoding.label()])?;
        Ok(())
    })();
    match result {
        Ok(()) => connection.execute_batch("RELEASE create_vector_space")?,
        Err(error) => {
            let _ = connection
                .execute_batch("ROLLBACK TO create_vector_space; RELEASE create_vector_space");
            return Err(error);
        }
    }
    Ok(table)
}
fn vector_index(connection: &Connection, id: &str) -> Result<(String, VectorEncoding)> {
    if !valid_space(id) {
        return Err(invalid("invalid vector space ID"));
    }
    let (table, label): (String, String) = connection.query_row(
        "SELECT table_name,element_type FROM vector_indexes WHERE space_id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if table != format!("scoped_vectors_{id}") {
        return Err(invalid("invalid vector index metadata"));
    }
    Ok((table, VectorEncoding::parse(&label)?))
}
fn vector_key(scope: &str, object: &str) -> Result<String> {
    if !valid_scope(scope) || object.is_empty() || object.len() > 4096 || object.contains('\0') {
        return Err(invalid("explicit scope and object ID required"));
    }
    let mut hash = Sha256::new();
    hash.update(scope.as_bytes());
    hash.update([0]);
    hash.update(object.as_bytes());
    Ok(format!("{:x}", hash.finalize()))
}
/// Insert/replace a caller-provided JSON vector, atomically in the caller's transaction.
/// The same object ID can occur in different scopes without collision.
pub fn put_vector(
    connection: &Connection,
    space: &str,
    scope: &str,
    object: &str,
    vector_json: &str,
) -> Result<()> {
    let key = vector_key(scope, object)?;
    let (table, encoding) = vector_index(connection, space)?;
    connection.execute_batch("SAVEPOINT put_scoped_vector")?;
    let result = (|| {
        connection.execute(&format!("DELETE FROM {table} WHERE vector_key=?1"), [&key])?;
        connection.execute(&format!("INSERT INTO {table}(vector_key,scope_id,object_id,embedding) VALUES(?1,?2,?3,{}(?4))",encoding.constructor()),rusqlite::params![key,scope,object,vector_json])?;
        Ok(())
    })();
    match result {
        Ok(()) => connection.execute_batch("RELEASE put_scoped_vector"),
        Err(error) => {
            let _ = connection
                .execute_batch("ROLLBACK TO put_scoped_vector; RELEASE put_scoped_vector");
            Err(error)
        }
    }
}
/// Delete requires an explicit scope; no cross-scope delete helper exists.
pub fn delete_vector(
    connection: &Connection,
    space: &str,
    scope: &str,
    object: &str,
) -> Result<usize> {
    let key = vector_key(scope, object)?;
    let (table, _) = vector_index(connection, space)?;
    connection.execute(
        &format!("DELETE FROM {table} WHERE vector_key=?1 AND scope_id=?2"),
        rusqlite::params![key, scope],
    )
}
#[derive(Clone, Debug, PartialEq)]
pub struct VectorHit {
    pub scope_id: String,
    pub object_id: String,
    pub distance: f64,
}
/// Search only explicitly authorized scopes. Each KNN is filtered by its actual vec0
/// partition key before ranking; merging per-scope top-k yields the overall top-k.
pub fn query_vectors(
    connection: &Connection,
    space: &str,
    scopes: &[&str],
    vector_json: &str,
    k: u32,
) -> Result<Vec<VectorHit>> {
    if scopes.is_empty()
        || scopes.len() > 64
        || scopes.iter().any(|s| !valid_scope(s))
        || !(1..=4096).contains(&k)
    {
        return Err(invalid("nonempty explicit scopes and bounded k required"));
    }
    let (table, encoding) = vector_index(connection, space)?;
    let sql = format!(
        "SELECT scope_id,object_id,distance FROM {table} WHERE embedding MATCH {}(?1) AND k=?2 AND scope_id=?3 ORDER BY distance",
        encoding.constructor()
    );
    let mut statement = connection.prepare(&sql)?;
    let mut result = Vec::new();
    for scope in scopes
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
    {
        result.extend(
            statement
                .query_map(rusqlite::params![vector_json, k, scope], |row| {
                    Ok(VectorHit {
                        scope_id: row.get(0)?,
                        object_id: row.get(1)?,
                        distance: row.get(2)?,
                    })
                })?
                .collect::<Result<Vec<_>>>()?,
        );
    }
    result.sort_by(|a, b| {
        a.distance
            .total_cmp(&b.distance)
            .then_with(|| a.scope_id.cmp(&b.scope_id))
            .then_with(|| a.object_id.cmp(&b.object_id))
    });
    result.truncate(k as usize);
    Ok(result)
}
/// Atomic migration of derived legacy vector indexes. Authoritative scopes are recovered
/// when unambiguous; otherwise data is quarantined, never implicitly shared as `general`.
fn migrate_legacy_vectors_v3(connection: &Connection) -> Result<()> {
    let mut statement=connection.prepare("SELECT name,sql FROM sqlite_schema WHERE type='table' AND name GLOB 'vectors_*' AND lower(sql) LIKE '%using vec0%'")?;
    let legacy = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>>>()?;
    drop(statement);
    for (table, sql) in legacy {
        let Some(id) = table.strip_prefix("vectors_") else {
            continue;
        };
        if !valid_space(id) {
            return Err(invalid("unsafe legacy vector space name"));
        }
        let compact: String = sql
            .chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect();
        let Some(dimensions) = compact
            .split("embeddingfloat[")
            .nth(1)
            .and_then(|s| s.split(']').next())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            return Err(invalid("unsupported legacy vector representation"));
        };
        let scoped = create_vector_space(connection, id, dimensions)?;
        let mut statement =
            connection.prepare(&format!("SELECT object_id,embedding FROM {table}"))?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (object, embedding) = row?;
            let mut scopes = std::collections::BTreeSet::new();
            for source in [
                "memories",
                "messages",
                "doc_chunks",
                "doc_fragments",
                "documents",
                "tasks",
                "evidence",
            ] {
                let scope: Option<String> = connection
                    .query_row(
                        &format!("SELECT scope_id FROM {source} WHERE id=?1"),
                        [&object],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten();
                if let Some(scope) = scope.filter(|s| valid_scope(s)) {
                    scopes.insert(scope);
                }
            }
            let scope = if scopes.len() == 1 {
                scopes.into_iter().next().unwrap()
            } else {
                "legacy:unscoped".to_string()
            };
            let key = vector_key(&scope, &object)?;
            connection.execute(&format!("INSERT INTO {scoped}(vector_key,scope_id,object_id,embedding) VALUES(?1,?2,?3,?4)"),rusqlite::params![key,scope,object,embedding])?;
        }
    }
    Ok(())
}

/// Rebuild lexical indexes from their authoritative content tables after restore.
/// Code graph content is reconstructible by the future indexer, not fabricated here.
pub fn rebuild_lexical_indexes(connection: &Connection, kind: DatabaseKind) -> Result<()> {
    if kind == DatabaseKind::Main {
        for table in ["messages", "memories", "doc_fragments", "doc_chunks"] {
            let fts = format!("{table}_fts");
            connection.execute_batch(&format!("INSERT INTO {fts}({fts}) VALUES('rebuild');"))?;
        }
    }
    if kind == DatabaseKind::CodeGraph {
        connection.execute_batch("INSERT INTO files_fts(files_fts) VALUES('rebuild'); INSERT INTO nodes_fts(nodes_fts) VALUES('rebuild');")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn main_db() -> Connection {
        let mut db = Connection::open_in_memory().unwrap();
        db.pragma_update(None, "foreign_keys", true).unwrap();
        migrate(&mut db, DatabaseKind::Main).unwrap();
        db
    }

    #[test]
    fn schemas_are_real_and_physically_distinct() {
        let main = main_db();
        for table in [
            "principals",
            "plans",
            "criteria",
            "audit",
            "memories",
            "blob_refs",
            "workflows",
            "embedding_spaces",
        ] {
            assert!(main.prepare(&format!("SELECT * FROM {table}")).is_ok());
        }
        for kind in [DatabaseKind::Telemetry, DatabaseKind::CodeGraph] {
            let mut db = Connection::open_in_memory().unwrap();
            migrate(&mut db, kind).unwrap();
            assert!(db.prepare("SELECT * FROM principals").is_err());
        }
    }

    #[test]
    fn lexical_index_tracks_insert_update_delete_and_rebuild() {
        let db = main_db();
        db.execute(
            "INSERT INTO memories(id,statement) VALUES('mem_a','synthetic iceberg')",
            [],
        )
        .unwrap();
        let hits = || {
            db.query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'iceberg'",
                [],
                |r| r.get::<_, u32>(0),
            )
            .unwrap()
        };
        assert_eq!(hits(), 1);
        db.execute(
            "UPDATE memories SET statement='synthetic forest' WHERE id='mem_a'",
            [],
        )
        .unwrap();
        assert_eq!(hits(), 0);
        rebuild_lexical_indexes(&db, DatabaseKind::Main).unwrap();
        db.execute("DELETE FROM memories WHERE id='mem_a'", [])
            .unwrap();
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'forest'",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn migration_is_atomic_and_old_reader_accepts_expansion() {
        let mut db = Connection::open_in_memory().unwrap();
        migrate_to(&mut db, DatabaseKind::Main, 1).unwrap();
        db.execute(
            "INSERT INTO principals(id,name) VALUES('owner','Synthetic')",
            [],
        )
        .unwrap();
        migrate(&mut db, DatabaseKind::Main).unwrap();
        migrate_to(&mut db, DatabaseKind::Main, 1).unwrap();
        assert_eq!(
            db.query_row("SELECT name FROM principals WHERE id='owner'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "Synthetic"
        );
        let mut empty = Connection::open_in_memory().unwrap();
        assert!(
            apply_migrations(
                &mut empty,
                &[
                    "CREATE TABLE valid(id INTEGER);".into(),
                    "INVALID SQL".into()
                ],
                None
            )
            .is_err()
        );
        assert!(empty.prepare("SELECT * FROM valid").is_err());
        assert!(empty.prepare("SELECT * FROM schema_migrations").is_err());
    }

    #[test]
    fn migration_checksums_reject_modified_history() {
        let mut db = main_db();
        db.execute(
            "UPDATE schema_migrations SET checksum='altered' WHERE version=1",
            [],
        )
        .unwrap();
        assert!(migrate(&mut db, DatabaseKind::Main).is_err());
        assert!(db.prepare("SELECT * FROM principals").is_ok());
    }

    #[test]
    fn blob_references_follow_owner_lifecycle_and_reject_missing_hashes() {
        let db = main_db();
        let hash = "a".repeat(64);
        assert!(
            db.execute(
                "INSERT INTO documents(id,blob_hash) VALUES('doc_bad',?1)",
                [&hash]
            )
            .is_err()
        );
        db.execute(
            "INSERT INTO blobs(hash,size,compressed_size) VALUES(?1,8,12)",
            [&hash],
        )
        .unwrap();
        db.execute(
            "INSERT INTO documents(id,blob_hash) VALUES('doc_a',?1)",
            [&hash],
        )
        .unwrap();
        assert_eq!(
            db.query_row("SELECT ref_count FROM blobs WHERE hash=?1", [&hash], |r| {
                r.get::<_, u32>(0)
            })
            .unwrap(),
            1
        );
        db.execute("DELETE FROM documents WHERE id='doc_a'", [])
            .unwrap();
        assert_eq!(
            db.query_row("SELECT ref_count FROM blobs WHERE hash=?1", [&hash], |r| {
                r.get::<_, u32>(0)
            })
            .unwrap(),
            0
        );
    }

    #[test]
    fn vector_index_search_and_data_share_one_transaction() {
        crate::initialize_sqlite().unwrap();
        let mut db = main_db();
        assert_eq!(
            create_vector_space(&db, "synthetic", 3).unwrap(),
            "scoped_vectors_synthetic"
        );
        assert!(create_vector_space(&db, "synthetic", 4).is_err());
        db.execute(
            "INSERT INTO scoped_vectors_synthetic(vector_key,scope_id,object_id,embedding) VALUES('a','project:a','mem_a','[1,0,0]')",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO scoped_vectors_synthetic(vector_key,scope_id,object_id,embedding) VALUES('b','project:a','mem_b','[0,0,1]')",
            [],
        )
        .unwrap();
        let best: String = db
            .query_row(
                "SELECT object_id FROM scoped_vectors_synthetic WHERE embedding MATCH '[0,0,0.9]' AND k=1 AND scope_id='project:a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(best, "mem_b");
        let transaction = db.transaction().unwrap();
        transaction.execute("INSERT INTO memories(id,statement) VALUES('mem_rollback','synthetic not committed')", []).unwrap();
        transaction.execute("INSERT INTO scoped_vectors_synthetic(vector_key,scope_id,object_id,embedding) VALUES('r','project:a','mem_rollback','[0,1,0]')", []).unwrap();
        transaction.rollback().unwrap();
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM memories WHERE id='mem_rollback'",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM scoped_vectors_synthetic WHERE object_id='mem_rollback'",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            0
        );
    }
}

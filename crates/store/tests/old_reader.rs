//! Compile an actual reader that contains only the baseline migration, then reuse it
//! after the current adapter upgrades the same file. This is a test executable, not a CLI.
use excalibur_store::{Database, DatabaseKind, Priority, StorageConfig};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};

#[tokio::test]
async fn separately_compiled_old_schema_reader_survives_current_expansion() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let migration = manifest.join("migrations/main/001.sql");
    let checksum = format!("{:x}", Sha256::digest(fs::read(&migration).unwrap()));
    let source = format!(
        r##"
use rusqlite::Connection;
const MIGRATION: &str=include_str!({migration:?});
fn main(){{
    let conn=Connection::open(std::env::args().nth(1).unwrap()).unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY CHECK(version>0),checksum TEXT NOT NULL,applied_at INTEGER NOT NULL DEFAULT(unixepoch()));").unwrap();
    let count:i64=conn.query_row("SELECT count(*) FROM schema_migrations WHERE version=1",[],|r|r.get(0)).unwrap();
    if count==0{{
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();conn.execute_batch(MIGRATION).unwrap();
        conn.execute("INSERT INTO schema_migrations(version,checksum) VALUES(1,?1)",[{checksum:?}]).unwrap();
        conn.execute_batch("PRAGMA user_version=1; COMMIT;").unwrap();
        conn.execute("INSERT INTO messages(id,text) VALUES('baseline','old schema data')",[]).unwrap();
    }}
    let actual:String=conn.query_row("SELECT checksum FROM schema_migrations WHERE version=1",[],|r|r.get(0)).unwrap();assert_eq!(actual,{checksum:?});
    let text:String=conn.query_row("SELECT text FROM messages WHERE id='baseline'",[],|r|r.get(0)).unwrap();assert_eq!(text,"old schema data");
    conn.execute("INSERT OR IGNORE INTO messages(id,text) VALUES('old-reader-write','still writable')",[]).unwrap();
}}
"##,
        migration = migration.to_string_lossy().to_string(),
        checksum = checksum
    );
    let source_path = temp.path().join("reader.rs");
    fs::write(&source_path, source).unwrap();
    let deps = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut candidates = fs::read_dir(&deps)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("librusqlite-")
                && p.extension().is_some_and(|s| s == "rlib")
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
    let rusqlite = candidates.last().expect("compiled rusqlite rlib");
    let executable = temp.path().join(if cfg!(windows) {
        "reader.exe"
    } else {
        "reader"
    });
    let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("--edition=2024")
        .arg(&source_path)
        .arg("--extern")
        .arg(format!("rusqlite={}", rusqlite.display()))
        .arg("-L")
        .arg(format!("dependency={}", deps.display()))
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = temp.path().join("excalibur.db");
    assert!(
        Command::new(&executable)
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    db.write(Priority::Normal, |c| {
        assert_eq!(
            c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
            excalibur_store::schema::CURRENT_VERSION as i64
        );
        Ok(())
    })
    .await
    .unwrap();
    db.shutdown().await.unwrap();
    drop(db);
    assert!(
        Command::new(&executable)
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
}

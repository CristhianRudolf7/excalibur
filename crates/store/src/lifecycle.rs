use crate::{
    BlobStore, Error, Result, StorageConfig,
    blobs::{durable_create_dirs, durable_rename, sync_directory},
    schema::{self, DatabaseKind},
};
use excalibur_types::{PrincipalId, ProjectId};
use rusqlite::Connection;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct StoragePaths {
    pub principal_dir: PathBuf,
}
impl StoragePaths {
    /// The host chooses its data root. Typed IDs cannot escape their physical directory.
    pub fn for_principal(root: impl AsRef<Path>, id: PrincipalId) -> Self {
        Self {
            principal_dir: root.as_ref().join(id.to_string()),
        }
    }
    pub fn main(&self) -> PathBuf {
        self.principal_dir.join("excalibur.db")
    }
    pub fn telemetry(&self) -> PathBuf {
        self.principal_dir.join("telemetry.db")
    }
    pub fn codegraph(&self, project: ProjectId) -> PathBuf {
        self.principal_dir
            .join("projects")
            .join(project.to_string())
            .join("codegraph.db")
    }
}
pub(crate) fn configure(
    conn: &Connection,
    kind: DatabaseKind,
    config: &StorageConfig,
    writer: bool,
) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_millis(config.busy_timeout_ms))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(
        None,
        "cache_size",
        -(if writer {
            config.writer_cache_kib
        } else {
            config.reader_cache_kib
        } as i64),
    )?;
    conn.pragma_update(None, "mmap_size", config.mmap_bytes)?;
    if writer {
        let mode = if config.network_filesystem {
            "DELETE"
        } else {
            "WAL"
        };
        let actual: String =
            conn.query_row(&format!("PRAGMA journal_mode={mode}"), [], |r| r.get(0))?;
        if !actual.eq_ignore_ascii_case(mode) {
            return Err(Error::Configuration(
                "requested journal mode unavailable".into(),
            ));
        }
        conn.pragma_update(
            None,
            "synchronous",
            if kind == DatabaseKind::Main {
                "FULL"
            } else {
                "NORMAL"
            },
        )?;
        conn.pragma_update(None, "fullfsync", true)?;
    } else {
        conn.pragma_update(None, "query_only", true)?;
    }
    Ok(())
}
pub(crate) fn quick_check(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA quick_check")?;
    let lines = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if lines != ["ok"] {
        return Err(Error::Corrupt(PathBuf::new()));
    }
    Ok(())
}
pub(crate) fn backup_dir(path: &Path) -> PathBuf {
    path.parent()
        .unwrap()
        .join("backups")
        .join(path.file_name().unwrap())
}
pub(crate) fn vacuum_copy(conn: &Connection, target: &Path) -> Result<()> {
    durable_create_dirs(target.parent().unwrap())?;
    let tmp = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
    // VACUUM INTO accepts an existing empty file and a bound filename.
    conn.execute("VACUUM INTO ?1", [tmp.path().to_string_lossy().as_ref()])?;
    File::open(tmp.path())?.sync_all()?;
    durable_rename(tmp.path(), target)?;
    Ok(())
}
pub(crate) fn backup(conn: &Connection, path: &Path, now: i64) -> Result<()> {
    let dir = backup_dir(path);
    durable_create_dirs(&dir)?;
    let daily = dir.join(format!("daily-{:012}.db", now.div_euclid(86400)));
    if !daily.exists() {
        vacuum_copy(conn, &daily)?;
    }
    let weekly = dir.join(format!("weekly-{:012}.db", now.div_euclid(86400 * 7)));
    if !weekly.exists() {
        vacuum_copy(conn, &weekly)?;
    }
    for (prefix, keep) in [("daily-", 7), ("weekly-", 4)] {
        let mut entries = fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
            .collect::<Vec<_>>();
        entries.sort();
        entries.reverse();
        for old in entries.into_iter().skip(keep) {
            fs::remove_file(old)?;
        }
    }
    let mut updates = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("before-update-")
        })
        .map(|e| e.path())
        .collect::<Vec<_>>();
    updates.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
    updates.reverse();
    for (index, file) in updates.into_iter().enumerate() {
        let modified = fs::metadata(&file)?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if index >= 7 || modified < now - 28 * 86400 {
            fs::remove_file(file)?;
        }
    }
    sync_directory(&dir)?;
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    pub original_path: PathBuf,
    pub snapshot_path: Option<PathBuf>,
    pub snapshot_at: Option<i64>,
    pub recovered_at: i64,
    pub salvage_path: Option<PathBuf>,
    pub salvaged_rows: usize,
    pub unreadable_tables: Vec<String>,
    pub data_loss_possible: bool,
}
fn sqlite_corruption(error: &Error) -> bool {
    matches!(error, Error::Corrupt(_))
        || matches!(error,Error::Sqlite(e) if matches!(e.sqlite_error_code(),Some(rusqlite::ErrorCode::DatabaseCorrupt|rusqlite::ErrorCode::NotADatabase)))
}
fn checked_sqlite(path: &Path) -> Result<()> {
    let c = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    quick_check(&c)
}
/// Bounded salvage preserves readable values separately; it never merges uncertain rows.
fn salvage(original: &Path, target: &Path) -> (usize, Vec<String>) {
    let mut failures = Vec::new();
    let mut count = 0;
    let mut bytes = 0;
    let operation = (|| -> Result<()> {
        let source =
            Connection::open_with_flags(original, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let output = Connection::open(target)?;
        output.execute_batch("CREATE TABLE salvaged_rows(table_name TEXT,row_number INTEGER,row_json TEXT,PRIMARY KEY(table_name,row_number))")?;
        let tables=source.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND sql NOT LIKE '%VIRTUAL TABLE%' ORDER BY name")?.query_map([],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
        for table in tables {
            if count >= 1000 || bytes >= 8 * 1024 * 1024 {
                failures.push("salvage budget exhausted".into());
                break;
            }
            let scan = (|| -> Result<()> {
                let escaped = table.replace('"', "\"\"");
                let mut statement =
                    source.prepare(&format!("SELECT * FROM \"{escaped}\" LIMIT 1000"))?;
                let columns = statement
                    .column_names()
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let mut rows = statement.query([])?;
                let mut row_number = 0;
                while let Some(row) = rows.next()? {
                    if count >= 1000 || bytes >= 8 * 1024 * 1024 {
                        break;
                    }
                    let mut object = serde_json::Map::new();
                    for (i, name) in columns.iter().enumerate() {
                        use rusqlite::types::ValueRef;
                        let value = match row.get_ref(i)? {
                            ValueRef::Null => serde_json::Value::Null,
                            ValueRef::Integer(v) => serde_json::json!(v),
                            ValueRef::Real(v) => serde_json::json!(v),
                            ValueRef::Text(v) => {
                                serde_json::json!({"text_utf8_lossy":String::from_utf8_lossy(v)})
                            }
                            ValueRef::Blob(v) => {
                                if v.len() > 1024 * 1024 {
                                    return Err(Error::Invalid("salvage field budget"));
                                }
                                serde_json::json!({"blob_hex":v.iter().map(|b|format!("{b:02x}")).collect::<String>()})
                            }
                        };
                        object.insert(name.clone(), value);
                    }
                    let text = serde_json::Value::Object(object).to_string();
                    if bytes + text.len() > 8 * 1024 * 1024 {
                        break;
                    }
                    output.execute(
                        "INSERT INTO salvaged_rows VALUES(?1,?2,?3)",
                        rusqlite::params![table, row_number, text],
                    )?;
                    row_number += 1;
                    count += 1;
                    bytes += text.len();
                }
                Ok(())
            })();
            if scan.is_err() {
                failures.push(table);
            }
        }
        Ok(())
    })();
    if operation.is_err() {
        failures.push("unreadable source catalog".into());
    }
    if let Ok(file) = File::open(target) {
        let _ = file.sync_all();
    }
    (count, failures)
}
fn save_report(conn: &Connection, report: &RecoveryReport) -> Result<()> {
    conn.execute("INSERT INTO storage_recoveries(id,original_path,snapshot_path,snapshot_at,recovered_at,salvage_path,salvaged_rows,unreadable_tables,data_loss_possible) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",rusqlite::params![excalibur_types::JobId::new().to_string(),report.original_path.to_string_lossy(),report.snapshot_path.as_ref().map(|p|p.to_string_lossy()),report.snapshot_at,report.recovered_at,report.salvage_path.as_ref().map(|p|p.to_string_lossy()),report.salvaged_rows,serde_json::to_string(&report.unreadable_tables).unwrap(),report.data_loss_possible])?;
    Ok(())
}
pub(crate) fn open_recover(
    path: &Path,
    kind: DatabaseKind,
    config: &StorageConfig,
    blobs: &BlobStore,
) -> Result<(Connection, Option<RecoveryReport>, Vec<crate::BlobIssue>)> {
    let mut recovery = None;
    if path.exists() {
        let corrupt = match checked_sqlite(path) {
            Ok(()) => false,
            Err(error) if sqlite_corruption(&error) => true,
            Err(error) => return Err(error),
        };
        if corrupt {
            let mut candidates = fs::read_dir(backup_dir(path))
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|s| s == "db"))
                .collect::<Vec<_>>();
            candidates.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
            candidates.reverse();
            let candidate = candidates.into_iter().find(|p| checked_sqlite(p).is_ok());
            if candidate.is_none() && kind == DatabaseKind::Main {
                return Err(Error::Corrupt(path.into()));
            }
            let quarantine = path.parent().unwrap().join("recovery");
            durable_create_dirs(&quarantine)?;
            let token = excalibur_types::JobId::new().to_string();
            for suffix in ["", "-wal", "-shm"] {
                let mut original = path.as_os_str().to_os_string();
                original.push(suffix);
                let original = PathBuf::from(original);
                if original.exists() {
                    let saved = quarantine.join(format!("{token}{suffix}"));
                    if suffix.is_empty() && candidate.is_some() {
                        fs::copy(&original, &saved)?;
                        File::open(&saved)?.sync_all()?;
                    } else {
                        durable_rename(&original, &saved)?;
                    }
                }
            }
            let saved = quarantine.join(&token);
            let salvage_path = quarantine.join(format!("{token}.salvage.db"));
            let (salvaged_rows, unreadable_tables) = salvage(&saved, &salvage_path);
            let snapshot_at = candidate
                .as_ref()
                .and_then(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|t| t.as_secs() as i64);
            recovery = Some(RecoveryReport {
                original_path: saved,
                snapshot_path: candidate.clone(),
                snapshot_at,
                recovered_at: crate::unix_seconds(),
                salvage_path: Some(salvage_path),
                salvaged_rows,
                unreadable_tables,
                data_loss_possible: true,
            });
            if let Some(candidate) = candidate {
                let tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
                fs::copy(&candidate, tmp.path())?;
                tmp.as_file().sync_all()?;
                durable_rename(tmp.path(), path)?;
            }
        }
    }
    let mut conn = Connection::open(path)?;
    configure(&conn, kind, config, true)?;
    let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > 0 && version < schema::CURRENT_VERSION {
        vacuum_copy(
            &conn,
            &backup_dir(path).join(format!(
                "before-update-{}.db",
                excalibur_types::JobId::new()
            )),
        )?;
    }
    schema::migrate(&mut conn, kind)?;
    if recovery.is_some() {
        schema::rebuild_lexical_indexes(&conn, kind)?;
    }
    if kind == DatabaseKind::Main {
        conn.execute("INSERT INTO decisions(id,statement,reason) SELECT ?1,'Reserve an interactive reader even on one-core hosts','A strict min(4,cores) pool has no background connection on a one-core host; the minimum is two connections to satisfy the independent interactive lane requirement.' WHERE NOT EXISTS(SELECT 1 FROM decisions WHERE statement='Reserve an interactive reader even on one-core hosts')",[format!("dec_{}",excalibur_types::JobId::new().ulid())])?;
    }
    quick_check(&conn)?;
    let issues = if kind == DatabaseKind::Main {
        if let Some(report) = &recovery {
            save_report(&conn, report)?;
        }
        crate::integrity::startup_sample(&conn, blobs)?
    } else {
        Vec::new()
    };
    Ok((conn, recovery, issues))
}

/// Offline restoration takes the same ownership lock as Database::open. The host must
/// stop/drop all handles first. Source validation and publication finish before unlock.
pub fn restore_backup(path: &Path, snapshot: &Path, kind: DatabaseKind) -> Result<RecoveryReport> {
    use fs2::FileExt;
    crate::initialize_sqlite()?;
    let parent = path
        .parent()
        .ok_or(Error::Invalid("database parent required"))?;
    durable_create_dirs(parent)?;
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".owner");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PathBuf::from(lock_path))?;
    lock.try_lock_exclusive()
        .map_err(|_| Error::AlreadyOpen(path.into()))?;
    let _blob_owner = if kind == DatabaseKind::Main {
        let root = parent.join("blobs");
        durable_create_dirs(&root)?;
        let owner = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".owner"))?;
        owner
            .try_lock_exclusive()
            .map_err(|_| Error::AlreadyOpen(root))?;
        Some(owner)
    } else {
        None
    };
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    fs::copy(snapshot, tmp.path())?;
    {
        let conn = Connection::open(tmp.path())?;
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        quick_check(&conn)?;
        schema::rebuild_lexical_indexes(&conn, kind)?;
    }
    tmp.as_file().sync_all()?;
    let quarantine = parent.join("recovery");
    durable_create_dirs(&quarantine)?;
    let token = excalibur_types::JobId::new();
    for suffix in ["", "-wal", "-shm"] {
        let mut original = path.as_os_str().to_os_string();
        original.push(suffix);
        let original = PathBuf::from(original);
        if original.exists() {
            let saved = quarantine.join(format!("{token}{suffix}"));
            if suffix.is_empty() {
                fs::copy(&original, &saved)?;
                File::open(&saved)?.sync_all()?;
            } else {
                durable_rename(&original, &saved)?;
            }
        }
    }
    durable_rename(tmp.path(), path)?;
    let saved = quarantine.join(token.to_string());
    let salvage_path = quarantine.join(format!("{token}.salvage.db"));
    let (salvaged_rows, unreadable_tables) = salvage(&saved, &salvage_path);
    let report = RecoveryReport {
        original_path: saved,
        snapshot_path: Some(snapshot.into()),
        snapshot_at: fs::metadata(snapshot)?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|t| t.as_secs() as i64),
        recovered_at: crate::unix_seconds(),
        salvage_path: Some(salvage_path),
        salvaged_rows,
        unreadable_tables,
        data_loss_possible: true,
    };
    if kind == DatabaseKind::Main {
        let mut conn = Connection::open(path)?;
        schema::migrate(&mut conn, kind)?;
        save_report(&conn, &report)?;
    }
    Ok(report)
}

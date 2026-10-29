//! Shared storage boundary. Hosts supply paths/mount facts and own startup/shutdown.
//! SQLite handles never leave blocking workers; no network, model, CLI or server dependency.
mod blobs;
mod boundary;
pub use boundary::{BlobOwner, MessageRecord, Store};
mod integrity;
mod lifecycle;
mod maintenance;
pub use integrity::{BlobIssue, BlobState, VerificationBudget, VerificationProgress};
pub use maintenance::{
    MaintenancePolicy, MaintenanceWindow, SchedulerStats, maintenance_scheduler_stats,
};
pub mod schema;
mod writer;

pub use blobs::{BlobHash, BlobIoStats, BlobStore, BlobWindow, PreparedBlob};
pub use excalibur_config::StorageConfig;
pub use lifecycle::{RecoveryReport, StoragePaths, restore_backup};
pub use rusqlite;
pub use schema::DatabaseKind;
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
pub use writer::{Database, Priority, ReadLane, ShortCodeKind, StoreEvent, StoreStats};

#[derive(Debug, Error)]
pub enum Error {
    #[error("storage I/O: {0}")]
    Io(std::io::Error),
    #[error("SQLite: {0}")]
    Sqlite(rusqlite::Error),
    #[error("storage configuration: {0}")]
    Configuration(String),
    #[error("external writer retained the database after bounded retries")]
    Busy,
    #[error("disk full; background writes paused")]
    Full,
    #[error("background writes are paused until space is restored")]
    BackgroundPaused,
    #[error("writer closed or failed")]
    Closed,
    #[error("writer panicked; host must stop and restart storage")]
    WriterPanicked,
    #[error("another store owns {0}")]
    AlreadyOpen(PathBuf),
    #[error("database is corrupt and no valid backup is available: {0}")]
    Corrupt(PathBuf),
    #[error("blob missing or corrupt: {0}")]
    Blob(String),
    #[error("blob is missing: {0}")]
    BlobMissing(String),
    #[error("blob content is corrupt: {0}")]
    BlobCorrupt(String),
    #[error("blob exceeds the caller size limit: {0}")]
    BlobTooLarge(String),
    #[error("blob has no registered metadata: {0}")]
    BlobMetadataMissing(String),
    #[error("blob belongs to another storage namespace")]
    BlobOwnerMismatch,
    #[error("operation requires the principal database")]
    PrincipalOnly,
    #[error("transaction control failed during {phase}; original={original}; control={control}")]
    TransactionControl {
        phase: &'static str,
        original: Box<Error>,
        control: Box<Error>,
    },
    #[error("group was rolled back: {0}")]
    GroupAborted(std::sync::Arc<Error>),
    #[error("invalid identifier or argument: {0}")]
    Invalid(&'static str),
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::StorageFull {
            Self::Full
        } else {
            Self::Io(e)
        }
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        match e.sqlite_error_code() {
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
                Self::Busy
            }
            Some(rusqlite::ErrorCode::DiskFull) => Self::Full,
            _ => Self::Sqlite(e),
        }
    }
}
pub type Result<T> = std::result::Result<T, Error>;
pub fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Register the statically linked sqlite-vec extension once for every connection.
pub fn initialize_sqlite() -> Result<()> {
    static INIT: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    let code = *INIT.get_or_init(|| {
        // sqlite-vec's documented C entry point has SQLite's extension ABI.
        unsafe {
            let entry = std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::ffi::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> i32,
            >(sqlite_vec::sqlite3_vec_init as *const ());
            rusqlite::ffi::sqlite3_auto_extension(Some(entry))
        }
    });
    if code != rusqlite::ffi::SQLITE_OK {
        return Err(Error::Configuration(
            "sqlite-vec registration failed".into(),
        ));
    }
    Ok(())
}

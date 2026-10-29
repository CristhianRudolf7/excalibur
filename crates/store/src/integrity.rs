use crate::{BlobStore, Error, Result, unix_seconds};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobState {
    Ok,
    Missing,
    Corrupt,
    MetadataMissing,
    Unavailable,
}
impl BlobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Corrupt => "corrupt",
            Self::MetadataMissing => "metadata_missing",
            Self::Unavailable => "unavailable",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobIssue {
    pub hash: String,
    pub state: BlobState,
    pub checked_at: i64,
    pub detail: String,
    // None: unguarded startup/publication observation; Some(None): metadata absent.
    pub(crate) generation: Option<Option<String>>,
}
impl BlobIssue {
    pub(crate) fn new(hash: String, state: BlobState, detail: impl Into<String>) -> Self {
        Self {
            hash,
            state,
            checked_at: unix_seconds(),
            detail: detail.into(),
            generation: None,
        }
    }
}
pub(crate) fn record(c: &Connection, issue: &BlobIssue) -> Result<bool> {
    if let Some(expected) = &issue.generation {
        let current: Option<String> = c
            .query_row(
                "SELECT content_generation FROM blobs WHERE hash=?1",
                [&issue.hash],
                |r| r.get(0),
            )
            .optional()?;
        if &current != expected {
            return Ok(false);
        }
    }
    if issue.state == BlobState::MetadataMissing
        && c.query_row(
            "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash=?1)",
            [&issue.hash],
            |r| r.get::<_, bool>(0),
        )?
    {
        return Ok(false);
    }
    let previous: Option<String> = c
        .query_row(
            "SELECT state FROM blob_integrity WHERE hash=?1",
            [&issue.hash],
            |r| r.get(0),
        )
        .optional()?;
    c.execute("INSERT INTO blob_integrity(hash,state,checked_at,detail) VALUES(?1,?2,?3,?4) ON CONFLICT(hash) DO UPDATE SET state=excluded.state,checked_at=excluded.checked_at,detail=excluded.detail",rusqlite::params![issue.hash,issue.state.as_str(),issue.checked_at,issue.detail])?;
    if issue.state == BlobState::Ok {
        c.execute(
            "UPDATE blob_incidents SET resolved_at=?2 WHERE hash=?1 AND resolved_at IS NULL",
            rusqlite::params![issue.hash, issue.checked_at],
        )?;
    } else if previous.as_deref() != Some(issue.state.as_str()) {
        c.execute(
            "INSERT INTO blob_incidents(id,hash,state,detected_at) VALUES(?1,?2,?3,?4)",
            rusqlite::params![
                excalibur_types::JobId::new().to_string(),
                issue.hash,
                issue.state.as_str(),
                issue.checked_at
            ],
        )?;
    }
    Ok(true)
}
/// Cheap bounded startup sample. It never reads or decompresses blob contents.
pub(crate) fn startup_sample(c: &Connection, blobs: &BlobStore) -> Result<Vec<BlobIssue>> {
    let mut stmt=c.prepare("SELECT DISTINCT r.hash,b.compressed_size FROM blob_refs r LEFT JOIN blobs b ON b.hash=r.hash ORDER BY r.hash LIMIT 32")?;
    let mut issues = Vec::new();
    for row in stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<u64>>(1)?))
    })? {
        let (hash, size) = row?;
        let issue = if let Some(size) = size {
            match hash.parse::<crate::BlobHash>() {
                Err(_) => Some(BlobIssue::new(
                    hash,
                    BlobState::Corrupt,
                    "invalid blob identifier",
                )),
                Ok(parsed) => match std::fs::metadata(blobs.path(&parsed)) {
                    Ok(meta) if !meta.is_file() || meta.len() != size => Some(BlobIssue::new(
                        hash,
                        BlobState::Corrupt,
                        "compressed size/type mismatch",
                    )),
                    Ok(_) => None,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        Some(BlobIssue::new(hash, BlobState::Missing, "file absent"))
                    }
                    Err(e) => Some(BlobIssue::new(hash, BlobState::Unavailable, e.to_string())),
                },
            }
        } else {
            Some(BlobIssue::new(
                hash,
                BlobState::MetadataMissing,
                "blob reference has no metadata",
            ))
        };
        if let Some(issue) = issue {
            record(c, &issue)?;
            issues.push(issue);
        }
    }
    Ok(issues)
}
#[derive(Clone, Copy, Debug)]
pub struct VerificationBudget {
    pub max_blobs: usize,
    pub max_bytes: u64,
}
impl Default for VerificationBudget {
    fn default() -> Self {
        Self {
            max_blobs: 8,
            max_bytes: 4 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct VerificationProgress {
    pub bytes_read: u64,
    pub completed: usize,
    pub issues: Vec<BlobIssue>,
    pub pending: bool,
}
type Decoder = zstd::stream::read::Decoder<'static, BufReader<File>>;
struct Active {
    hash: String,
    size: u64,
    bytes: u64,
    reader: Decoder,
    hasher: Sha256,
    generation: Option<String>,
}
#[derive(Default)]
pub(crate) struct Verifier {
    last: String,
    active: Option<Active>,
}
impl Verifier {
    pub(crate) fn step(
        &mut self,
        path: &Path,
        blobs: &BlobStore,
        budget: VerificationBudget,
    ) -> Result<VerificationProgress> {
        if budget.max_blobs == 0
            || budget.max_blobs > 1024
            || budget.max_bytes == 0
            || budget.max_bytes > 64 * 1024 * 1024
        {
            return Err(Error::Invalid("invalid verification budget"));
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut result = VerificationProgress::default();
        let mut buffer = [0u8; 65536];
        while result.completed < budget.max_blobs && result.bytes_read < budget.max_bytes {
            if self.active.is_none() {
                let next: Option<(String, u64, bool,Option<String>)> = conn
                    .query_row(
                        "SELECT hash,size,metadata_exists,content_generation FROM (SELECT hash,size,1 AS metadata_exists,content_generation FROM blobs UNION SELECT r.hash,0,0,NULL FROM blob_refs r LEFT JOIN blobs b ON b.hash=r.hash WHERE b.hash IS NULL) WHERE hash>?1 ORDER BY hash LIMIT 1",
                        [&self.last],
                        |r| Ok((r.get(0)?, r.get(1)?,r.get(2)?,r.get(3)?)),
                    )
                    .optional()?;
                let Some((hash, size, metadata_exists, generation)) = next else {
                    self.last.clear();
                    result.pending = false;
                    return Ok(result);
                };
                self.last = hash.clone();
                if !metadata_exists {
                    result.completed += 1;
                    result.issues.push(BlobIssue::new(
                        hash,
                        BlobState::MetadataMissing,
                        "blob reference has no metadata",
                    ));
                    continue;
                }
                let reader = (|| -> Result<Decoder> {
                    let parsed = hash.parse()?;
                    let file = File::open(blobs.path(&parsed)).map_err(|e| {
                        if e.kind() == std::io::ErrorKind::NotFound {
                            Error::BlobMissing(hash.clone())
                        } else {
                            e.into()
                        }
                    })?;
                    let mut decoder =
                        Decoder::new(file).map_err(|_| Error::BlobCorrupt(hash.clone()))?;
                    decoder
                        .window_log_max(23)
                        .map_err(|_| Error::BlobCorrupt(hash.clone()))?;
                    Ok(decoder)
                })();
                match reader {
                    Ok(reader) => {
                        self.active = Some(Active {
                            hash,
                            size,
                            bytes: 0,
                            reader,
                            hasher: Sha256::new(),
                            generation,
                        })
                    }
                    Err(error) => {
                        result.completed += 1;
                        let mut issue = issue_from_error(hash, &error);
                        issue.generation = Some(generation);
                        result.issues.push(issue);
                        continue;
                    }
                }
            }
            let active = self.active.as_mut().unwrap();
            let capacity = (budget.max_bytes - result.bytes_read).min(buffer.len() as u64) as usize;
            match active.reader.read(&mut buffer[..capacity]) {
                Ok(0) => {
                    let active = self.active.take().unwrap();
                    let okay = active.bytes == active.size
                        && format!("{:x}", active.hasher.finalize()) == active.hash;
                    result.completed += 1;
                    let mut issue = BlobIssue::new(
                        active.hash,
                        if okay {
                            BlobState::Ok
                        } else {
                            BlobState::Corrupt
                        },
                        if okay {
                            "verified"
                        } else {
                            "hash or size mismatch"
                        },
                    );
                    issue.generation = Some(active.generation);
                    result.issues.push(issue);
                }
                Ok(count) => {
                    active.hasher.update(&buffer[..count]);
                    active.bytes += count as u64;
                    result.bytes_read += count as u64;
                    if active.bytes > active.size {
                        let active = self.active.take().unwrap();
                        result.completed += 1;
                        let mut issue = BlobIssue::new(
                            active.hash,
                            BlobState::Corrupt,
                            "decompressed size mismatch",
                        );
                        issue.generation = Some(active.generation);
                        result.issues.push(issue);
                    }
                }
                Err(_) => {
                    let active = self.active.take().unwrap();
                    result.completed += 1;
                    let mut issue = BlobIssue::new(
                        active.hash,
                        BlobState::Corrupt,
                        "invalid compressed content",
                    );
                    issue.generation = Some(active.generation);
                    result.issues.push(issue);
                }
            }
        }
        result.pending = true;
        Ok(result)
    }
}
pub(crate) fn issue_from_error(hash: String, error: &Error) -> BlobIssue {
    BlobIssue::new(
        hash,
        match error {
            Error::BlobMissing(_) => BlobState::Missing,
            Error::BlobCorrupt(_) => BlobState::Corrupt,
            _ => BlobState::Unavailable,
        },
        error.to_string(),
    )
}

//! Durable quarantine for rejected files: content-addressed objects plus an append-only
//! JSON Lines manifest. Objects are written with mode 0600 inside 0700 directories, so
//! nothing quarantined is ever executable.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub const MANIFEST: &str = "manifest.jsonl";
const OBJECTS: &str = "objects";
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineRecord {
    pub sha256: String,
    pub size: u64,
    /// Stable machine code of the rejection reason.
    pub reason_code: String,
    pub reason: String,
    pub original_name: Option<String>,
    pub declared_type: String,
    pub detected_type: String,
    /// Object path relative to the quarantine directory.
    pub object: String,
    pub recorded_at_ms: u64,
}

/// Item submitted to [`Quarantine::store`].
#[derive(Clone, Copy, Debug)]
pub struct QuarantineItem<'a> {
    pub bytes: &'a [u8],
    pub sha256: &'a str,
    pub reason_code: &'a str,
    pub reason: &'a str,
    pub original_name: Option<&'a str>,
    pub declared_type: &'a str,
    pub detected_type: &'a str,
}

#[derive(Debug)]
pub struct Quarantine {
    dir: PathBuf,
    lock: Mutex<()>,
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}
#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

fn private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a real directory", path.display()),
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path)?;
        }
        Err(e) => return Err(e),
    }
    restrict(path, 0o700)
}

fn open_private(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn valid_hash(sha256: &str) -> bool {
    sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Quarantine {
    /// Opens or creates the quarantine directory. The directory itself must not be a symlink.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        private_dir(&dir)?;
        private_dir(&dir.join(OBJECTS))?;
        Ok(Self {
            dir,
            lock: Mutex::new(()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join(MANIFEST)
    }

    pub fn object_path(&self, sha256: &str) -> PathBuf {
        self.dir.join(OBJECTS).join(format!("{sha256}.bin"))
    }

    /// Stores the bytes (once per content hash) and appends a durable manifest record.
    pub fn store(&self, item: QuarantineItem<'_>) -> io::Result<QuarantineRecord> {
        if !valid_hash(item.sha256) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid sha256",
            ));
        }
        let _local = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let objects = self.dir.join(OBJECTS);
        let object = self.object_path(item.sha256);
        if fs::symlink_metadata(&object).is_err() {
            let tmp = objects.join(format!(
                ".tmp-{}-{}-{}",
                item.sha256,
                std::process::id(),
                TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_file(&tmp);
            let mut file =
                open_private(OpenOptions::new().write(true).create_new(true)).open(&tmp)?;
            file.write_all(item.bytes)?;
            file.sync_all()?;
            drop(file);
            restrict(&tmp, 0o600)?;
            fs::rename(&tmp, &object)?;
            sync_dir(&objects)?;
        }
        // Never leave an executable bit, even on an object written by an older process.
        restrict(&object, 0o600)?;

        let record = QuarantineRecord {
            sha256: item.sha256.to_string(),
            size: item.bytes.len() as u64,
            reason_code: item.reason_code.to_string(),
            reason: item.reason.to_string(),
            original_name: item.original_name.map(str::to_string),
            declared_type: item.declared_type.to_string(),
            detected_type: item.detected_type.to_string(),
            object: format!("{OBJECTS}/{}.bin", item.sha256),
            recorded_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };
        let mut line = serde_json::to_vec(&record).map_err(io::Error::other)?;
        line.push(b'\n');

        let manifest = self.manifest_path();
        let created = fs::symlink_metadata(&manifest).is_err();
        let mut file =
            open_private(OpenOptions::new().read(true).write(true).create(true)).open(&manifest)?;
        // Cross-process exclusion for torn-tail repair and append.
        file.lock()?;
        repair_tail(&mut file)?;
        file.seek(SeekFrom::End(0))?;
        file.write_all(&line)?;
        file.sync_all()?;
        file.unlock()?;
        restrict(&manifest, 0o600)?;
        if created {
            sync_dir(&self.dir)?;
        }
        Ok(record)
    }

    /// Reads every complete manifest record. A torn final line left by an interrupted append
    /// is ignored; corruption elsewhere is an error.
    pub fn records(&self) -> io::Result<Vec<QuarantineRecord>> {
        let mut text = String::new();
        match File::open(self.manifest_path()) {
            Ok(mut f) => {
                f.lock_shared()?;
                f.read_to_string(&mut text)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        }
        let complete = match text.rfind('\n') {
            Some(i) => &text[..=i],
            None => "",
        };
        complete
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            })
            .collect()
    }
}

/// Truncates an incomplete trailing line so the next append starts on a fresh line.
fn repair_tail(file: &mut File) -> io::Result<()> {
    let len = file.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }
    file.seek(SeekFrom::Start(0))?;
    let mut all = Vec::new();
    file.read_to_end(&mut all)?;
    let keep = all.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    file.set_len(keep as u64)?;
    file.sync_all()
}

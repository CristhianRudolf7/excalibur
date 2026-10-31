//! Durable quarantine for rejected files: content-addressed objects plus an append-only
//! JSON Lines manifest. Objects are written with mode 0600 inside 0700 directories, so
//! nothing quarantined is ever executable.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
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

/// Temporary objects older than this are leftovers of an interrupted write and are removed
/// when the quarantine is opened.
pub const STALE_TEMP_AGE: Duration = Duration::from_secs(10 * 60);
const MAX_NAME_BYTES: usize = 1024;
const MAX_TYPE_BYTES: usize = 256;
const MAX_REASON_BYTES: usize = 2048;

/// Changes permissions through an open handle (fchmod), never through a path that could be
/// swapped for a symlink.
#[cfg(unix)]
fn restrict(file: &File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))
}
#[cfg(not(unix))]
fn restrict(_file: &File, _mode: u32) -> io::Result<()> {
    Ok(())
}

fn not_real(path: &Path, what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{} is not a real {what}", path.display()),
    )
}

#[cfg(unix)]
fn same_inode(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(not(unix))]
fn same_inode(_a: &fs::Metadata, _b: &fs::Metadata) -> bool {
    true
}

/// Creates or validates a private directory without following a symlink at its final
/// component, then restricts it through a handle to that same inode.
fn private_dir(path: &Path) -> io::Result<()> {
    let created = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(not_real(path, "directory"));
        }
        Ok(_) => false,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path)?;
            true
        }
        Err(e) => return Err(e),
    };
    let link = fs::symlink_metadata(path)?;
    let handle = File::open(path)?;
    let opened = handle.metadata()?;
    if link.file_type().is_symlink() || !opened.is_dir() || !same_inode(&link, &opened) {
        return Err(not_real(path, "directory"));
    }
    restrict(&handle, 0o700)?;
    if created && let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        sync_dir(parent)?;
    }
    Ok(())
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

/// Truncates untrusted text at a character boundary.
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// True when `path` is a regular file (not a symlink) whose content hashes to `sha256`.
fn verified_object(path: &Path, sha256: &str) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {
            let mut file = File::open(path)?;
            if !same_inode(&meta, &file.metadata()?) {
                return Ok(false);
            }
            let mut hasher = Sha256::new();
            io::copy(&mut file, &mut hasher)?;
            let actual: String = hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if actual != sha256 {
                return Ok(false);
            }
            restrict(&file, 0o600)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn remove_stale_temps(objects: &Path) -> io::Result<()> {
    let now = SystemTime::now();
    for entry in fs::read_dir(objects)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with(".tmp-") {
            continue;
        }
        let meta = fs::symlink_metadata(entry.path())?;
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or_default();
        if meta.file_type().is_symlink() || (meta.is_file() && age >= STALE_TEMP_AGE) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

impl Quarantine {
    /// Opens or creates the quarantine directory. Neither it nor its `objects` directory may
    /// be a symlink; leftover temporary objects from interrupted writes are removed. Parent
    /// directories are created as given and are trusted host configuration.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        private_dir(&dir)?;
        let objects = dir.join(OBJECTS);
        private_dir(&objects)?;
        remove_stale_temps(&objects)?;
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

    /// Stores the bytes (once per content hash) and appends a durable manifest record. An
    /// existing object is reused only when it is a regular file whose content still matches
    /// its hash; a symlink, a non-file or a corrupted object is replaced atomically.
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
        if !verified_object(&object, item.sha256)? {
            let tmp = objects.join(format!(
                ".tmp-{}-{}-{}",
                item.sha256,
                std::process::id(),
                TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file =
                open_private(OpenOptions::new().write(true).create_new(true)).open(&tmp)?;
            let written = restrict(&file, 0o600)
                .and_then(|()| file.write_all(item.bytes))
                .and_then(|()| file.sync_all())
                // rename replaces a planted symlink itself, never its target.
                .and_then(|()| fs::rename(&tmp, &object));
            if let Err(e) = written {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
            sync_dir(&objects)?;
        }

        let record = QuarantineRecord {
            sha256: item.sha256.to_string(),
            size: item.bytes.len() as u64,
            reason_code: bounded(item.reason_code, MAX_TYPE_BYTES),
            reason: bounded(item.reason, MAX_REASON_BYTES),
            original_name: item.original_name.map(|n| bounded(n, MAX_NAME_BYTES)),
            declared_type: bounded(item.declared_type, MAX_TYPE_BYTES),
            detected_type: bounded(item.detected_type, MAX_TYPE_BYTES),
            object: format!("{OBJECTS}/{}.bin", item.sha256),
            recorded_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };
        let mut line = serde_json::to_vec(&record).map_err(io::Error::other)?;
        line.push(b'\n');

        let manifest = self.manifest_path();
        let created = match fs::symlink_metadata(&manifest) {
            Ok(meta) if !meta.file_type().is_file() => return Err(not_real(&manifest, "file")),
            Ok(_) => false,
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(e) => return Err(e),
        };
        let mut file =
            open_private(OpenOptions::new().read(true).write(true).create(true)).open(&manifest)?;
        if !file.metadata()?.is_file() {
            return Err(not_real(&manifest, "file"));
        }
        // Cross-process exclusion for torn-tail repair and append.
        file.lock()?;
        restrict(&file, 0o600)?;
        repair_tail(&mut file)?;
        file.seek(SeekFrom::End(0))?;
        file.write_all(&line)?;
        file.sync_all()?;
        file.unlock()?;
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

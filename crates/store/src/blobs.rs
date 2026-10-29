use crate::{Error, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobHash(String);
impl BlobHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl FromStr for BlobHash {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        if s.len() != 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Invalid("invalid SHA-256"));
        }
        Ok(Self(s.into()))
    }
}
/// Prepared outside the writer: compression and most I/O cannot occupy its queue.
/// Uncommitted temporary files and published orphans are safe to collect after 24 hours.
pub struct PreparedBlob {
    pub hash: BlobHash,
    pub size: u64,
    pub compressed_size: u64,
    temp: tempfile::NamedTempFile,
    root: PathBuf,
}
#[derive(Clone, Debug)]
pub struct BlobStore {
    root: PathBuf,
    stats: Arc<IoCounters>,
}
#[derive(Debug, Default)]
struct IoCounters {
    decoded_bytes: AtomicU64,
    verified_reads: AtomicU64,
}
/// Direct verified-access reads; incremental scans report VerificationProgress separately.
#[derive(Clone, Copy, Debug)]
pub struct BlobIoStats {
    pub decoded_bytes: u64,
    pub verified_reads: u64,
}
/// A verified bounded byte window. Offsets count uncompressed bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobWindow {
    pub bytes: Vec<u8>,
    pub offset: u64,
    pub total_bytes: u64,
}
impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(Error::Invalid("blob root must be absolute"));
        }
        durable_create_dirs(&root.join("staging"))?;
        durable_create_dirs(&root.join("sha256"))?;
        sync_directory(&root)?;
        if let Some(parent) = root.parent() {
            sync_directory(parent)?;
        }
        Ok(Self {
            root,
            stats: Arc::new(IoCounters::default()),
        })
    }
    pub fn io_stats(&self) -> BlobIoStats {
        BlobIoStats {
            decoded_bytes: self.stats.decoded_bytes.load(Ordering::Relaxed),
            verified_reads: self.stats.verified_reads.load(Ordering::Relaxed),
        }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn path(&self, hash: &BlobHash) -> PathBuf {
        self.root
            .join("sha256")
            .join(&hash.0[..2])
            .join(&hash.0[2..])
    }
    /// Call on a blocking worker, not a Tokio runtime thread.
    pub fn prepare(&self, bytes: &[u8]) -> Result<PreparedBlob> {
        self.prepare_reader(bytes)
    }
    /// Streaming form for documents and host-controlled interruption tests.
    pub fn prepare_reader(&self, mut source: impl Read) -> Result<PreparedBlob> {
        let mut temp = tempfile::NamedTempFile::new_in(self.root.join("staging"))?;
        fs2::FileExt::lock_exclusive(temp.as_file())?;
        let mut hasher = Sha256::new();
        let mut size = 0;
        let mut encoder = zstd::stream::Encoder::new(temp.as_file_mut(), 3)?;
        let mut buffer = [0u8; 65536];
        loop {
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            encoder.write_all(&buffer[..count])?;
            size += count as u64;
        }
        encoder.finish()?;
        temp.as_file().sync_all()?;
        let compressed_size = temp.as_file().metadata()?.len();
        Ok(PreparedBlob {
            hash: BlobHash(format!("{:x}", hasher.finalize())),
            size,
            compressed_size,
            temp,
            root: self.root.clone(),
        })
    }
    /// Must execute on the same writer that commits references and performs GC.
    pub(crate) fn publish(&self, prepared: PreparedBlob) -> Result<(BlobHash, u64, u64)> {
        if prepared.root != self.root {
            return Err(Error::BlobOwnerMismatch);
        }
        let path = self.path(&prepared.hash);
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent)?;
        sync_directory(&self.root.join("sha256"))?;
        // Publish freshly prepared, known content, including duplicates/repairs.
        // No read, hashing or decompression belongs to the writer transaction.
        durable_rename(prepared.temp.path(), &path)?;
        Ok((prepared.hash, prepared.size, prepared.compressed_size))
    }
    pub fn read(&self, hash: &BlobHash, max_bytes: u64) -> Result<Vec<u8>> {
        let file = File::open(self.path(hash)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::BlobMissing(hash.0.clone())
            } else {
                e.into()
            }
        })?;
        self.stats.verified_reads.fetch_add(1, Ordering::Relaxed);
        let mut decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|_| Error::BlobCorrupt(hash.0.clone()))?;
        decoder
            .window_log_max(23)
            .map_err(|_| Error::BlobCorrupt(hash.0.clone()))?;
        let mut data = Vec::new();
        let decoded = decoder
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut data);
        self.stats
            .decoded_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        decoded.map_err(|_| Error::BlobCorrupt(hash.0.clone()))?;
        if data.len() as u64 > max_bytes {
            return Err(Error::BlobTooLarge(hash.0.clone()));
        }
        if format!("{:x}", Sha256::digest(&data)) != hash.0 {
            return Err(Error::BlobCorrupt(hash.0.clone()));
        }
        Ok(data)
    }
    /// Decode/hash the full registered content with a fixed 64 KiB buffer while
    /// retaining at most a 1 MiB window. Each request is O(total uncompressed bytes),
    /// not random access; zstd's decoder window is separately capped at 8 MiB.
    pub(crate) fn read_window(
        &self,
        hash: &BlobHash,
        expected_size: u64,
        offset: u64,
        max_bytes: usize,
        mut inspector: Option<Box<dyn excalibur_types::runtime::DurableByteInspector>>,
    ) -> Result<BlobWindow> {
        if !(1..=1024 * 1024).contains(&max_bytes) {
            return Err(Error::Invalid("blob window must be 1..=1048576 bytes"));
        }
        let file = File::open(self.path(hash)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::BlobMissing(hash.0.clone())
            } else {
                e.into()
            }
        })?;
        self.stats.verified_reads.fetch_add(1, Ordering::Relaxed);
        let mut decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|_| Error::BlobCorrupt(hash.0.clone()))?;
        decoder
            .window_log_max(23)
            .map_err(|_| Error::BlobCorrupt(hash.0.clone()))?;
        let offset = offset.min(expected_size);
        let end = offset.saturating_add(max_bytes as u64).min(expected_size);
        let mut bytes = Vec::with_capacity((end - offset) as usize);
        let mut buffer = [0u8; 65536];
        let mut total = 0u64;
        let mut hasher = Sha256::new();
        loop {
            let count = decoder
                .read(&mut buffer)
                .map_err(|_| Error::BlobCorrupt(hash.0.clone()))?;
            if count == 0 {
                break;
            }
            self.stats
                .decoded_bytes
                .fetch_add(count as u64, Ordering::Relaxed);
            let next = total
                .checked_add(count as u64)
                .ok_or_else(|| Error::BlobCorrupt(hash.0.clone()))?;
            if next > expected_size {
                return Err(Error::BlobCorrupt(hash.0.clone()));
            }
            hasher.update(&buffer[..count]);
            if let Some(inspector) = &mut inspector {
                inspector
                    .inspect(&buffer[..count])
                    .map_err(|_| Error::BlobAccessDenied)?;
            }
            if next > offset && total < end {
                let from = offset.max(total) - total;
                let until = end.min(next) - total;
                bytes.extend_from_slice(&buffer[from as usize..until as usize]);
            }
            total = next;
        }
        if total != expected_size || format!("{:x}", hasher.finalize()) != hash.0 {
            return Err(Error::BlobCorrupt(hash.0.clone()));
        }
        if let Some(inspector) = &mut inspector {
            inspector.finish().map_err(|_| Error::BlobAccessDenied)?;
        }
        Ok(BlobWindow {
            bytes,
            offset,
            total_bytes: total,
        })
    }
}
pub(crate) fn durable_create_dirs(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    for ancestor in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        sync_directory(ancestor)?;
    }
    Ok(())
}
#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}
#[cfg(not(windows))]
pub(crate) fn durable_rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)?;
    sync_directory(to.parent().ok_or(Error::Invalid("no parent"))?)
}
#[cfg(windows)]
pub(crate) fn durable_rename(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // Windows flushes the move through its native durable-rename flag.
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

use crate::{
    BlobHash, BlobStore, DatabaseKind, Error, PreparedBlob, Result, StorageConfig, lifecycle,
};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags};
use std::{
    any::Any,
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, oneshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Priority {
    Interactive = 0,
    Journal = 1,
    Normal = 2,
    Background = 3,
}
#[derive(Clone, Copy, Debug)]
pub enum ReadLane {
    Interactive,
    Background,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreEvent {
    BusyRetry(u32),
    DiskFull,
    BackgroundResumed,
    WriterFailed,
    Recovered,
    NetworkJournalDegraded,
    WalLarge(u64),
    Checkpoint(&'static str),
    Backup,
    BlobGarbageCollected(usize),
    RebuildRequired(DatabaseKind),
    MaintenanceFailed(String),
    BlobIntegrity(crate::BlobIssue),
    RecoveryInfo(crate::RecoveryReport),
    ForeignKeyIssue {
        table: String,
        rowid: Option<i64>,
        parent: String,
    },
}
#[derive(Clone, Debug, Default)]
pub struct StoreStats {
    pub committed: [u64; 4],
    pub failed: [u64; 4],
    pub max_queue_wait_micros: [u64; 4],
    pub max_completion_micros: [u64; 4],
    pub busy_retries: u64,
    pub groups: u64,
    pub max_group_operations: usize,
}
type Output = Box<dyn Any + Send>;
type Work = Box<dyn FnOnce(&Connection) -> Result<Output> + Send>;
struct Job {
    work: Work,
    response: oneshot::Sender<Result<Output>>,
    priority: Priority,
    queued: Instant,
    _permit: OwnedSemaphorePermit,
    transaction: bool,
}
struct Queue {
    lanes: [VecDeque<Job>; 4],
    closed: bool,
}
struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    slots: [Arc<Semaphore>; 4],
    events: broadcast::Sender<StoreEvent>,
    stats: Mutex<StoreStats>,
    paused: AtomicBool,
    failed: AtomicBool,
    stopping: AtomicBool,
}
struct Readers {
    interactive: Arc<Mutex<Connection>>,
    background: Arc<Mutex<Vec<Connection>>>,
    foreground_slots: Arc<Semaphore>,
    background_slots: Arc<Semaphore>,
    active: Arc<(Mutex<usize>, Condvar)>,
}
struct ReaderActivity(Arc<(Mutex<usize>, Condvar)>);
impl Drop for ReaderActivity {
    fn drop(&mut self) {
        let mut active = self.0.0.lock().unwrap();
        *active -= 1;
        self.0.1.notify_all();
    }
}
struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
struct Inner {
    shared: Arc<Shared>,
    readers: Readers,
    join: Mutex<Option<thread::JoinHandle<()>>>,
    path: PathBuf,
    blobs: BlobStore,
    startup_events: Vec<StoreEvent>,
    kind: DatabaseKind,
    config: StorageConfig,
    maintenance_lock: Arc<Mutex<()>>,
    maintenance_serial: tokio::sync::Mutex<()>,
    scheduler_registration: Mutex<Option<u64>>,
    maintenance_allowed: AtomicBool,
    verifier: Arc<Mutex<crate::integrity::Verifier>>,
    _lock: File,
    _blob_lock: Option<File>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        crate::maintenance::unregister(
            self.scheduler_registration
                .get_mut()
                .unwrap_or_else(|p| p.into_inner())
                .take(),
        );
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .closed = true;
        for slots in &self.shared.slots {
            slots.close();
        }
        self.shared.wake.notify_all();
        if let Some(join) = self
            .join
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = join.join();
        }
        // CLOEXEC does not close inherited descriptors until exec. The final
        // Rust owner releases advisory ownership explicitly, so a concurrently
        // forked non-owner cannot keep the file description locked in that gap.
        let _ = fs2::FileExt::unlock(&self._lock);
        if let Some(lock) = &self._blob_lock {
            let _ = fs2::FileExt::unlock(lock);
        }
    }
}
#[derive(Clone)]
pub struct Database {
    inner: Arc<Inner>,
}
impl Database {
    /// Blocking startup: validates and recovers files, registers extensions and migrates.
    /// Only one Database may own a path. Clones share the same writer and pools.
    pub fn open(path: impl AsRef<Path>, kind: DatabaseKind, config: StorageConfig) -> Result<Self> {
        Self::open_with_maintenance(path, kind, config, crate::MaintenancePolicy::default())
    }
    pub fn open_with_maintenance(
        path: impl AsRef<Path>,
        kind: DatabaseKind,
        config: StorageConfig,
        policy: crate::MaintenancePolicy,
    ) -> Result<Self> {
        policy.validate()?;
        config
            .validate()
            .map_err(|e| Error::Configuration(e.to_string()))?;
        crate::initialize_sqlite()?;
        let path = path.as_ref();
        fs::create_dir_all(
            path.parent()
                .ok_or(Error::Invalid("database path needs a parent"))?,
        )?;
        let path = fs::canonicalize(path.parent().unwrap())?.join(
            path.file_name()
                .ok_or(Error::Invalid("database filename required"))?,
        );
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".owner");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(lock_path))?;
        lock.try_lock_exclusive()
            .map_err(|_| Error::AlreadyOpen(path.clone()))?;
        let blob_root = if kind == DatabaseKind::Main {
            path.parent().unwrap().join("blobs")
        } else {
            path.parent()
                .unwrap()
                .join("blob-stores")
                .join(path.file_name().unwrap())
        };
        let blobs = BlobStore::new(blob_root)?;
        let blob_lock = if kind == DatabaseKind::Main {
            let ownership = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(blobs.root().join(".owner"))?;
            ownership
                .try_lock_exclusive()
                .map_err(|_| Error::AlreadyOpen(blobs.root().into()))?;
            Some(ownership)
        } else {
            None
        };
        let (conn, recovery, issues) = lifecycle::open_recover(&path, kind, &config, &blobs)?;
        let recovered = recovery.is_some();
        let (events, _) = broadcast::channel(256);
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                lanes: std::array::from_fn(|_| VecDeque::new()),
                closed: false,
            }),
            wake: Condvar::new(),
            slots: std::array::from_fn(|_| Arc::new(Semaphore::new(config.queue_capacity))),
            events,
            stats: Mutex::new(StoreStats::default()),
            paused: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
        });
        let open_reader = || -> Result<Connection> {
            let c = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            lifecycle::configure(&c, kind, &config, false)?;
            Ok(c)
        };
        let interactive = Arc::new(Mutex::new(open_reader()?));
        let mut background = Vec::new();
        for _ in 1..config.read_connections {
            background.push(open_reader()?);
        }
        let readers = Readers {
            interactive,
            background: Arc::new(Mutex::new(background)),
            foreground_slots: Arc::new(Semaphore::new(1)),
            background_slots: Arc::new(Semaphore::new(config.read_connections - 1)),
            active: Arc::new((Mutex::new(0), Condvar::new())),
        };
        let mut startup_events = Vec::new();
        if recovered {
            startup_events.push(StoreEvent::Recovered);
            if kind == DatabaseKind::CodeGraph {
                startup_events.push(StoreEvent::RebuildRequired(kind));
            }
        }
        if let Some(report) = recovery {
            startup_events.push(StoreEvent::RecoveryInfo(report));
        }
        for issue in issues {
            startup_events.push(StoreEvent::BlobIntegrity(issue));
        }
        {
            let mut statement = conn.prepare("PRAGMA foreign_key_check")?;
            let mut rows = statement.query([])?;
            while startup_events.len() < 64 {
                let Some(row) = rows.next()? else {
                    break;
                };
                startup_events.push(StoreEvent::ForeignKeyIssue {
                    table: row.get(0)?,
                    rowid: row.get(1)?,
                    parent: row.get(2)?,
                });
            }
        }
        if config.network_filesystem {
            startup_events.push(StoreEvent::NetworkJournalDegraded);
        }
        let worker_config = config.clone();
        let s = shared.clone();
        let p = path.clone();
        let b = blobs.clone();
        let join = thread::Builder::new()
            .name("excalibur-store-writer".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    writer_loop(conn, &s, &p, kind, &worker_config, &b)
                }));
                if outcome.is_err() {
                    s.failed.store(true, Ordering::Release);
                    let _ = s.events.send(StoreEvent::WriterFailed);
                }
                let mut q = s.queue.lock().unwrap_or_else(|p| p.into_inner());
                q.closed = true;
                for lane in &mut q.lanes {
                    for job in lane.drain(..) {
                        let _ = job.response.send(Err(Error::Closed));
                    }
                }
                for slots in &s.slots {
                    slots.close();
                }
            })?;
        let db = Self {
            inner: Arc::new(Inner {
                shared,
                readers,
                join: Mutex::new(Some(join)),
                path,
                blobs,
                startup_events,
                kind,
                config,
                maintenance_lock: Arc::new(Mutex::new(())),
                maintenance_serial: tokio::sync::Mutex::new(()),
                scheduler_registration: Mutex::new(None),
                maintenance_allowed: AtomicBool::new(true),
                verifier: Arc::new(Mutex::new(crate::integrity::Verifier::default())),
                _lock: lock,
                _blob_lock: blob_lock,
            }),
        };
        // Startup facts are also exposed through a query so subscriptions cannot miss them.

        if recovered {
            let _ = db.inner.shared.events.send(StoreEvent::Recovered);
        }
        let weak = Arc::downgrade(&db.inner);
        let registration = crate::maintenance::register(
            policy,
            Arc::new(move |runtime, now| {
                use crate::maintenance::Outcome;
                let Some(inner) = weak.upgrade() else {
                    return Outcome::Gone;
                };
                if !inner.maintenance_allowed.load(Ordering::Acquire)
                    || inner.shared.paused.load(Ordering::Acquire)
                {
                    return Outcome::Deferred;
                }
                {
                    let queue = inner.shared.queue.lock().unwrap();
                    if queue.closed {
                        return Outcome::Gone;
                    }
                    if queue.lanes.iter().any(|l| !l.is_empty()) {
                        return Outcome::Deferred;
                    }
                }
                let db = Database { inner };
                let Ok((lifetime, activity)) = db.activity() else {
                    return Outcome::Gone;
                };
                let _lifetime = lifetime;
                let _activity = activity;
                match runtime.block_on(db.maintenance(now)) {
                    Ok(()) => Outcome::Complete,
                    Err(Error::Closed) => Outcome::Gone,
                    Err(e) => {
                        let _ = db
                            .inner
                            .shared
                            .events
                            .send(StoreEvent::MaintenanceFailed(e.to_string()));
                        Outcome::Deferred
                    }
                }
            }),
        );
        *db.inner.scheduler_registration.lock().unwrap() = registration;
        Ok(db)
    }
    pub fn set_maintenance_allowed(&self, allowed: bool) {
        self.inner
            .maintenance_allowed
            .store(allowed, Ordering::Release);
    }
    pub fn recovery_report(&self) -> Option<&crate::RecoveryReport> {
        self.inner.startup_events.iter().find_map(|e| {
            if let StoreEvent::RecoveryInfo(report) = e {
                Some(report)
            } else {
                None
            }
        })
    }
    pub fn startup_events(&self) -> &[StoreEvent] {
        &self.inner.startup_events
    }
    fn activity(&self) -> Result<(Arc<Inner>, ReaderActivity)> {
        let q = self.inner.shared.queue.lock().unwrap();
        if q.closed {
            return Err(Error::Closed);
        }
        *self.inner.readers.active.0.lock().unwrap() += 1;
        Ok((
            self.inner.clone(),
            ReaderActivity(self.inner.readers.active.clone()),
        ))
    }
    pub fn path(&self) -> &Path {
        &self.inner.path
    }
    pub fn blobs(&self) -> BlobStore {
        self.inner.blobs.clone()
    }
    pub fn subscribe_events(&self) -> broadcast::Receiver<StoreEvent> {
        self.inner.shared.events.subscribe()
    }
    pub fn stats(&self) -> StoreStats {
        self.inner.shared.stats.lock().unwrap().clone()
    }
    pub fn is_failed(&self) -> bool {
        self.inner.shared.failed.load(Ordering::Acquire)
    }
    async fn command<T: Send + 'static, F: FnOnce(&Connection) -> Result<T> + Send + 'static>(
        &self,
        priority: Priority,
        transaction: bool,
        work: F,
    ) -> Result<T> {
        if self.is_failed() {
            return Err(Error::WriterPanicked);
        }
        if priority == Priority::Background && self.inner.shared.paused.load(Ordering::Acquire) {
            return Err(Error::BackgroundPaused);
        }
        if priority == Priority::Background {
            tokio::task::yield_now().await;
        }
        // Separate bounded lanes keep background backpressure from occupying interactive slots.
        let permit = self.inner.shared.slots[priority as usize]
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Closed)?;
        let (tx, rx) = oneshot::channel();
        {
            let mut q = self.inner.shared.queue.lock().unwrap();
            if q.closed {
                return Err(Error::Closed);
            }
            q.lanes[priority as usize].push_back(Job {
                work: Box::new(move |conn| work(conn).map(|v| Box::new(v) as Output)),
                response: tx,
                priority,
                queued: Instant::now(),
                _permit: permit,
                transaction,
            });
        }
        self.inner.shared.wake.notify_one();
        let value = rx.await.map_err(|_| Error::Closed)??;
        value
            .downcast::<T>()
            .map(|v| *v)
            .map_err(|_| Error::Invalid("response type mismatch"))
    }
    pub async fn write<T: Send + 'static, F: FnOnce(&Connection) -> Result<T> + Send + 'static>(
        &self,
        priority: Priority,
        work: F,
    ) -> Result<T> {
        self.command(priority, true, work).await
    }
    pub async fn read<T: Send + 'static, F: FnOnce(&Connection) -> Result<T> + Send + 'static>(
        &self,
        lane: ReadLane,
        work: F,
    ) -> Result<T> {
        let readers = &self.inner.readers;
        let (slots, interactive, pool) = (
            match lane {
                ReadLane::Interactive => readers.foreground_slots.clone(),
                ReadLane::Background => readers.background_slots.clone(),
            },
            readers.interactive.clone(),
            readers.background.clone(),
        );
        let permit = slots.acquire_owned().await.map_err(|_| Error::Closed)?;
        {
            let q = self.inner.shared.queue.lock().unwrap();
            if q.closed {
                return Err(Error::Closed);
            }
            *readers.active.0.lock().unwrap() += 1;
        }
        let activity = ReaderActivity(readers.active.clone());
        let lifetime = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _lifetime = lifetime;
            let _activity = activity;
            let _permit = permit;
            match lane {
                ReadLane::Interactive => {
                    let conn = interactive.lock().unwrap_or_else(|p| p.into_inner());
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&conn)));
                    if !conn.is_autocommit() {
                        conn.execute_batch("ROLLBACK")?;
                    }
                    result.map_err(|_| Error::Invalid("reader panicked"))?
                }
                ReadLane::Background => {
                    let conn = pool.lock().unwrap().pop().ok_or(Error::Closed)?;
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&conn)));
                    if !conn.is_autocommit() {
                        let _ = conn.execute_batch("ROLLBACK");
                    }
                    pool.lock().unwrap().push(conn);
                    result.map_err(|_| Error::Invalid("reader panicked"))?
                }
            }
        })
        .await
        .map_err(|_| Error::Closed)?
    }
    pub async fn attach_blob(
        &self,
        prepared: PreparedBlob,
        owner_type: String,
        owner_id: String,
        slot: String,
    ) -> Result<BlobHash> {
        self.write_with_blob(prepared,Priority::Normal,move|conn,hash| {
            conn.execute("INSERT INTO blob_refs(owner_type,owner_id,slot,hash) VALUES(?1,?2,?3,?4) ON CONFLICT(owner_type,owner_id,slot) DO UPDATE SET hash=excluded.hash",rusqlite::params![owner_type,owner_id,slot,hash.as_str()])?;
            Ok(hash.clone())
        }).await
    }
    /// Publish a staged blob before atomically updating its owning records.
    pub async fn write_with_blob<
        T: Send + 'static,
        F: FnOnce(&Connection, &BlobHash) -> Result<T> + Send + 'static,
    >(
        &self,
        prepared: PreparedBlob,
        priority: Priority,
        work: F,
    ) -> Result<T> {
        if self.inner.kind != DatabaseKind::Main {
            return Err(Error::PrincipalOnly);
        }
        let blobs = self.blobs();
        self.write(priority,move|conn| {
            let (hash,size,compressed)=blobs.publish(prepared)?;
            conn.execute("INSERT INTO blobs(hash,size,compressed_size,content_generation) VALUES(?1,?2,?3,?4) ON CONFLICT(hash) DO UPDATE SET size=excluded.size,compressed_size=excluded.compressed_size,content_generation=excluded.content_generation",rusqlite::params![hash.as_str(),size,compressed,excalibur_types::JobId::new().to_string()])?;
            crate::integrity::record(conn,&crate::BlobIssue::new(hash.as_str().into(),crate::BlobState::Ok,"verified prepared content"))?;
            work(conn,&hash)
        }).await
    }
    pub async fn read_blob(&self, hash: BlobHash, max_bytes: u64) -> Result<Vec<u8>> {
        if self.inner.kind != DatabaseKind::Main {
            return Err(Error::PrincipalOnly);
        }
        use rusqlite::OptionalExtension;
        let lookup = hash.as_str().to_string();
        let generation: Option<String> = self
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT content_generation FROM blobs WHERE hash=?1",
                    [lookup],
                    |r| r.get(0),
                )
                .optional()?)
            })
            .await?;
        let blobs = self.blobs();
        let original = hash.clone();
        let (lifetime, activity) = self.activity()?;
        let result = tokio::task::spawn_blocking(move || {
            let _lifetime = lifetime;
            let _activity = activity;
            blobs.read(&hash, max_bytes)
        })
        .await
        .map_err(|_| Error::Closed)?;
        let issue = blob_access_issue(&original, &result);
        self.record_blob_access(generation, issue).await;
        result
    }
    /// Read a verified uncompressed byte window without allocating the full blob.
    /// The complete registered content is decoded and hashed off the writer on every
    /// request (O(total bytes)); returned buffers are bounded to at most 1 MiB.
    pub async fn read_blob_window(
        &self,
        hash: BlobHash,
        offset: u64,
        max_bytes: usize,
    ) -> Result<crate::BlobWindow> {
        self.read_blob_window_inspected(hash, offset, max_bytes, None)
            .await
    }
    /// Inspect the entire decoded content in the existing verification pass, before
    /// returning any window. Bounded memory; no second decompression/scan is added.
    pub async fn read_blob_window_inspected(
        &self,
        hash: BlobHash,
        offset: u64,
        max_bytes: usize,
        inspector: Option<Box<dyn excalibur_types::runtime::DurableByteInspector>>,
    ) -> Result<crate::BlobWindow> {
        if self.inner.kind != DatabaseKind::Main {
            return Err(Error::PrincipalOnly);
        }
        if !(1..=1024 * 1024).contains(&max_bytes) {
            return Err(Error::Invalid("blob window must be 1..=1048576 bytes"));
        }
        use rusqlite::OptionalExtension;
        let lookup = hash.as_str().to_string();
        let metadata: Option<(u64, String)> = self
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT size,content_generation FROM blobs WHERE hash=?1",
                    [lookup],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?)
            })
            .await?;
        let generation = metadata.as_ref().map(|(_, generation)| generation.clone());
        let original = hash.clone();
        let result = if let Some((size, _)) = metadata {
            let blobs = self.blobs();
            let (lifetime, activity) = self.activity()?;
            tokio::task::spawn_blocking(move || {
                let _lifetime = lifetime;
                let _activity = activity;
                blobs.read_window(&hash, size, offset, max_bytes, inspector)
            })
            .await
            .map_err(|_| Error::Closed)?
        } else {
            Err(Error::BlobMetadataMissing(hash.as_str().into()))
        };
        let issue = blob_access_issue(&original, &result);
        self.record_blob_access(generation, issue).await;
        result
    }
    async fn record_blob_access(
        &self,
        generation: Option<String>,
        issue: Option<crate::BlobIssue>,
    ) {
        if let Some(mut issue) = issue {
            issue.generation = Some(generation);
            let observed = issue.clone();
            let recorded = self
                .write(Priority::Journal, move |c| {
                    crate::integrity::record(c, &issue)
                })
                .await;
            if matches!(recorded, Ok(true)) && observed.state != crate::BlobState::Ok {
                let _ = self
                    .inner
                    .shared
                    .events
                    .send(StoreEvent::BlobIntegrity(observed));
            }
            if let Err(error) = recorded {
                let _ = self
                    .inner
                    .shared
                    .events
                    .send(StoreEvent::MaintenanceFailed(error.to_string()));
            }
        }
    }
    /// One bounded incremental integrity pass; no model or allocation proportional to blobs.
    pub async fn verify_blobs(
        &self,
        budget: crate::VerificationBudget,
    ) -> Result<crate::VerificationProgress> {
        if self.inner.kind != DatabaseKind::Main {
            return Err(Error::PrincipalOnly);
        }
        let path = self.inner.path.clone();
        let blobs = self.blobs();
        let verifier = self.inner.verifier.clone();
        let (lifetime, activity) = self.activity()?;
        let mut progress = tokio::task::spawn_blocking(move || {
            let _lifetime = lifetime;
            let _activity = activity;
            verifier.lock().unwrap().step(&path, &blobs, budget)
        })
        .await
        .map_err(|_| Error::Closed)??;
        let mut applied = Vec::new();
        for issue in progress.issues {
            let observation = issue.clone();
            if self
                .write(Priority::Background, move |c| {
                    crate::integrity::record(c, &issue)
                })
                .await?
            {
                if observation.state != crate::BlobState::Ok {
                    let _ = self
                        .inner
                        .shared
                        .events
                        .send(StoreEvent::BlobIntegrity(observation.clone()));
                }
                applied.push(observation);
            }
        }
        progress.issues = applied;
        Ok(progress)
    }
    pub async fn backup(&self, now: i64) -> Result<()> {
        let _serial = self.inner.maintenance_serial.lock().await;
        self.backup_unlocked(now).await
    }
    async fn backup_unlocked(&self, now: i64) -> Result<()> {
        if self.inner.shared.paused.load(Ordering::Acquire) {
            return Err(Error::BackgroundPaused);
        }
        let path = self.inner.path.clone();
        let config = self.inner.config.clone();
        let kind = self.inner.kind;
        let serial = self.inner.maintenance_lock.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancelled.clone());
        let shared = self.inner.shared.clone();
        let (lifetime, activity) = self.activity()?;
        let result = tokio::task::spawn_blocking(move || {
            let _lifetime = lifetime;
            let _activity = activity;
            if cancelled.load(Ordering::Acquire) || shared.stopping.load(Ordering::Acquire) {
                return Err(Error::Closed);
            }
            let _guard = serial.lock().unwrap();
            let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            lifecycle::configure(&conn, kind, &config, false)?;
            conn.pragma_update(None, "query_only", false)?;
            let progress_cancelled = cancelled.clone();
            let progress_shared = shared.clone();
            conn.progress_handler(1000, Some(move || {
                progress_cancelled.load(Ordering::Acquire)
                    || progress_shared.stopping.load(Ordering::Acquire)
            }));
            let result = lifecycle::backup(&conn, &path, now);
            if (cancelled.load(Ordering::Acquire) || shared.stopping.load(Ordering::Acquire))
                && matches!(&result, Err(Error::Sqlite(e)) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted))
            {
                return Err(Error::Closed);
            }
            result
        })
        .await
        .map_err(|_| Error::Closed)?;
        if matches!(result, Err(Error::Full)) {
            self.inner.shared.paused.store(true, Ordering::Release);
            let _ = self.inner.shared.events.send(StoreEvent::DiskFull);
        }
        result?;
        let _ = self.inner.shared.events.send(StoreEvent::Backup);
        Ok(())
    }
    /// Daily independent maintenance lane; scans/backups never occupy the writer.
    /// Each candidate is rechecked and collected as a short serialized command.
    pub async fn maintenance(&self, now: i64) -> Result<()> {
        let _serial = self.inner.maintenance_serial.lock().await;
        self.backup_unlocked(now).await?;
        if self.inner.kind == DatabaseKind::Main {
            self.verify_blobs(crate::VerificationBudget::default())
                .await?;
        }
        let path = self.inner.path.clone();
        let blobs = self.blobs();
        let kind = self.inner.kind;
        let (lifetime, activity) = self.activity()?;
        let candidates = tokio::task::spawn_blocking(move || {
            let _lifetime = lifetime;
            let _activity = activity;
            if kind == DatabaseKind::Main {
                garbage_candidates(&path, &blobs, now)
            } else {
                Ok(Vec::new())
            }
        })
        .await
        .map_err(|_| Error::Closed)??;
        let mut count = 0;
        for (file, hash) in candidates {
            let removed = self
                .command(Priority::Background, false, move |conn| {
                    let staging = hash.is_none();
                    if let Some(hash) = hash {
                        let has_ref: bool = conn.query_row(
                            "SELECT EXISTS(SELECT 1 FROM blob_refs WHERE hash=?1)",
                            [&hash],
                            |r| r.get(0),
                        )?;
                        if has_ref {
                            return Ok(false);
                        }
                        let young: bool = conn.query_row(
                            "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash=?1 AND created_at>=?2)",
                            rusqlite::params![hash, now - 86400],
                            |r| r.get(0),
                        )?;
                        if young {
                            return Ok(false);
                        }
                        conn.execute("DELETE FROM blobs WHERE hash=?1 AND ref_count=0", [hash])?;
                    }
                    let _lease = if staging {
                        let handle = match OpenOptions::new().read(true).write(true).open(&file) {
                            Ok(f) => f,
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                            Err(e) => return Err(e.into()),
                        };
                        if handle.try_lock_exclusive().is_err() {
                            return Ok(false);
                        }
                        Some(handle)
                    } else {
                        None
                    };
                    match fs::remove_file(file) {
                        Ok(()) => Ok(true),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                        Err(e) => Err(e.into()),
                    }
                })
                .await?;
            count += usize::from(removed);
        }
        let s = self.inner.shared.clone();
        let timeout = self.inner.config.busy_timeout_ms;
        self.command(Priority::Background, false, move |conn| {
            // Non-blocking TRUNCATE: if a snapshot is pinned, retry in a later maintenance window.
            conn.busy_timeout(Duration::ZERO)?;
            let result = conn
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .map_err(Error::from);
            let _ = s.events.send(StoreEvent::Checkpoint("TRUNCATE"));
            conn.busy_timeout(Duration::from_millis(timeout))?;
            result
        })
        .await?;
        let _ = self
            .inner
            .shared
            .events
            .send(StoreEvent::BlobGarbageCollected(count));
        Ok(())
    }
    pub(crate) fn report_storage_error(&self, error: &Error) {
        if matches!(error, Error::Full) {
            self.inner.shared.paused.store(true, Ordering::Release);
            let _ = self.inner.shared.events.send(StoreEvent::DiskFull);
        }
    }
    pub fn resume_background(&self) {
        self.inner.shared.paused.store(false, Ordering::Release);
        let _ = self.inner.shared.events.send(StoreEvent::BackgroundResumed);
    }
    /// Stop admissions, drain acknowledged queued work and join the dedicated writer.
    pub async fn shutdown(&self) -> Result<()> {
        self.inner.shared.stopping.store(true, Ordering::Release);
        crate::maintenance::unregister(self.inner.scheduler_registration.lock().unwrap().take());
        self.inner.shared.queue.lock().unwrap().closed = true;
        for slots in &self.inner.shared.slots {
            slots.close();
        }
        self.inner.shared.wake.notify_all();
        self.inner.readers.foreground_slots.close();
        self.inner.readers.background_slots.close();
        let active = self.inner.readers.active.clone();
        tokio::task::spawn_blocking(move || {
            let mut count = active.0.lock().unwrap();
            while *count > 0 {
                count = active.1.wait(count).unwrap();
            }
        })
        .await
        .map_err(|_| Error::Closed)?;
        let join = self.inner.join.lock().unwrap().take();
        if let Some(join) = join {
            tokio::task::spawn_blocking(move || join.join())
                .await
                .map_err(|_| Error::Closed)?
                .map_err(|_| Error::WriterPanicked)?;
        }
        if self.is_failed() {
            return Err(Error::WriterPanicked);
        }
        Ok(())
    }
    /// Allocate counters in their own committed command; deleting rows never reuses a code.
    pub async fn allocate_short_id(&self, project: String, kind: ShortCodeKind) -> Result<String> {
        let key = match kind {
            ShortCodeKind::Task => "T".into(),
            ShortCodeKind::Requirement => "R".into(),
            ShortCodeKind::Milestone => "M".into(),
            ShortCodeKind::Criterion(n) if n > 0 => format!("C{n}"),
            _ => return Err(Error::Invalid("invalid requirement number")),
        };
        self.write(Priority::Normal,move|conn|{
            let n:u64=conn.query_row("INSERT INTO short_codes(project_id,kind,next_value) VALUES(?1,?2,2) ON CONFLICT(project_id,kind) DO UPDATE SET next_value=next_value+1 RETURNING next_value-1",rusqlite::params![project,key],|r|r.get(0))?;
            Ok(if key.starts_with('C'){format!("{key}.{n}")}else{format!("{key}{n}")})
        }).await
    }
}
#[derive(Clone, Copy, Debug)]
pub enum ShortCodeKind {
    Task,
    Requirement,
    Criterion(u64),
    Milestone,
}
fn next_job(shared: &Shared, max_priority: usize) -> Option<Job> {
    let mut q = shared.queue.lock().unwrap();
    for lane in q.lanes.iter_mut().take(max_priority + 1) {
        if let Some(job) = lane.pop_front() {
            return Some(job);
        }
    }
    None
}
fn begin(conn: &Connection, s: &Shared, c: &StorageConfig) -> Result<()> {
    retry_sql(conn, s, c, "BEGIN IMMEDIATE")
}
fn retry_sql(conn: &Connection, s: &Shared, c: &StorageConfig, sql: &str) -> Result<()> {
    for attempt in 0..=c.busy_retries {
        match conn.execute_batch(sql) {
            Ok(()) => return Ok(()),
            Err(e)
                if matches!(
                    e.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ) && attempt < c.busy_retries =>
            {
                s.stats.lock().unwrap().busy_retries += 1;
                let _ = s.events.send(StoreEvent::BusyRetry(attempt + 1));
                // Bounded deterministic jitter avoids a global RNG; not a test timing dependency.
                thread::sleep(Duration::from_millis(
                    (10u64 << attempt.min(8)) + (attempt as u64 * 7 % 11),
                ));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Busy)
}
fn writer_loop(
    conn: Connection,
    s: &Shared,
    path: &Path,
    kind: DatabaseKind,
    c: &StorageConfig,
    blobs: &BlobStore,
) {
    if c.network_filesystem {
        let _ = s.events.send(StoreEvent::NetworkJournalDegraded);
    }
    if kind == DatabaseKind::Main
        && conn.execute_batch("CREATE TEMP TABLE touched_blobs(hash TEXT PRIMARY KEY); CREATE TEMP TRIGGER touched_insert AFTER INSERT ON main.blob_refs BEGIN INSERT OR IGNORE INTO touched_blobs VALUES(NEW.hash); END; CREATE TEMP TRIGGER touched_update AFTER UPDATE OF hash ON main.blob_refs BEGIN INSERT OR IGNORE INTO touched_blobs VALUES(NEW.hash); END;").is_err(){s.failed.store(true,Ordering::Release);let _=s.events.send(StoreEvent::WriterFailed);return;}
    loop {
        let first = next_job(s, 3);
        let Some(first) = first else {
            let q = s.queue.lock().unwrap();
            if q.closed {
                break;
            }
            if q.lanes.iter().any(|l| !l.is_empty()) {
                continue;
            }
            let (q, _) = s.wake.wait_timeout(q, Duration::from_secs(1)).unwrap();
            drop(q);
            // Never start background maintenance ahead of a newly queued interactive command.
            if s.queue.lock().unwrap().lanes.iter().any(|l| !l.is_empty()) {
                continue;
            }
            if !c.network_filesystem {
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
                let _ = s.events.send(StoreEvent::Checkpoint("PASSIVE"));
            }
            continue;
        };
        if first.priority == Priority::Background && s.paused.load(Ordering::Acquire) {
            let _ = first.response.send(Err(Error::BackgroundPaused));
            continue;
        }
        let p = first.priority as usize;
        if !first.transaction {
            let result = (first.work)(&conn);
            if matches!(result, Err(Error::Full)) {
                s.paused.store(true, Ordering::Release);
                let _ = s.events.send(StoreEvent::DiskFull);
            }
            record(s, &first.priority, first.queued, result.is_ok());
            let _ = first.response.send(result);
            continue;
        }
        let started = Instant::now();
        if kind == DatabaseKind::Main {
            let _ = conn.execute_batch("DELETE FROM touched_blobs");
        }
        if let Err(e) = begin(&conn, s, c) {
            if matches!(e, Error::Full) {
                s.paused.store(true, Ordering::Release);
                let _ = s.events.send(StoreEvent::DiskFull);
            }
            record(s, &first.priority, first.queued, false);
            let _ = first.response.send(Err(e));
            continue;
        }
        let mut completed = Vec::new();
        let mut job = Some(first);
        let mut fatal = false;
        let mut aborted: Option<Error> = None;
        while let Some(current) = job.take() {
            let priority = current.priority;
            {
                let mut stats = s.stats.lock().unwrap();
                stats.max_queue_wait_micros[priority as usize] = stats.max_queue_wait_micros
                    [priority as usize]
                    .max(current.queued.elapsed().as_micros() as u64);
            }
            let queued = current.queued;
            drop(current._permit);
            let mut result = match conn.execute_batch("SAVEPOINT command") {
                Err(error) => {
                    let cause = Error::TransactionControl {
                        phase: "savepoint",
                        original: Box::new(Error::Invalid("command not executed")),
                        control: Box::new(error.into()),
                    };
                    aborted = Some(cause);
                    Err(Error::Closed)
                }
                Ok(()) => {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (current.work)(&conn)))
                        .unwrap_or_else(|_| {
                            fatal = true;
                            Err(Error::WriterPanicked)
                        })
                }
            };
            if aborted.is_none() {
                if result.is_ok() {
                    if let Err(error) = conn.execute_batch("RELEASE command") {
                        aborted = Some(Error::TransactionControl {
                            phase: "release",
                            original: Box::new(Error::Invalid("command release failed")),
                            control: Box::new(error.into()),
                        });
                    }
                } else if let Err(control) =
                    conn.execute_batch("ROLLBACK TO command; RELEASE command")
                {
                    let original = result.err().unwrap();
                    aborted = Some(if matches!(original, Error::Full) {
                        Error::Full
                    } else {
                        Error::TransactionControl {
                            phase: "rollback to savepoint",
                            original: Box::new(original),
                            control: Box::new(control.into()),
                        }
                    });
                    result = Err(Error::Closed);
                }
            }
            if matches!(result, Err(Error::Full)) {
                s.paused.store(true, Ordering::Release);
                let _ = s.events.send(StoreEvent::DiskFull);
            }
            completed.push((priority, queued, current.response, result));
            if fatal
                || aborted.is_some()
                || completed.len() >= c.group_commit_max_operations
                || started.elapsed() >= Duration::from_millis(c.group_commit_ms)
            {
                break;
            }
            // Only same or higher priority may join this group; control operations stay outside.
            let mut q = s.queue.lock().unwrap();
            for lane in q.lanes.iter_mut().take(p + 1) {
                if lane.front().is_some_and(|j| j.transaction) {
                    job = lane.pop_front();
                    break;
                }
            }
        }
        let validation = if kind == DatabaseKind::Main && !fatal && aborted.is_none() {
            validate_presence(&conn, blobs)
        } else {
            Ok(())
        };
        let mut commit = if fatal {
            Err(Error::WriterPanicked)
        } else if let Some(error) = aborted {
            Err(error)
        } else if let Err(e) = validation {
            Err(e)
        } else {
            retry_sql(&conn, s, c, "COMMIT")
        };
        if matches!(commit, Err(Error::Full)) {
            s.paused.store(true, Ordering::Release);
            let _ = s.events.send(StoreEvent::DiskFull);
        }
        if commit.is_err()
            && !conn.is_autocommit()
            && let Err(control) = conn.execute_batch("ROLLBACK")
        {
            let original = commit.err().unwrap();
            commit = Err(Error::TransactionControl {
                phase: "group rollback",
                original: Box::new(original),
                control: Box::new(control.into()),
            });
            fatal = true;
        }
        let commit = commit.map_err(Arc::new);
        {
            let mut stats = s.stats.lock().unwrap();
            stats.groups += 1;
            stats.max_group_operations = stats.max_group_operations.max(completed.len());
        }
        for (priority, queued, response, result) in completed {
            let final_result = if commit.is_ok() {
                result
            } else {
                let cause = commit.as_ref().err().unwrap();
                Err(match cause.as_ref() {
                    Error::Full => Error::Full,
                    Error::Blob(hash) => Error::Blob(hash.clone()),
                    Error::Busy => Error::Busy,
                    Error::WriterPanicked => Error::WriterPanicked,
                    _ => Error::GroupAborted(cause.clone()),
                })
            };
            record(s, &priority, queued, final_result.is_ok());
            let _ = response.send(final_result);
        }
        if fatal {
            s.failed.store(true, Ordering::Release);
            let _ = s.events.send(StoreEvent::WriterFailed);
            break;
        }
        if kind == DatabaseKind::Main || !c.network_filesystem {
            check_wal(s, path, c.wal_alert_bytes);
        }
    }
}
fn record(s: &Shared, p: &Priority, queued: Instant, success: bool) {
    let mut stats = s.stats.lock().unwrap();
    let index = *p as usize;
    if success {
        stats.committed[index] += 1;
    } else {
        stats.failed[index] += 1;
    }
    stats.max_completion_micros[index] =
        stats.max_completion_micros[index].max(queued.elapsed().as_micros() as u64);
}
fn check_wal(s: &Shared, path: &Path, limit: u64) {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    if let Ok(meta) = fs::metadata(PathBuf::from(wal))
        && meta.len() > limit
    {
        let _ = s.events.send(StoreEvent::WalLarge(meta.len()));
    }
}
fn garbage_candidates(
    path: &Path,
    blobs: &BlobStore,
    now: i64,
) -> Result<Vec<(PathBuf, Option<String>)>> {
    let mut protected = std::collections::BTreeSet::new();
    if let Ok(entries) = fs::read_dir(lifecycle::backup_dir(path)) {
        for entry in entries {
            let entry = entry?;
            if entry.path().extension().is_none_or(|s| s != "db") {
                continue;
            }
            let backup =
                Connection::open_with_flags(entry.path(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let exists: bool = backup.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='blob_refs')",
                [],
                |r| r.get(0),
            )?;
            if !exists {
                continue;
            }
            let mut stmt = backup.prepare("SELECT DISTINCT hash FROM blob_refs")?;
            for hash in stmt.query_map([], |r| r.get::<_, String>(0))? {
                protected.insert(hash?);
            }
        }
    }
    let mut candidates = Vec::new();
    for dir in fs::read_dir(blobs.root().join("sha256"))? {
        let dir = dir?;
        if !dir.file_type()?.is_dir() {
            continue;
        }
        for file in fs::read_dir(dir.path())? {
            let file = file?;
            let hash = format!(
                "{}{}",
                dir.file_name().to_string_lossy(),
                file.file_name().to_string_lossy()
            );
            let age = file
                .metadata()?
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if age < now - 86400 && !protected.contains(&hash) {
                candidates.push((file.path(), Some(hash)));
            }
        }
    }
    for file in fs::read_dir(blobs.root().join("staging"))? {
        let file = file?;
        let modified = file
            .metadata()?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if modified < now - 86400 {
            candidates.push((file.path(), None));
        }
    }
    Ok(candidates)
}
fn validate_presence(conn: &Connection, blobs: &BlobStore) -> Result<()> {
    let mut stmt = conn.prepare("SELECT hash FROM touched_blobs")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    for row in rows {
        let hash = row?;
        let parsed = hash.parse::<BlobHash>()?;
        if !blobs.path(&parsed).is_file() {
            return Err(Error::Blob(hash));
        }
    }
    Ok(())
}

fn blob_access_issue<T>(hash: &BlobHash, result: &Result<T>) -> Option<crate::BlobIssue> {
    match result {
        Ok(_) => Some(crate::BlobIssue::new(
            hash.as_str().into(),
            crate::BlobState::Ok,
            "verified on access",
        )),
        Err(Error::BlobTooLarge(_) | Error::Invalid(_) | Error::BlobAccessDenied) => None,
        Err(Error::BlobMetadataMissing(_)) => Some(crate::BlobIssue::new(
            hash.as_str().into(),
            crate::BlobState::MetadataMissing,
            "blob access has no registered metadata",
        )),
        Err(error) => Some(crate::integrity::issue_from_error(
            hash.as_str().into(),
            error,
        )),
    }
}

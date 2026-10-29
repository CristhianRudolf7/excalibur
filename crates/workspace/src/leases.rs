use crate::*;
use excalibur_store::rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions, TryLockError},
    io::{Read, Seek, SeekFrom, Write},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
fn active_state(state: &str, reason: Option<&str>) -> bool {
    matches!(
        state,
        "running" | "verifying" | "recovering" | "waiting_tool" | "waiting_children"
    ) || state == "waiting" && matches!(reason, Some("tool" | "subagents" | "children"))
}
fn durable_state(
    c: &excalibur_store::rusqlite::Connection,
    run: &str,
) -> excalibur_store::Result<Option<(String, Option<String>)>> {
    let row: Option<(String, Option<String>)> = c
        .query_row("SELECT state,reason FROM runs WHERE id=?1", [run], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let Some(fallback) = row else { return Ok(None) };
    let event:Option<String>=c.query_row("SELECT payload FROM journal WHERE run_id=?1 AND kind='state_change' AND state='confirmed' ORDER BY sequence DESC LIMIT 1",[run],|r|r.get(0)).optional()?;
    if let Some(event) = event {
        let event: serde_json::Value = serde_json::from_str(&event)
            .map_err(|_| excalibur_store::Error::Invalid("invalid durable run control state"))?;
        let state = event["state"]
            .as_str()
            .ok_or(excalibur_store::Error::Invalid(
                "missing durable control state",
            ))?;
        return Ok(Some((
            state.to_owned(),
            event["reason"].as_str().map(str::to_owned),
        )));
    }
    Ok(Some(fallback))
}
fn acquisition_allowed(
    c: &excalibur_store::rusqlite::Connection,
    run: &str,
) -> excalibur_store::Result<bool> {
    let Some((state, reason)) = durable_state(c, run)? else {
        return Ok(true);
    };
    if active_state(&state, reason.as_deref()) {
        return Ok(true);
    }
    if matches!(state.as_str(), "created" | "ready" | "pending") {
        return Ok(true);
    }
    Ok(false)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeaseOwner {
    pub path: PathBuf,
    pub holder: String,
    pub activity: String,
    pub expires_at: i64,
}
struct Entry {
    owner: LeaseOwner,
    expires: AtomicI64,
    file: Mutex<File>,
    activity: Mutex<String>,
}
impl Entry {
    fn current_owner(&self) -> LeaseOwner {
        LeaseOwner {
            activity: self.activity.lock().unwrap().clone(),
            expires_at: self.expires.load(Ordering::Acquire),
            ..self.owner.clone()
        }
    }
}
impl Drop for Entry {
    fn drop(&mut self) {
        if let Ok(file) = self.file.get_mut() {
            let _ = file.unlock();
        }
    }
}
#[derive(Default)]
struct Shared {
    entries: Mutex<BTreeMap<PathBuf, Arc<Entry>>>,
    active: Mutex<BTreeSet<String>>,
    refresher: Mutex<Refresher>,
    admission: tokio::sync::Mutex<()>,
    notify: tokio::sync::Notify,
}
#[derive(Default)]
struct Refresher {
    running: bool,
    context: Weak<TickContext>,
}
struct TickContext {
    db: Database,
    root: PathBuf,
    directory: PathBuf,
    redactor: Redactor,
}
#[derive(Clone)]
pub struct LeaseManager {
    db: Database,
    root: PathBuf,
    directory: PathBuf,
    redactor: Redactor,
    shared: Arc<Shared>,
    tick: Arc<TickContext>,
}
/// Dropping the effect guard does not release the durable lease; run-state hooks
/// do that. It keeps OS exclusion alive if release races a noninterruptible effect.
pub struct LeaseGuard {
    _entries: Vec<Arc<Entry>>,
}
impl LeaseManager {
    fn start_refresher(&self) {
        {
            let mut state = self.shared.refresher.lock().unwrap();
            state.context = Arc::downgrade(&self.tick);
            if state.running {
                return;
            }
            state.running = true;
        }
        let weak = Arc::downgrade(&self.shared);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let Some(shared) = weak.upgrade() else { break };
                let owners = shared
                    .active
                    .lock()
                    .unwrap()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>();
                let context = {
                    let mut state = shared.refresher.lock().unwrap();
                    match state.context.upgrade() {
                        Some(context) => context,
                        None => {
                            state.running = false;
                            break;
                        }
                    }
                };
                let manager = LeaseManager {
                    db: context.db.clone(),
                    root: context.root.clone(),
                    directory: context.directory.clone(),
                    redactor: context.redactor.clone(),
                    shared,
                    tick: context.clone(),
                };
                for owner in owners {
                    if manager.heartbeat(&owner, "running", None).await.is_err() {
                        manager.shared.refresher.lock().unwrap().running = false;
                        return;
                    }
                }
            }
        });
    }

    pub fn open(
        db: Database,
        root: PathBuf,
        directory: PathBuf,
        redactor: Redactor,
    ) -> Result<Self> {
        std::fs::create_dir_all(&directory)?;
        let directory = directory.canonicalize()?;
        let root = root.canonicalize()?;
        type Maps = Mutex<BTreeMap<(PathBuf, PathBuf, PathBuf), Weak<Shared>>>;
        let map_key = (db.path().canonicalize()?, root.clone(), directory.clone());
        static MAPS: OnceLock<Maps> = OnceLock::new();
        let shared = {
            let mut maps = MAPS.get_or_init(Default::default).lock().unwrap();
            maps.retain(|_, v| v.strong_count() > 0);
            if let Some(v) = maps.get(&map_key).and_then(Weak::upgrade) {
                v
            } else {
                let v = Arc::new(Shared::default());
                maps.insert(map_key, Arc::downgrade(&v));
                v
            }
        };
        let tick = Arc::new(TickContext {
            db: db.clone(),
            root: root.clone(),
            directory: directory.clone(),
            redactor: redactor.clone(),
        });
        Ok(Self {
            db,
            root,
            directory,
            redactor,
            shared,
            tick,
        })
    }
    pub async fn acquire_many(
        &self,
        run: &str,
        paths: &[PathBuf],
        activity: &str,
    ) -> Result<LeaseGuard> {
        self.acquire_many_with_wait(run, paths, activity, Duration::from_secs(30))
            .await
    }
    /// A caller that already waited on another resource must pass its remaining
    /// admission budget, rather than starting another 30-second wait.
    pub async fn acquire_many_with_wait(
        &self,
        run: &str,
        paths: &[PathBuf],
        activity: &str,
        wait: Duration,
    ) -> Result<LeaseGuard> {
        self.heartbeat(run, "running", None).await?;
        self.acquire_many_at(run, paths, activity, excalibur_store::unix_seconds(), wait)
            .await
    }
    /// Explicit synthetic time/wait budget for deterministic backend tests and hosts.
    pub async fn acquire_many_at(
        &self,
        run: &str,
        paths: &[PathBuf],
        activity: &str,
        now: i64,
        wait: Duration,
    ) -> Result<LeaseGuard> {
        if run.is_empty() || activity.len() > 1024 || wait > Duration::from_secs(30) {
            return Err(Error::Invalid("invalid lease request"));
        }
        let mut paths = paths
            .iter()
            .map(|p| crate::typed_entries::entry_path(&self.root, p))
            .collect::<Result<Vec<_>>>()?;
        paths.sort();
        paths.dedup();
        let activity = self.redactor.redact_text(activity);
        let start = tokio::time::Instant::now();
        loop {
            let attempt = self
                .attempt(
                    run,
                    &paths,
                    &activity,
                    now + start.elapsed().as_secs() as i64,
                )
                .await;
            match attempt {
                Err(Error::Busy(owner)) => {
                    let remaining = wait.saturating_sub(start.elapsed());
                    if remaining.is_zero() {
                        return Err(Error::Busy(owner));
                    }
                    tokio::select! {_=tokio::time::sleep(remaining.min(Duration::from_millis(100)))=>{},_=self.shared.notify.notified()=>{}}
                }
                other => return other,
            }
        }
    }
    async fn attempt(
        &self,
        run: &str,
        paths: &[PathBuf],
        activity: &str,
        now: i64,
    ) -> Result<LeaseGuard> {
        let _serial = self.shared.admission.lock().await;
        let expires = now
            .checked_add(600)
            .ok_or(Error::Invalid("lease time overflow"))?;
        {
            let mut entries = self.shared.entries.lock().unwrap();
            entries
                .retain(|_, e| e.expires.load(Ordering::Acquire) > now || Arc::strong_count(e) > 1);
        }
        let wanted = paths.to_vec();
        let owner = run.to_owned();
        let busy=self.db.read(ReadLane::Interactive,move|c|{for p in wanted{let row:Option<LeaseOwner>=c.query_row("SELECT l.holder_id,l.expires_at,COALESCE(a.activity,'cooperating writer') FROM leases l LEFT JOIN workspace_lease_activity a ON a.path=l.path WHERE l.path=?1 AND l.holder_id<>?2 AND l.expires_at>?3",params![p.to_string_lossy(),owner,now],|r|Ok(LeaseOwner{path:p.clone(),holder:r.get(0)?,expires_at:r.get(1)?,activity:r.get(2)?})).optional()?;if row.is_some(){return Ok(row);}}Ok(None)}).await?;
        if let Some(owner) = busy {
            return Err(Error::Busy(owner));
        }
        let mut acquired = Vec::new();
        for path in paths {
            if let Some(entry) = self.shared.entries.lock().unwrap().get(path).cloned() {
                if entry.owner.holder != run {
                    return Err(Error::Busy(entry.current_owner()));
                }
                acquired.push(entry);
                continue;
            }
            let name = self.directory.join(hash(path.to_string_lossy().as_bytes()));
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(name)?;
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => {
                    let mut bytes = String::new();
                    let _ = file.read_to_string(&mut bytes);
                    let owner = serde_json::from_str(&bytes).unwrap_or(LeaseOwner {
                        path: path.clone(),
                        holder: "cooperating process".into(),
                        activity: "OS file lock retained".into(),
                        expires_at: expires,
                    });
                    return Err(Error::Busy(owner));
                }
                Err(TryLockError::Error(e)) => return Err(e.into()),
            }
            let owner = LeaseOwner {
                path: path.clone(),
                holder: run.into(),
                activity: activity.into(),
                expires_at: expires,
            };
            file.set_len(0)?;
            file.write_all(&serde_json::to_vec(&owner)?)?;
            file.sync_all()?;
            acquired.push(Arc::new(Entry {
                owner,
                expires: AtomicI64::new(expires),
                file: Mutex::new(file),
                activity: Mutex::new(activity.into()),
            }));
        }
        let (run, paths, activity, root) = (
            run.to_owned(),
            paths.to_vec(),
            activity.to_owned(),
            self.root.to_string_lossy().into_owned(),
        );
        let current_activity = activity.clone();
        self.db.write(Priority::Journal,move|c|{for path in paths{let path=path.to_str().ok_or(excalibur_store::Error::Invalid("lease paths must be UTF-8"))?;if !acquisition_allowed(c,&run)?{return Err(excalibur_store::Error::Invalid("inactive runs cannot acquire file leases"));}let changed=c.execute("INSERT INTO leases(path,holder_id,expires_at) VALUES(?1,?2,?3) ON CONFLICT(path) DO UPDATE SET holder_id=excluded.holder_id,expires_at=excluded.expires_at,updated_at=unixepoch() WHERE leases.holder_id=excluded.holder_id OR leases.expires_at<=?4",params![path,run,expires,now])?;if changed==0{return Err(excalibur_store::Error::Invalid("lease owner changed during admission"));}c.execute("INSERT INTO workspace_lease_activity VALUES(?1,?2,?3) ON CONFLICT(path) DO UPDATE SET activity=excluded.activity,root=excluded.root",params![path,activity,root])?;}Ok(())}).await?;
        let mut entries = self.shared.entries.lock().unwrap();
        for entry in &acquired {
            entry.expires.store(expires, Ordering::Release);
            *entry.activity.lock().unwrap() = current_activity.clone();
            {
                let mut file = entry.file.lock().unwrap();
                file.seek(SeekFrom::Start(0))?;
                file.set_len(0)?;
                file.write_all(&serde_json::to_vec(&entry.current_owner())?)?;
                file.sync_data()?;
            }
            entries.insert(entry.owner.path.clone(), entry.clone());
        }
        Ok(LeaseGuard { _entries: acquired })
    }
    pub async fn heartbeat(&self, run: &str, state: &str, reason: Option<&str>) -> Result<()> {
        let owner = run.to_owned();
        let actual = self
            .db
            .read(ReadLane::Interactive, move |c| durable_state(c, &owner))
            .await?;
        let (state, reason) = actual.unwrap_or((state.to_owned(), reason.map(str::to_owned)));
        if active_state(&state, reason.as_deref()) {
            self.shared.active.lock().unwrap().insert(run.to_owned());
            self.start_refresher();
        }
        self.renew_at(
            run,
            &state,
            reason.as_deref(),
            excalibur_store::unix_seconds(),
            true,
        )
        .await
    }
    pub async fn heartbeat_at(
        &self,
        run: &str,
        state: &str,
        reason: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.renew_at(run, state, reason, now, false).await
    }
    async fn renew_at(
        &self,
        run: &str,
        state: &str,
        reason: Option<&str>,
        now: i64,
        authoritative: bool,
    ) -> Result<()> {
        let active = active_state(state, reason);
        if !active {
            return self.release_run(run).await;
        }
        let _serial = self.shared.admission.lock().await;
        let expires = now
            .checked_add(600)
            .ok_or(Error::Invalid("lease time overflow"))?;
        let (run, root) = (run.to_owned(), self.root.to_string_lossy().into_owned());
        let owner = run.clone();
        let renewed=self.db.write(Priority::Journal,move|c|{if authoritative&&durable_state(c,&run)?.is_some_and(|(state,reason)|!active_state(&state,reason.as_deref())){c.execute("DELETE FROM leases WHERE holder_id=?1 AND path IN(SELECT path FROM workspace_lease_activity WHERE root=?2)",params![run,root])?;return Ok(false);}c.execute("UPDATE leases SET expires_at=?2,updated_at=unixepoch() WHERE holder_id=?1 AND path IN(SELECT path FROM workspace_lease_activity WHERE root=?3)",params![run,expires,root])?;Ok(true)}).await?;
        if !renewed {
            self.shared.active.lock().unwrap().remove(&owner);
            self.shared
                .entries
                .lock()
                .unwrap()
                .retain(|path, entry| entry.owner.holder != owner || !path.starts_with(&self.root));
            self.shared.notify.notify_waiters();
            return Ok(());
        }

        for entry in self
            .shared
            .entries
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.owner.holder == owner)
        {
            entry.expires.store(expires, Ordering::Release);
            let mut file = entry.file.lock().unwrap();
            file.seek(SeekFrom::Start(0))?;
            file.set_len(0)?;
            file.write_all(&serde_json::to_vec(&entry.current_owner())?)?;
            file.sync_data()?;
        }
        Ok(())
    }
    pub async fn release_run(&self, run: &str) -> Result<()> {
        self.shared.active.lock().unwrap().remove(run);
        let _serial = self.shared.admission.lock().await;
        let (owner, root) = (run.to_owned(), self.root.to_string_lossy().into_owned());
        self.db.write(Priority::Journal,move|c|{c.execute("DELETE FROM leases WHERE holder_id=?1 AND path IN(SELECT path FROM workspace_lease_activity WHERE root=?2)",params![owner,root])?;Ok(())}).await?;
        self.shared
            .entries
            .lock()
            .unwrap()
            .retain(|path, e| e.owner.holder != run || !path.starts_with(&self.root));
        self.shared.notify.notify_waiters();
        Ok(())
    }
    pub async fn owners(&self) -> Result<Vec<LeaseOwner>> {
        let root = self.root.to_string_lossy().into_owned();
        Ok(self.db.read(ReadLane::Interactive,move|c|{let mut q=c.prepare("SELECT l.path,l.holder_id,l.expires_at,a.activity FROM leases l JOIN workspace_lease_activity a ON a.path=l.path WHERE a.root=?1 ORDER BY l.path")?;Ok(q.query_map([root],|r|Ok(LeaseOwner{path:PathBuf::from(r.get::<_,String>(0)?),holder:r.get(1)?,expires_at:r.get(2)?,activity:r.get(3)?}))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?)
    }
}

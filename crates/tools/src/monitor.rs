//! Explicit project lifecycle adapter: filesystem events feed a durable change queue.
use crate::LocalTools;
use excalibur_store::{
    Priority, ReadLane,
    rusqlite::{OptionalExtension, params},
};
use excalibur_tasks::TaskStore;
use excalibur_types::runtime::BoxFuture;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, mpsc};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectChange {
    pub id: i64,
    pub path: PathBuf,
    pub previous_hash: Option<String>,
    pub current_hash: Option<String>,
    pub external: bool,
    #[serde(default = "semantic_change_default")]
    pub semantic_changed: bool,
}
fn semantic_change_default() -> bool {
    true
}
fn semantic_hash(path: &Path, bytes: &[u8]) -> String {
    if path == Path::new("tests/README.md")
        && let Ok(bytes) = excalibur_tasks::checklist::canonical_content(bytes)
    {
        return crate::files::hash(&bytes);
    }
    crate::files::hash(bytes)
}
pub trait ChangeObserver: Send + Sync {
    /// Content has passed the same root/protected-path checks as the file tools.
    fn changed<'a>(
        &'a self,
        change: &'a ProjectChange,
        content: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Result<(), String>>;
}
#[derive(Clone)]
pub struct EvidenceTarget {
    pub tasks: TaskStore,
    pub scope: String,
}
#[derive(Clone)]
pub struct ProjectMonitor {
    inner: Arc<MonitorInner>,
}
struct MonitorInner {
    tools: LocalTools,
    observer: Option<Arc<dyn ChangeObserver>>,
    evidence: Vec<EvidenceTarget>,
    serial: Mutex<()>,
    failure: std::sync::Mutex<Option<String>>,
    worker: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    watcher: std::sync::Mutex<Option<RecommendedWatcher>>,
}
impl Drop for MonitorInner {
    fn drop(&mut self) {
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
        {
            worker.abort();
        }
        if let Ok(watcher) = self.watcher.get_mut() {
            watcher.take();
        }
    }
}
impl ProjectMonitor {
    /// Host owns this handle while a project is active; dropping it stops observation.
    pub async fn start(
        tools: LocalTools,
        observer: Option<Arc<dyn ChangeObserver>>,
        evidence: Vec<EvidenceTarget>,
    ) -> Result<Self, String> {
        tools.db.write(Priority::Normal,|c| {c.execute_batch("CREATE TABLE IF NOT EXISTS project_observations(project TEXT NOT NULL,path TEXT NOT NULL,hash TEXT NOT NULL,PRIMARY KEY(project,path));CREATE TABLE IF NOT EXISTS project_changes(id INTEGER PRIMARY KEY AUTOINCREMENT,project TEXT NOT NULL,path TEXT NOT NULL,previous_hash TEXT,current_hash TEXT,external INTEGER NOT NULL,state TEXT NOT NULL DEFAULT 'pending');CREATE INDEX IF NOT EXISTS project_changes_pending ON project_changes(project,state,id);CREATE TABLE IF NOT EXISTS project_semantics(project TEXT NOT NULL,path TEXT NOT NULL,hash TEXT NOT NULL,PRIMARY KEY(project,path));CREATE TABLE IF NOT EXISTS project_change_semantics(change_id INTEGER PRIMARY KEY REFERENCES project_changes(id),changed INTEGER NOT NULL);")?;Ok(())}).await.map_err(|e|e.to_string())?;
        let monitor = Self {
            inner: Arc::new(MonitorInner {
                tools,
                observer,
                evidence,
                serial: Mutex::new(()),
                failure: std::sync::Mutex::new(None),
                worker: std::sync::Mutex::new(None),
                watcher: std::sync::Mutex::new(None),
            }),
        };
        let (tx, mut rx) = mpsc::channel::<Vec<PathBuf>>(64);
        let overflow = Arc::new(AtomicBool::new(false));
        let callback_overflow = overflow.clone();
        let ignored = monitor.inner.tools.policy.protected_paths.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let mut paths = match event {
                    Ok(event) if !matches!(event.kind, notify::EventKind::Access(_)) => {
                        if matches!(
                            event.kind,
                            notify::EventKind::Remove(notify::event::RemoveKind::Folder)
                        ) {
                            callback_overflow.store(true, Ordering::Release);
                        }
                        event.paths
                    }
                    Ok(_) => return,
                    Err(_) => {
                        callback_overflow.store(true, Ordering::Release);
                        Vec::new()
                    }
                };
                paths.retain(|path| {
                    !ignored.iter().any(|ignored| path.starts_with(ignored))
                        && !path.components().any(|p| {
                            matches!(
                                p.as_os_str().to_str(),
                                Some(".git" | "target" | "node_modules")
                            )
                        })
                });
                if paths.is_empty() && !callback_overflow.load(Ordering::Acquire) {
                    return;
                }
                if tx.try_send(paths).is_err() {
                    callback_overflow.store(true, Ordering::Release);
                }
            })
            .map_err(|e| e.to_string())?;
        watcher
            .watch(&monitor.inner.tools.root, RecursiveMode::Recursive)
            .map_err(|e| e.to_string())?;
        *monitor
            .inner
            .watcher
            .lock()
            .map_err(|_| "watcher lock poisoned")? = Some(watcher);
        // Capture notifications before baseline work can yield to an observer.
        monitor.scan(true).await?;
        let weak = Arc::downgrade(&monitor.inner);
        let worker = tokio::spawn(async move {
            loop {
                let signal = tokio::select! { signal=rx.recv()=>signal.map(Some), _=tokio::time::sleep(std::time::Duration::from_secs(30))=>Some(None) };
                let Some(signal) = signal else { break };
                let Some(inner) = weak.upgrade() else { break };
                let monitor = Self { inner };
                let result = if let Some(paths) = signal {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let mut paths = paths.into_iter().collect::<BTreeSet<_>>();
                    while let Ok(more) = rx.try_recv() {
                        paths.extend(more);
                    }
                    let full = overflow.swap(false, Ordering::AcqRel)
                        || paths.iter().any(|p| {
                            p.is_dir()
                                || matches!(
                                    p.file_name().and_then(|s| s.to_str()),
                                    Some(".gitignore" | "exclude")
                                )
                        });
                    monitor
                        .scan_selected(false, if full { None } else { Some(&paths) })
                        .await
                } else {
                    let _serial = monitor.inner.serial.lock().await;
                    monitor.drain().await
                };
                if let Ok(mut error) = monitor.inner.failure.lock() {
                    *error = result
                        .err()
                        .map(|e| monitor.inner.tools.policy.redactor.redact_text(&e));
                }
            }
        });
        *monitor
            .inner
            .worker
            .lock()
            .map_err(|_| "watcher lock poisoned")? = Some(worker);
        Ok(monitor)
    }
    pub async fn stop(&self) {
        if let Ok(mut watcher) = self.inner.watcher.lock() {
            watcher.take();
        }
        let worker = self
            .inner
            .worker
            .lock()
            .ok()
            .and_then(|mut worker| worker.take());
        if let Some(worker) = worker {
            worker.abort();
            let _ = worker.await;
        }
    }
    pub async fn scan_now(&self) -> Result<Vec<ProjectChange>, String> {
        self.scan(false).await
    }
    pub fn last_error(&self) -> Option<String> {
        self.inner.failure.lock().ok().and_then(|e| e.clone())
    }
    async fn scan(&self, baseline: bool) -> Result<Vec<ProjectChange>, String> {
        self.scan_selected(baseline, None).await
    }
    async fn scan_selected(
        &self,
        baseline: bool,
        paths: Option<&BTreeSet<PathBuf>>,
    ) -> Result<Vec<ProjectChange>, String> {
        let _serial = self.inner.serial.lock().await;
        let tools = &self.inner.tools;
        let project = tools.root.to_string_lossy().into_owned();
        let mut current = if paths.is_some() {
            tools
                .db
                .read(ReadLane::Interactive, {
                    let project = project.clone();
                    move |c| {
                        let mut q = c.prepare(
                            "SELECT path,hash FROM project_observations WHERE project=?1",
                        )?;
                        Ok(q.query_map([project], |r| {
                            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                        })?
                        .collect::<Result<BTreeMap<_, _>, _>>()?)
                    }
                })
                .await
                .map_err(|e| e.to_string())?
        } else {
            BTreeMap::new()
        };
        if let Some(paths) = paths {
            for path in paths {
                if let Ok(relative) = path.strip_prefix(&tools.root) {
                    current.remove(&relative.to_string_lossy().into_owned());
                }
            }
        }
        let mut semantics = BTreeMap::new();
        for (path, bytes) in tools.authorized_project_files_selected(paths)? {
            semantics.insert(
                path.to_string_lossy().into_owned(),
                semantic_hash(&path, &bytes),
            );
            current.insert(
                path.to_string_lossy().into_owned(),
                crate::files::hash(&bytes),
            );
        }
        let project = tools.root.to_string_lossy().into_owned();
        tools.db.write(Priority::Normal,move|c| {
            let old={let mut stmt=c.prepare("SELECT path,hash FROM project_observations WHERE project=?1")?;stmt.query_map([&project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<Result<BTreeMap<_,_>,_>>()?};
            let old_semantics={let mut q=c.prepare("SELECT path,hash FROM project_semantics WHERE project=?1")?;q.query_map([&project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<Result<BTreeMap<_,_>,_>>()?};
            let initial=baseline&&old.is_empty();
            for path in old.keys().chain(current.keys()).collect::<std::collections::BTreeSet<_>>() {
                if old.get(path)==current.get(path) {if let Some(hash)=semantics.get(path){c.execute("INSERT INTO project_semantics VALUES(?1,?2,?3) ON CONFLICT(project,path) DO UPDATE SET hash=excluded.hash",params![project,path,hash])?;}continue}
                {
                    let absolute=Path::new(&project).join(path).to_string_lossy().into_owned();
                    let latest:Option<Option<String>>=c.query_row("SELECT postimage_blob FROM file_versions WHERE path=?1 ORDER BY rowid DESC LIMIT 1",[absolute],|r|r.get(0)).optional()?;
                    let external=!initial && latest.as_ref().is_none_or(|hash|hash.as_ref()!=current.get(path));
                    c.execute("INSERT INTO project_changes(project,path,previous_hash,current_hash,external) VALUES(?1,?2,?3,?4,?5)",params![project,path,old.get(path),current.get(path),external])?;
                    let projected=path.as_str()=="tests/README.md";let empty=crate::files::hash(b"");
                    let before=old_semantics.get(path).or_else(||old.get(path)).or(if projected{Some(&empty)}else{None});
                    let after=semantics.get(path).or(if projected{Some(&empty)}else{None});
                    c.execute("INSERT INTO project_change_semantics VALUES(?1,?2)",params![c.last_insert_rowid(),before!=after])?;
                }
                if let Some(hash)=semantics.get(path){c.execute("INSERT INTO project_semantics VALUES(?1,?2,?3) ON CONFLICT(project,path) DO UPDATE SET hash=excluded.hash",params![project,path,hash])?;}else{c.execute("DELETE FROM project_semantics WHERE project=?1 AND path=?2",params![project,path])?;}
                if let Some(hash)=current.get(path){c.execute("INSERT INTO project_observations VALUES(?1,?2,?3) ON CONFLICT(project,path) DO UPDATE SET hash=excluded.hash",params![project,path,hash])?;}else{c.execute("DELETE FROM project_observations WHERE project=?1 AND path=?2",params![project,path])?;}
            }
            Ok(())
        }).await.map_err(|e|e.to_string())?;
        let mut changes = Vec::new();
        loop {
            let batch = self.drain().await?;
            let more = batch.len() == 256;
            changes.extend(batch);
            if !more {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok(changes)
    }
    async fn drain(&self) -> Result<Vec<ProjectChange>, String> {
        let tools = &self.inner.tools;
        let project = tools.root.to_string_lossy().into_owned();
        let changes=tools.db.read(ReadLane::Interactive,move|c|{
            let mut stmt=c.prepare("SELECT pc.id,pc.path,pc.previous_hash,pc.current_hash,pc.external,COALESCE(ps.changed,1) FROM project_changes pc LEFT JOIN project_change_semantics ps ON ps.change_id=pc.id WHERE pc.project=?1 AND pc.state='pending' ORDER BY pc.id LIMIT 256")?;
            Ok(stmt.query_map([project],|r|Ok(ProjectChange{id:r.get(0)?,path:PathBuf::from(r.get::<_,String>(1)?),previous_hash:r.get(2)?,current_hash:r.get(3)?,external:r.get(4)?,semantic_changed:r.get(5)?}))?.collect::<Result<Vec<_>,_>>()?)
        }).await.map_err(|e|e.to_string())?;
        let journal = excalibur_journal::Journal::new(tools.db.clone())
            .await
            .map_err(|e| e.to_string())?;
        let selected = changes
            .iter()
            .map(|change| tools.root.join(&change.path))
            .collect::<BTreeSet<_>>();
        let contents = tools.authorized_project_files_selected(Some(&selected))?;
        for change in &changes {
            if change.path.as_os_str().is_empty()
                || change
                    .path
                    .components()
                    .any(|p| !matches!(p, std::path::Component::Normal(_)))
            {
                return Err("invalid queued project path".into());
            }
            // Revocation/deletion may now be a symlink outside the root. Do not
            // follow it merely to invalidate previously authorized metadata.
            let relative = change
                .path
                .to_str()
                .ok_or("non UTF-8 project path")?
                .to_owned();
            let content = contents.get(Path::new(&relative)).cloned();
            if content.as_deref().map(crate::files::hash) != change.current_hash {
                // A later queue row supersedes this intermediate content. Never index stale bytes.
                let id = change.id;
                tools
                    .db
                    .write(Priority::Normal, move |c| {
                        c.execute(
                            "UPDATE project_changes SET state='superseded' WHERE id=?1",
                            [id],
                        )?;
                        Ok(())
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                continue;
            }
            for target in self
                .inner
                .evidence
                .iter()
                .filter(|_| change.semantic_changed)
            {
                target
                    .tasks
                    .invalidate_paths(
                        &target.scope,
                        &[tools.root.join(&relative).to_string_lossy().into_owned()],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
            }
            if let Some(observer) = &self.inner.observer {
                observer.changed(change, content).await?;
            }
            if change.external && change.semantic_changed {
                let path = tools.root.join(&relative).to_string_lossy().into_owned();
                let holders = tools
                    .db
                    .read(ReadLane::Interactive, move |c| {
                        let mut stmt = c.prepare(
                            "SELECT holder_id FROM leases WHERE path=?1 AND expires_at>unixepoch()",
                        )?;
                        Ok(stmt
                            .query_map([path], |r| r.get::<_, String>(0))?
                            .collect::<Result<Vec<_>, _>>()?)
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                for run in holders {
                    let id = format!("external-file:{}", change.id);
                    let text = tools.policy.redactor.redact_text(&format!(
                        "{} changed outside Excalibur; its previous read fingerprint is stale.",
                        relative
                    ));
                    journal
                        .append(
                            &run,
                            "directive_received",
                            &format!("directive:{id}:received"),
                            serde_json::json!({"id":id,"text":text}),
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
            let id = change.id;
            tools
                .db
                .write(Priority::Normal, move |c| {
                    c.execute(
                        "UPDATE project_changes SET state='processed' WHERE id=?1",
                        [id],
                    )?;
                    Ok(())
                })
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(changes)
    }
}

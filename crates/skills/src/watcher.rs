//! Filesystem notification plus periodic recovery from lost events.
use crate::index::SkillIndex;
use notify::{RecursiveMode, Watcher};
use std::{sync::Arc, time::Duration};

pub struct SkillWatcher {
    task: tokio::task::JoinHandle<()>,
}
impl Drop for SkillWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl SkillWatcher {
    pub fn start(index: Arc<SkillIndex>) -> Result<Self, String> {
        Self::with_interval(index, Duration::from_secs(30 * 60))
    }
    pub fn with_interval(index: Arc<SkillIndex>, interval: Duration) -> Result<Self, String> {
        if interval.is_zero() {
            return Err("rescan interval must be positive".into());
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut watcher =
            notify::recommended_watcher(move |_event: notify::Result<notify::Event>| {
                let _ = tx.try_send(());
            })
            .map_err(|e| e.to_string())?;
        for root in index.roots() {
            if let Some(path) = &root.path {
                // Watch the closest existing parent too, so creating a missing
                // skills root is noticed. Periodic scans remain the backstop.
                let mut existing = path.as_path();
                while !existing.exists() {
                    existing = existing
                        .parent()
                        .ok_or("skill root has no existing parent")?;
                }
                let mode = if existing == path.as_path() {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                };
                watcher.watch(existing, mode).map_err(|e| e.to_string())?;
            }
        }
        let task = tokio::spawn(async move {
            let mut watcher = watcher;
            let mut watched = std::collections::BTreeSet::new();
            let mut timer = tokio::time::interval(interval);
            loop {
                tokio::select! { _ = timer.tick() => {}, event = rx.recv() => { if event.is_none() { break; } } }
                for root in index.roots() {
                    if let Some(path) = &root.path
                        && path.is_dir()
                        && !watched.contains(path)
                        && watcher.watch(path, RecursiveMode::Recursive).is_ok()
                    {
                        watched.insert(path.clone());
                    }
                }
                index.refresh().await;
            }
        });
        Ok(Self { task })
    }
}

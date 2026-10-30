use crate::git::command;
use crate::typed_entries::TreeEntry;
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub run_id: String,
    pub step: u64,
    pub phase: String,
    pub tree: String,
    pub created_at: i64,
}
#[derive(Clone)]
pub struct Shadow {
    pub(crate) workspace: Workspace,
}
impl Shadow {
    pub(crate) fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
    fn directory(&self) -> PathBuf {
        self.workspace
            .data
            .join("shadow")
            .join(hash(self.workspace.root.to_string_lossy().as_bytes()))
    }
    pub(crate) async fn git(
        &self,
        args: Vec<String>,
        index: Option<&Path>,
        input: Option<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let directory = self.directory();
        std::fs::create_dir_all(&directory)?;
        let repository = directory.join("objects.git");
        let mut env = vec![("GIT_DIR".into(), repository.to_string_lossy().into_owned())];
        if let Some(index) = index {
            env.push((
                "GIT_INDEX_FILE".into(),
                index.to_string_lossy().into_owned(),
            ));
        }
        let (ok, out, error) =
            command(&directory, &args, &env, input, &self.workspace.redactor).await?;
        if !ok {
            return Err(Error::Git(error));
        }
        Ok(out)
    }
    /// Input is supplied by the host's existing capability-aware file reader. This
    /// library never walks/reads project files or follows private hardlinks itself.
    ///
    /// Returns the stored snapshot plus non-fatal skip warnings (oversized or
    /// secret content is excluded from the snapshot instead of failing it).
    pub async fn snapshot_files(
        &self,
        run: &str,
        step: u64,
        phase: &str,
        files: BTreeMap<PathBuf, Vec<u8>>,
    ) -> Result<(Snapshot, Vec<String>)> {
        self.snapshot_entries(
            run,
            step,
            phase,
            files
                .into_iter()
                .map(|(path, content)| (path, SnapshotEntry::file(content, false)))
                .collect(),
        )
        .await
    }
    pub async fn snapshot_entries(
        &self,
        run: &str,
        step: u64,
        phase: &str,
        files: BTreeMap<PathBuf, SnapshotEntry>,
    ) -> Result<(Snapshot, Vec<String>)> {
        if phase.is_empty() || phase.len() > 64 || phase.chars().any(char::is_control) {
            return Err(Error::Invalid("snapshot phase invalid"));
        }
        let id = hash(format!("{}:{run}:{step}:{phase}", self.workspace.root.display()).as_bytes());
        if let Some(existing) = self.lookup(&id).await? {
            self.pin_tree(&existing.tree).await?;
            return Ok((existing, Vec::new()));
        }
        let directory = self.directory();
        std::fs::create_dir_all(&directory)?;
        let repository = directory.join("objects.git");
        if !repository.exists() {
            let (ok, _, error) = command(
                &directory,
                &[
                    "init".into(),
                    "--bare".into(),
                    repository.to_string_lossy().into_owned(),
                ],
                &[],
                None,
                &self.workspace.redactor,
            )
            .await?;
            if !ok {
                return Err(Error::Git(error));
            }
        }
        let index = directory.join(format!("index-{}", excalibur_types::JobId::new()));
        self.git(
            vec!["read-tree".into(), "--empty".into()],
            Some(&index),
            None,
        )
        .await?;
        let result = async {
            let mut total = 0usize;
            let mut warnings: Vec<String> = Vec::new();
            // Validate every entry before touching the object store, so an
            // oversized or secret file degrades to a skipped path with a
            // warning instead of failing the whole snapshot.
            let mut plan: Vec<(String, String, Vec<u8>)> = Vec::new();
            for (path, entry) in files {
                if entry.kind == EntryKind::Symlink && entry.executable {
                    return Err(Error::Invalid(
                        "symbolic-link executable flag must be false",
                    ));
                }
                if path.is_absolute()
                    || path
                        .components()
                        .any(|c| !matches!(c, Component::Normal(_)))
                    || path.components().any(|c| c.as_os_str() == ".git")
                {
                    return Err(Error::Invalid(
                        "snapshot paths must be normalized relative files",
                    ));
                }
                let name = path
                    .to_str()
                    .ok_or(Error::Invalid("snapshot path must be UTF-8"))?
                    .to_owned();
                if name.bytes().any(|b| b == 0) {
                    return Err(Error::Invalid(
                        "snapshot path contains an embedded NUL byte",
                    ));
                }
                let mode = entry.mode().to_owned();
                let bytes = entry.content;
                total = total
                    .checked_add(bytes.len())
                    .ok_or(Error::Invalid("snapshot size overflow"))?;
                if bytes.len() > 64 * 1024 * 1024 || total > 256 * 1024 * 1024 {
                    warnings.push(format!("snapshot skips oversized path: {name}"));
                    continue;
                }
                if self.workspace.redactor.contains_secret(&bytes) {
                    warnings.push(format!("snapshot skips path with secret content: {name}"));
                    continue;
                }
                plan.push((mode, name, bytes));
            }
            // Materialize every blob once, then hash all of them in a single
            // hash-object process instead of one process per file, with
            // identical resulting objects. Staging names are positional, so
            // hostile project names never reach argv or stdin.
            let mut staged: Vec<(String, String, String)> = Vec::new();
            if !plan.is_empty() {
                let staging = directory.join(format!("staging-{}", excalibur_types::JobId::new()));
                std::fs::create_dir_all(&staging)?;
                let hashed = async {
                    let mut input = Vec::new();
                    for (sequence, (_, _, bytes)) in plan.iter().enumerate() {
                        let staging_name = format!("{sequence:06}");
                        std::fs::write(staging.join(&staging_name), bytes)?;
                        input.extend_from_slice(
                            staging.join(&staging_name).to_string_lossy().as_bytes(),
                        );
                        input.push(b'\n');
                    }
                    let out = self
                        .git(
                            vec![
                                "hash-object".into(),
                                "-w".into(),
                                "--no-filters".into(),
                                "--stdin-paths".into(),
                            ],
                            Some(&index),
                            Some(input),
                        )
                        .await?;
                    String::from_utf8(out).map_err(|_| Error::Invalid("invalid shadow object"))
                }
                .await;
                let _ = std::fs::remove_dir_all(&staging);
                let oids = hashed?;
                let mut oids: Vec<&str> = oids.lines().collect();
                if oids.len() != plan.len() {
                    return Err(Error::Invalid("shadow object count mismatch"));
                }
                for ((mode, name, _), oid) in plan.drain(..).zip(oids.drain(..)) {
                    staged.push((mode, oid.trim().to_owned(), name));
                }
            }
            if !staged.is_empty() {
                // NUL-terminated entries (`-z`): names travel literally, so
                // quotes are never interpreted and embedded newlines cannot
                // start a forged entry.
                let mut info = Vec::new();
                for (mode, oid, name) in &staged {
                    info.extend_from_slice(format!("{mode} {oid} 0\t{name}").as_bytes());
                    info.push(0);
                }
                self.git(
                    vec![
                        "update-index".into(),
                        "--add".into(),
                        "-z".into(),
                        "--index-info".into(),
                    ],
                    Some(&index),
                    Some(info),
                )
                .await?;
            }
            let tree = String::from_utf8(
                self.git(vec!["write-tree".into()], Some(&index), None)
                    .await?,
            )
            .map_err(|_| Error::Invalid("invalid shadow tree"))?
            .trim()
            .to_owned();
            let snapshot = Snapshot {
                id: id.clone(),
                run_id: run.into(),
                step,
                phase: phase.into(),
                tree,
                created_at: excalibur_store::unix_seconds(),
            };
            // Immutable refs are keyed by tree, not snapshot ID: concurrent
            // attempts at one receipt cannot overwrite the winner's root.
            // Publish the GC root before making the SQLite receipt visible.
            self.pin_tree(&snapshot.tree).await?;
            let (row, root) = (
                snapshot.clone(),
                self.workspace.root.to_string_lossy().into_owned(),
            );
            self.workspace
                .db
                .write(Priority::Journal, move |c| {
                    c.execute(
                        "INSERT OR IGNORE INTO workspace_snapshots VALUES(?1,?2,?3,?4,?5,?6,?7)",
                        excalibur_store::rusqlite::params![
                            row.id,
                            root,
                            row.run_id,
                            row.step,
                            row.phase,
                            row.tree,
                            row.created_at
                        ],
                    )?;
                    Ok(())
                })
                .await?;
            let found = self.lookup(&id).await?;
            let snapshot = found.ok_or(Error::Invalid("snapshot receipt missing"))?;
            self.collect_garbage().await;
            Ok((snapshot, warnings))
        }
        .await;
        let _ = std::fs::remove_file(index);
        result
    }
    async fn pin_tree(&self, tree: &str) -> Result<()> {
        self.git(
            vec![
                "update-ref".into(),
                format!("refs/excalibur/trees/{tree}"),
                tree.into(),
            ],
            None,
            None,
        )
        .await?;
        Ok(())
    }
    /// Upgrade legacy receipts before collecting. Never expire a live run's
    /// history by snapshot count; time-based retention belongs to H9.
    async fn protect_recorded_trees(&self) -> Result<()> {
        let root = self.workspace.root.to_string_lossy().into_owned();
        let trees = self
            .workspace
            .db
            .read(ReadLane::Background, move |c| {
                let mut q =
                    c.prepare("SELECT DISTINCT tree FROM workspace_snapshots WHERE root=?1")?;
                Ok(q.query_map([root], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        for tree in trees {
            self.pin_tree(&tree).await?;
        }
        Ok(())
    }
    /// Rate-limited explicit `gc`: loose objects accumulate on every
    /// snapshot, and only a collection older than one hour is eligible.
    async fn collect_garbage(&self) {
        let loose = self
            .git(vec!["count-objects".into(), "-v".into()], None, None)
            .await
            .ok()
            .and_then(|out| {
                String::from_utf8(out).ok()?.lines().find_map(|line| {
                    line.strip_prefix("count: ")
                        .and_then(|n| n.trim().parse::<usize>().ok())
                })
            })
            .unwrap_or(0);
        if loose < 1000 {
            return;
        }
        let marker = self.directory().join("last-shadow-gc");
        let stale = std::fs::metadata(&marker)
            .and_then(|meta| meta.modified())
            .map(|modified| {
                modified
                    .elapsed()
                    .is_ok_and(|age| age > std::time::Duration::from_secs(3600))
            })
            .unwrap_or(true);
        if !stale {
            return;
        }
        // A missing/corrupt legacy root prevents collection rather than
        // exposing other retained snapshots to destructive pruning.
        if self.protect_recorded_trees().await.is_err() {
            eprintln!("shadow collection deferred: a retained tree could not be protected");
            return;
        }
        if self
            .git(
                vec!["gc".into(), "--prune=1.hour.ago".into(), "--quiet".into()],
                None,
                None,
            )
            .await
            .is_ok()
        {
            let _ = std::fs::write(&marker, excalibur_store::unix_seconds().to_string());
        }
    }
    pub(crate) async fn lookup(&self, id: &str) -> Result<Option<Snapshot>> {
        let (id, root) = (
            id.to_owned(),
            self.workspace.root.to_string_lossy().into_owned(),
        );
        Ok(self.workspace.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT id,run_id,step,phase,tree,created_at FROM workspace_snapshots WHERE id=?1 AND root=?2",excalibur_store::rusqlite::params![id,root],|r|Ok(Snapshot{id:r.get(0)?,run_id:r.get(1)?,step:r.get(2)?,phase:r.get(3)?,tree:r.get(4)?,created_at:r.get(5)?})).optional()?)).await?)
    }
    pub async fn list_snapshots(&self, run: &str) -> Result<Vec<Snapshot>> {
        let (run, root) = (
            run.to_owned(),
            self.workspace.root.to_string_lossy().into_owned(),
        );
        Ok(self.workspace.db.read(ReadLane::Interactive,move|c|{let mut q=c.prepare("SELECT id,run_id,step,phase,tree,created_at FROM workspace_snapshots WHERE run_id=?1 AND root=?2 ORDER BY step,created_at,id")?;Ok(q.query_map(excalibur_store::rusqlite::params![run,root],|r|Ok(Snapshot{id:r.get(0)?,run_id:r.get(1)?,step:r.get(2)?,phase:r.get(3)?,tree:r.get(4)?,created_at:r.get(5)?}))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?)
    }
    pub(crate) async fn tree(&self, id: &str) -> Result<BTreeMap<PathBuf, TreeEntry>> {
        let snapshot = self
            .lookup(id)
            .await?
            .ok_or(Error::Invalid("shadow snapshot not in workspace"))?;
        let bytes = self
            .git(
                vec!["ls-tree".into(), "-r".into(), "-z".into(), snapshot.tree],
                None,
                None,
            )
            .await?;
        let mut files = BTreeMap::new();
        for entry in bytes.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let text = std::str::from_utf8(entry)
                .map_err(|_| Error::Invalid("snapshot tree path not UTF-8"))?;
            let (meta, path) = text
                .split_once('\t')
                .ok_or(Error::Invalid("invalid shadow tree record"))?;
            let oid = meta
                .split_whitespace()
                .nth(2)
                .ok_or(Error::Invalid("invalid shadow object"))?;
            let mode = meta
                .split_whitespace()
                .next()
                .ok_or(Error::Invalid("shadow entry mode missing"))?;
            files.insert(
                PathBuf::from(path),
                TreeEntry {
                    oid: oid.into(),
                    mode: mode.into(),
                },
            );
        }
        Ok(files)
    }
    /// Entry names only; lets a capability-aware host validate a historical
    /// capture without opening or diffing any stored file content.
    pub async fn snapshot_paths(&self, snapshot: &str) -> Result<Vec<PathBuf>> {
        Ok(self.tree(snapshot).await?.into_keys().collect())
    }
    pub async fn changed_files(&self, before: &str, after: &str) -> Result<Vec<PathBuf>> {
        let (a, b) = (self.tree(before).await?, self.tree(after).await?);
        Ok(a.keys()
            .chain(b.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|path| a.get(*path) != b.get(*path))
            .cloned()
            .collect())
    }
    pub async fn diff(&self, before: &str, after: &str) -> Result<String> {
        let a = self
            .lookup(before)
            .await?
            .ok_or(Error::Invalid("snapshot missing"))?;
        let b = self
            .lookup(after)
            .await?
            .ok_or(Error::Invalid("snapshot missing"))?;
        let bytes = self
            .git(
                vec!["diff-tree".into(), "-p".into(), a.tree, b.tree],
                None,
                None,
            )
            .await?;
        Ok(self
            .workspace
            .redactor
            .redact_text(&String::from_utf8_lossy(&bytes)))
    }
    pub async fn plan_restore(
        &self,
        before: &str,
        current: BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) -> Result<RevertPlan> {
        let snapshot = self
            .lookup(before)
            .await?
            .ok_or(Error::Invalid("snapshot missing"))?;
        let after = self
            .list_snapshots(&snapshot.run_id)
            .await?
            .into_iter()
            .find(|s| s.step == snapshot.step && s.phase == "after")
            .ok_or(Error::Invalid(
                "restore requires a paired after-step snapshot",
            ))?;
        self.plan_between(before, &after.id, current).await
    }
    /// Restore only the explicitly selected guarded paths. Unselected contents
    /// are neither loaded from Git nor added to the resulting editing plan.
    pub async fn plan_restore_selected(
        &self,
        before: &str,
        current: BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) -> Result<RevertPlan> {
        let snapshot = self
            .lookup(before)
            .await?
            .ok_or(Error::Invalid("snapshot missing"))?;
        let after = self
            .list_snapshots(&snapshot.run_id)
            .await?
            .into_iter()
            .find(|s| s.step == snapshot.step && s.phase == "after")
            .ok_or(Error::Invalid(
                "restore requires a paired after-step snapshot",
            ))?;
        self.plan_between_inner(before, &after.id, current, true)
            .await
    }
    pub async fn plan_between(
        &self,
        desired: &str,
        base: &str,
        current: BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) -> Result<RevertPlan> {
        self.plan_between_inner(desired, base, current, false).await
    }
    async fn plan_between_inner(
        &self,
        desired: &str,
        base: &str,
        current: BTreeMap<PathBuf, Option<Vec<u8>>>,
        selected: bool,
    ) -> Result<RevertPlan> {
        self.plan_between_entries(
            desired,
            base,
            current
                .into_iter()
                .map(|(path, content)| {
                    (path, content.map(|bytes| SnapshotEntry::file(bytes, false)))
                })
                .collect(),
            selected,
        )
        .await
    }
}

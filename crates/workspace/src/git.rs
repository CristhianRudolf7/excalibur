use crate::*;
use excalibur_store::rusqlite::params;
use excalibur_types::runtime::BoxFuture;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
pub(crate) struct GitLock(std::fs::File);
impl Drop for GitLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProjectMode {
    Shared,
    Worktrees,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Worktree {
    pub task: String,
    pub owner: String,
    pub branch: String,
    pub path: PathBuf,
    pub integration_branch: String,
    pub integration_path: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TestReport {
    pub passed: bool,
    pub checks_run: u64,
    pub not_applicable_reason: Option<String>,
    pub report: String,
}
pub trait IntegrationChecks: Send + Sync {
    fn verify<'a>(
        &'a self,
        worktree: &'a Worktree,
        changed: &'a [String],
    ) -> BoxFuture<'a, Result<TestReport>>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegrationReport {
    pub id: String,
    pub state: String,
    pub child: String,
    pub target_before: Option<String>,
    pub rebased_oid: Option<String>,
    pub intent: String,
    pub files: Vec<String>,
    pub child_diff: String,
    pub integration_diff: String,
    pub tests: Option<TestReport>,
    pub detail: String,
}
#[derive(Clone)]
pub struct GitWorkspace {
    pub(crate) workspace: Workspace,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Overlap {
    pub file: PathBuf,
    pub owners: Vec<String>,
}
pub fn overlaps(assignments: &BTreeMap<String, BTreeSet<PathBuf>>) -> Vec<Overlap> {
    let mut files: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for (owner, paths) in assignments {
        for path in paths {
            files.entry(path.clone()).or_default().push(owner.clone());
        }
    }
    files
        .into_iter()
        .filter(|(_, owners)| owners.len() > 1)
        .map(|(file, owners)| Overlap { file, owners })
        .collect()
}
pub(crate) async fn command(
    cwd: &Path,
    args: &[String],
    environment: &[(String, String)],
    input: Option<Vec<u8>>,
    redactor: &Redactor,
) -> Result<(bool, Vec<u8>, String)> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LANG", "C.UTF-8")
        .env("GIT_PAGER", "cat")
        .current_dir(cwd)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "rebase.updateRefs=false",
            "-c",
            "rebase.autoStash=false",
            "-c",
            "rerere.enabled=false",
            "-c",
            "core.pager=cat",
            "-c",
            "credential.helper=",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "core.excludesFile=/dev/null",
            "-c",
            "gc.auto=0",
            "-c",
            "protocol.allow=never",
        ])
        .args(if args.first().is_some_and(|arg| arg == "init") {
            Vec::new()
        } else {
            vec![format!("--work-tree={}", cwd.display())]
        })
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
    ] {
        cmd.env_remove(key);
    }
    for (k, v) in environment {
        cmd.env(k, v);
    }
    if args.first().is_none_or(|arg| arg != "init") {
        let mut probe = tokio::process::Command::new("git");
        probe
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .current_dir(cwd)
            .args([
                "config",
                "--local",
                "--no-includes",
                "--name-only",
                "--null",
                "--list",
            ])
            .kill_on_drop(true);
        for (k, v) in environment {
            probe.env(k, v);
        }
        let checked = tokio::time::timeout(Duration::from_secs(10), probe.output())
            .await
            .map_err(|_| Error::Invalid("git configuration inspection timed out"))??;
        if !checked.status.success() || checked.stdout.len() > 1024 * 1024 {
            return Err(Error::Invalid("cannot validate local git configuration"));
        }
        for key in checked
            .stdout
            .split(|b| *b == 0)
            .filter(|key| !key.is_empty())
        {
            let key = String::from_utf8_lossy(key).to_ascii_lowercase();
            if key == "include.path"
                || key.starts_with("includeif.")
                || key.starts_with("filter.")
                || (key.starts_with("diff.")
                    && (key.ends_with(".command") || key.ends_with(".textconv")))
                || (key.starts_with("merge.") && key.ends_with(".driver"))
            {
                return Err(Error::Invalid(
                    "custom git includes/execution drivers require a confined host adapter",
                ));
            }
        }
    }
    cmd.kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let task = async {
        let write = async {
            if let Some(input) = input {
                stdin.write_all(&input).await?;
            }
            drop(stdin);
            Ok::<_, std::io::Error>(())
        };
        let readout = async {
            let mut out = Vec::new();
            stdout
                .take(64 * 1024 * 1024 + 1)
                .read_to_end(&mut out)
                .await?;
            Ok::<_, std::io::Error>(out)
        };
        let readerr = async {
            let mut err = Vec::new();
            stderr.take(1024 * 1024 + 1).read_to_end(&mut err).await?;
            Ok::<_, std::io::Error>(err)
        };
        let (_, out, err, status) = tokio::try_join!(write, readout, readerr, child.wait())?;
        if out.len() > 64 * 1024 * 1024 || err.len() > 1024 * 1024 {
            return Err(Error::Invalid("git output exceeds bounded limit"));
        }
        Ok((
            status.success(),
            out,
            redactor
                .redact_text(&String::from_utf8_lossy(&err))
                .chars()
                .take(4096)
                .collect(),
        ))
    };
    tokio::time::timeout(Duration::from_secs(120), task)
        .await
        .map_err(|_| Error::Invalid("git operation timed out; inspect durable queue state"))?
}
impl GitWorkspace {
    pub(crate) fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
    pub(crate) async fn git(&self, path: &Path, args: &[&str]) -> Result<String> {
        let args = args.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (ok, out, error) = command(path, &args, &[], None, &self.workspace.redactor).await?;
        if !ok {
            return Err(Error::Git(error));
        }
        String::from_utf8(out).map_err(|_| Error::Invalid("git metadata must be UTF-8"))
    }
    pub async fn mode(&self) -> Result<ProjectMode> {
        let git = self.workspace.root.join(".git");
        if !git.exists() {
            return Ok(ProjectMode::Shared);
        }
        let modules = self
            .git(&self.workspace.root, &["ls-files", "--stage"])
            .await?;
        if self.workspace.root.join(".gitmodules").exists()
            || modules.lines().any(|line| line.starts_with("160000 "))
        {
            Ok(ProjectMode::Shared)
        } else {
            Ok(ProjectMode::Worktrees)
        }
    }
    pub async fn create(&self, task: &str, owner: &str) -> Result<Worktree> {
        if self.workspace.data.starts_with(&self.workspace.root) {
            return Err(Error::Invalid(
                "worktrees require a host data directory outside the project",
            ));
        }
        if task.is_empty() || owner.is_empty() {
            return Err(Error::Invalid("task and child owner required"));
        }
        if self.mode().await? != ProjectMode::Worktrees {
            return Err(Error::Invalid(
                "project requires shared-tree mode (no git or submodules)",
            ));
        }
        let task_key = hash(format!("{}:{task}", self.workspace.root.display()).as_bytes());
        let child_key = hash(owner.as_bytes());
        let integration_branch = format!("excalibur/integration/{task_key}");
        let branch = format!("excalibur/task/{task_key}/{child_key}");
        let base = self.workspace.data.join("worktrees").join(&task_key);
        std::fs::create_dir_all(&base)?;
        let integration_path = base.join("integration");
        let path = base.join(&child_key);
        let _guard = self.lock(&format!("task:{task_key}"))?;
        let root = self.workspace.root.to_string_lossy().into_owned();
        let key = task.to_owned();
        let existing=self.workspace.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT root,integration_branch,integration_path,base_oid FROM workspace_tasks WHERE id=?1",[key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?))).optional()?)).await?;
        if let Some((stored, _, _, _)) = &existing
            && stored != &root
        {
            return Err(Error::Invalid("worktree task belongs to another project"));
        }
        let base_oid = if let Some((_, _, _, base)) = existing {
            base
        } else {
            self.git(&self.workspace.root, &["rev-parse", "HEAD"])
                .await?
                .trim()
                .to_owned()
        };
        let (id, b, p, base_copy) = (
            task.to_owned(),
            integration_branch.clone(),
            integration_path.to_string_lossy().into_owned(),
            base_oid.clone(),
        );
        self.workspace.db.write(Priority::Journal,move|c|{c.execute("INSERT OR IGNORE INTO workspace_tasks(id,root,integration_branch,integration_path,base_oid,state) VALUES(?1,?2,?3,?4,?5,'preparing')",params![id,root,b,p,base_copy])?;Ok(())}).await?;
        self.ensure_worktree(&integration_path, &integration_branch, &base_oid)
            .await?;
        let (t, o, b, p) = (
            task.to_owned(),
            owner.to_owned(),
            branch.clone(),
            path.to_string_lossy().into_owned(),
        );
        self.workspace.db.write(Priority::Journal,move|c|{c.execute("INSERT OR IGNORE INTO workspace_children(task_id,owner,branch,path,state) VALUES(?1,?2,?3,?4,'preparing')",params![t,o,b,p])?;Ok(())}).await?;
        self.ensure_worktree(&path, &branch, &integration_branch)
            .await?;
        let (t, o) = (task.to_owned(), owner.to_owned());
        self.workspace
            .db
            .write(Priority::Journal, move |c| {
                c.execute(
                    "UPDATE workspace_tasks SET state='active' WHERE id=?1",
                    [&t],
                )?;
                c.execute(
                    "UPDATE workspace_children SET state='active' WHERE task_id=?1 AND owner=?2",
                    params![t, o],
                )?;
                Ok(())
            })
            .await?;
        Ok(Worktree {
            task: task.into(),
            owner: owner.into(),
            branch,
            path,
            integration_branch,
            integration_path,
        })
    }
    async fn ensure_worktree(&self, path: &Path, branch: &str, start: &str) -> Result<()> {
        if !path.exists() {
            let reference = format!("refs/heads/{branch}");
            let exists = command(
                &self.workspace.root,
                &[
                    "show-ref".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    reference,
                ],
                &[],
                None,
                &self.workspace.redactor,
            )
            .await?
            .0;
            if exists {
                self.git(
                    &self.workspace.root,
                    &[
                        "worktree",
                        "add",
                        path.to_str()
                            .ok_or(Error::Invalid("worktree path invalid"))?,
                        branch,
                    ],
                )
                .await?;
            } else {
                self.git(
                    &self.workspace.root,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        branch,
                        path.to_str()
                            .ok_or(Error::Invalid("worktree path invalid"))?,
                        start,
                    ],
                )
                .await?;
            }
        }
        self.validate_owned(path, branch).await
    }
    pub(crate) async fn validate_owned(&self, path: &Path, branch: &str) -> Result<()> {
        if !path.canonicalize()?.starts_with(&self.workspace.data)
            || !branch.starts_with("excalibur/")
        {
            return Err(Error::Invalid(
                "git operation is outside an owned task worktree",
            ));
        }
        if self
            .git(path, &["symbolic-ref", "--short", "HEAD"])
            .await?
            .trim()
            != branch
        {
            return Err(Error::Invalid(
                "worktree branch changed externally; user branch will not be modified",
            ));
        }
        Ok(())
    }
    pub async fn list_worktrees(&self, task: &str) -> Result<Vec<Worktree>> {
        let task_owned = task.to_owned();
        let project_root = self.workspace.root.to_string_lossy().into_owned();
        let owners=self.workspace.db.read(ReadLane::Interactive,move|c|{let mut q=c.prepare("SELECT c.owner FROM workspace_children c JOIN workspace_tasks t ON t.id=c.task_id WHERE c.task_id=?1 AND c.state<>'removed' AND t.root=?2 ORDER BY c.owner")?;Ok(q.query_map(params![task_owned,project_root],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?)}).await?;
        let mut result = Vec::new();
        for owner in owners {
            result.push(self.worktree(task, &owner).await?);
        }
        Ok(result)
    }
    pub async fn remove(&self, task: &str, owner: &str) -> Result<()> {
        let tree = self.worktree(task, owner).await?;
        let _guard = self.lock(&format!("integration:{task}"))?;
        if tree.path.exists() {
            self.validate_owned(&tree.path, &tree.branch).await?;
            if !self
                .git(&tree.path, &["status", "--porcelain"])
                .await?
                .is_empty()
            {
                return Err(Error::Invalid("dirty worktree cannot be removed"));
            }
            self.git(
                &self.workspace.root,
                &[
                    "worktree",
                    "remove",
                    tree.path
                        .to_str()
                        .ok_or(Error::Invalid("worktree path invalid"))?,
                ],
            )
            .await?;
        }
        let (task, owner) = (task.to_owned(), owner.to_owned());
        self.workspace
            .db
            .write(Priority::Journal, move |c| {
                c.execute(
                    "UPDATE workspace_children SET state='removed' WHERE task_id=?1 AND owner=?2",
                    params![task, owner],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    pub(crate) fn lock(&self, key: &str) -> Result<GitLock> {
        let directory = self.workspace.data.join("git-locks");
        std::fs::create_dir_all(&directory)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(hash(key.as_bytes())))?;
        file.try_lock()
            .map_err(|_| Error::Invalid("another integration operation holds the project lock"))?;
        Ok(GitLock(file))
    }
    pub async fn worktree(&self, task: &str, owner: &str) -> Result<Worktree> {
        let (t, o) = (task.to_owned(), owner.to_owned());
        let root = self.workspace.root.to_string_lossy().into_owned();
        Ok(self.workspace.db.read(ReadLane::Interactive,move|c|Ok(c.query_row("SELECT c.branch,c.path,t.integration_branch,t.integration_path FROM workspace_children c JOIN workspace_tasks t ON t.id=c.task_id WHERE c.task_id=?1 AND c.owner=?2 AND t.root=?3",params![t,o,root],|r|Ok(Worktree{task:t.clone(),owner:o.clone(),branch:r.get(0)?,path:PathBuf::from(r.get::<_,String>(1)?),integration_branch:r.get(2)?,integration_path:PathBuf::from(r.get::<_,String>(3)?)}))?)).await?)
    }
    pub async fn enqueue(&self, task: &str, owner: &str, intent: &str) -> Result<String> {
        let tree = self.worktree(task, owner).await?;
        self.validate_owned(&tree.path, &tree.branch).await?;
        if !self
            .git(&tree.path, &["status", "--porcelain"])
            .await?
            .is_empty()
        {
            return Err(Error::Invalid("checkpoint child changes before enqueue"));
        }
        let oid = self
            .git(&tree.path, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_owned();
        let id = hash(format!("{task}:{owner}:{oid}").as_bytes());
        let (id_copy, t, o, intent) = (
            id.clone(),
            task.to_owned(),
            owner.to_owned(),
            self.workspace.redactor.redact_text(intent),
        );
        self.workspace.db.write(Priority::Journal,move|c|{c.execute("INSERT OR IGNORE INTO workspace_integrations(id,task_id,child,source_oid,intent,state,created_at) VALUES(?1,?2,?3,?4,?5,'queued',unixepoch())",params![id_copy,t,o,oid,intent])?;Ok(())}).await?;
        Ok(id)
    }
    pub async fn list(&self, task: &str) -> Result<Vec<IntegrationReport>> {
        let task = task.to_owned();
        let root = self.workspace.root.to_string_lossy().into_owned();
        Ok(self.workspace.db.read(ReadLane::Interactive,move|c|{let mut q=c.prepare("SELECT i.id,i.state,i.child,i.target_before,i.rebased_oid,i.intent,i.report FROM workspace_integrations i JOIN workspace_tasks t ON t.id=i.task_id WHERE i.task_id=?1 AND t.root=?2 ORDER BY i.created_at,i.rowid")?;let rows=q.query_map(params![task,root],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;Ok(rows.into_iter().map(|(id,state,child,target_before,rebased_oid,intent,report)|{let mut parsed:IntegrationReport=serde_json::from_str(&report).unwrap_or(IntegrationReport{id:id.clone(),state:state.clone(),child,target_before,rebased_oid,intent,files:vec![],child_diff:String::new(),integration_diff:String::new(),tests:None,detail:String::new()});parsed.state=state;parsed}).collect())}).await?)
    }
    pub async fn process_next(
        &self,
        task: &str,
        identity: &GitIdentity,
        checks: &dyn IntegrationChecks,
    ) -> Result<Option<IntegrationReport>> {
        let _guard = self.lock(&format!("integration:{task}"))?;
        let reports = self.list(task).await?;
        let Some(mut report) = reports.into_iter().find(|r| {
            matches!(
                r.state.as_str(),
                "queued" | "rebasing" | "testing" | "advancing"
            )
        }) else {
            return Ok(None);
        };
        let tree = self.worktree(task, &report.child).await?;
        self.validate_owned(&tree.path, &tree.branch).await?;
        self.validate_owned(&tree.integration_path, &tree.integration_branch)
            .await?;
        if report.state == "testing" {
            report.state = "returned".into();
            report.detail = "tests were interrupted; an explicit retry is required".into();
            self.save(&report).await?;
            return Ok(Some(report));
        }
        let id = report.id.clone();
        let source = self
            .workspace
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT source_oid FROM workspace_integrations WHERE id=?1",
                    [id],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .await?;
        if report.state == "queued"
            && self.git(&tree.path, &["rev-parse", "HEAD"]).await?.trim() != source
        {
            report.state = "returned".into();
            report.detail =
                "child source changed after enqueue; explicitly submit the new checkpoint".into();
            self.save(&report).await?;
            return Ok(Some(report));
        }
        let current = self
            .git(&tree.integration_path, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_owned();
        if report.state == "advancing" {
            if report.rebased_oid.as_deref() == Some(&current)
                && self
                    .git(&tree.integration_path, &["status", "--porcelain"])
                    .await?
                    .is_empty()
            {
                report.state = "integrated".into();
            } else {
                report.state = "returned".into();
                report.detail =
                    "integration advancement interrupted; inspect owned worktree before retry"
                        .into();
            }
            self.save(&report).await?;
            return Ok(Some(report));
        }
        report.target_before = Some(current.clone());
        report.state = "rebasing".into();
        self.save(&report).await?;
        let args = vec!["rebase".into(), current.clone()];
        let (ok, _, error) = command(
            &tree.path,
            &args,
            &identity_env(identity)?,
            None,
            &self.workspace.redactor,
        )
        .await?;
        if !ok {
            report.state = "conflicted".into();
            report.detail = error;
            report.files = self
                .git(&tree.path, &["diff", "--name-only", "--diff-filter=U"])
                .await?
                .lines()
                .map(str::to_owned)
                .collect();
            let base = self
                .git(&tree.path, &["merge-base", &source, &current])
                .await?
                .trim()
                .to_owned();
            report.child_diff = self
                .workspace
                .redactor
                .redact_text(
                    &self
                        .git(
                            &tree.path,
                            &["diff", "--no-ext-diff", "--no-textconv", &base, &source],
                        )
                        .await?,
                )
                .chars()
                .take(8192)
                .collect();
            report.integration_diff = self
                .workspace
                .redactor
                .redact_text(
                    &self
                        .git(
                            &tree.path,
                            &["diff", "--no-ext-diff", "--no-textconv", &base, &current],
                        )
                        .await?,
                )
                .chars()
                .take(8192)
                .collect();
            self.save(&report).await?;
            return Ok(Some(report));
        }
        let rebased = self
            .git(&tree.path, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_owned();
        report.rebased_oid = Some(rebased.clone());
        report.files = self
            .git(&tree.path, &["diff", "--name-only", &current, &rebased])
            .await?
            .lines()
            .map(str::to_owned)
            .collect();
        report.state = "testing".into();
        self.save(&report).await?;
        let tests = checks.verify(&tree, &report.files).await;
        report.tests = Some(match tests {
            Ok(mut tests) => {
                tests.report = self.workspace.redactor.redact_text(&tests.report);
                tests
            }
            Err(error) => TestReport {
                passed: false,
                checks_run: 0,
                not_applicable_reason: None,
                report: self.workspace.redactor.redact_text(&error.to_string()),
            },
        });
        let tests = report.tests.as_ref().unwrap();
        if !tests.passed
            || (tests.checks_run == 0
                && tests
                    .not_applicable_reason
                    .as_ref()
                    .is_none_or(|r| r.trim().is_empty()))
        {
            report.state = "returned".into();
            report.detail = "impacted tests did not establish a passing integration".into();
            self.save(&report).await?;
            return Ok(Some(report));
        }
        if self.git(&tree.path, &["rev-parse", "HEAD"]).await?.trim() != rebased
            || !self
                .git(&tree.path, &["status", "--porcelain"])
                .await?
                .is_empty()
        {
            report.state = "returned".into();
            report.detail = "child changed during impacted checks".into();
            self.save(&report).await?;
            return Ok(Some(report));
        }
        report.state = "advancing".into();
        self.save(&report).await?;
        if !self
            .git(&tree.integration_path, &["status", "--porcelain"])
            .await?
            .is_empty()
        {
            report.state = "returned".into();
            report.detail = "integration worktree contains unexpected edits".into();
            self.save(&report).await?;
            return Ok(Some(report));
        }
        self.git(&tree.integration_path, &["merge", "--ff-only", &rebased])
            .await?;
        report.state = "integrated".into();
        self.save(&report).await?;
        Ok(Some(report))
    }
    pub async fn retry(&self, id: &str, identity: &GitIdentity) -> Result<()> {
        let id = id.to_owned();
        let root = self.workspace.root.to_string_lossy().into_owned();
        let (task, child, state) = self
            .workspace
            .db
            .read(ReadLane::Interactive, {
                let id = id.clone();
                move |c| {
                    Ok(c.query_row(
                        "SELECT i.task_id,i.child,i.state FROM workspace_integrations i JOIN workspace_tasks t ON t.id=i.task_id WHERE i.id=?1 AND t.root=?2",
                        params![id,root],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                            ))
                        },
                    )?)
                }
            })
            .await?;
        if !matches!(state.as_str(), "returned" | "conflicted") {
            return Err(Error::Invalid(
                "only returned or conflicted integrations can retry",
            ));
        }
        let tree = self.worktree(&task, &child).await?;
        if state == "conflicted" {
            let (ok, _, error) = command(
                &tree.path,
                &["rebase".into(), "--continue".into()],
                &identity_env(identity)?,
                None,
                &self.workspace.redactor,
            )
            .await?;
            if !ok {
                return Err(Error::Git(error));
            }
        }
        // Only a committed checkpoint integrates: uncommitted resolution edits
        // must be checkpointed before the retry, mirroring enqueue.
        if !self
            .git(&tree.path, &["status", "--porcelain"])
            .await?
            .is_empty()
        {
            return Err(Error::Invalid("checkpoint child changes before retry"));
        }
        let resolved = self
            .git(&tree.path, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_owned();
        // Adopt the post-resolution checkpoint atomically: compare-and-swap on
        // the terminal state, refresh the source the queue guard compares
        // against, and clear transient rebase state so process_next rebases,
        // checks, and advances the resolved checkpoint instead of returning it.
        let (retry_id, task_id, child_id, source) =
            (id.clone(), task.clone(), child.clone(), resolved.clone());
        let changed = self
            .workspace
            .db
            .write(Priority::Journal, move |c| {
                if c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM workspace_integrations WHERE task_id=?1 AND child=?2 AND source_oid=?3 AND id<>?4)",
                    params![task_id, child_id, source, retry_id],
                    |r| r.get::<_, bool>(0),
                )? {
                    return Err(excalibur_store::Error::Invalid(
                        "resolved checkpoint is already queued; process the existing entry",
                    ));
                }
                let text: String = c.query_row(
                    "SELECT report FROM workspace_integrations WHERE id=?1",
                    params![retry_id],
                    |r| r.get(0),
                )?;
                let mut report: IntegrationReport =
                    serde_json::from_str(&text).map_err(|_| {
                        excalibur_store::Error::Invalid("integration report is corrupt")
                    })?;
                report.state = "queued".into();
                report.detail = "re-queued at resolved checkpoint".into();
                report.target_before = None;
                report.rebased_oid = None;
                report.files.clear();
                report.child_diff.clear();
                report.integration_diff.clear();
                report.tests = None;
                let text = serde_json::to_string(&report).map_err(|_| {
                    excalibur_store::Error::Invalid("integration report is corrupt")
                })?;
                Ok(c.execute(
                    "UPDATE workspace_integrations SET state='queued',source_oid=?2,target_before=NULL,rebased_oid=NULL,report=?3 WHERE id=?1 AND state IN ('returned','conflicted')",
                    params![retry_id, source, text],
                )?)
            })
            .await?;
        if changed == 0 {
            return Err(Error::Invalid(
                "integration entry changed concurrently; list the queue before retrying",
            ));
        }
        Ok(())
    }
    async fn save(&self, report: &IntegrationReport) -> Result<()> {
        let (id, state, text, target, rebased) = (
            report.id.clone(),
            report.state.clone(),
            serde_json::to_string(report)?,
            report.target_before.clone(),
            report.rebased_oid.clone(),
        );
        self.workspace.db.write(Priority::Journal,move|c|{c.execute("UPDATE workspace_integrations SET state=?2,report=?3,target_before=?4,rebased_oid=?5 WHERE id=?1",params![id,state,text,target,rebased])?;Ok(())}).await?;
        Ok(())
    }
}
pub(crate) fn identity_env(identity: &GitIdentity) -> Result<Vec<(String, String)>> {
    if identity.name.trim().is_empty()
        || identity.email.trim().is_empty()
        || identity.name.contains(['\n', '\0'])
        || identity.email.contains(['\n', '\0'])
    {
        return Err(Error::Invalid("explicit host git identity required"));
    }
    Ok(vec![
        ("GIT_AUTHOR_NAME".into(), identity.name.clone()),
        ("GIT_AUTHOR_EMAIL".into(), identity.email.clone()),
        ("GIT_COMMITTER_NAME".into(), identity.name.clone()),
        ("GIT_COMMITTER_EMAIL".into(), identity.email.clone()),
    ])
}

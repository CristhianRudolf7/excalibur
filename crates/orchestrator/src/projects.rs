use crate::*;
use std::path::{Path, PathBuf};
impl Orchestrator {
    pub async fn register_project(&self, mut project: Project) -> Result<()> {
        if project.id.trim().is_empty()
            || project.name.trim().is_empty()
            || project.roots.is_empty()
            || !matches!(project.status.as_str(), "active" | "paused" | "archived")
        {
            return Err(invalid("project id, name, roots and valid status required"));
        }
        for root in &mut project.roots {
            *root = root
                .canonicalize()
                .map_err(|e| invalid(&format!("project root: {e}")))?;
            if !root.is_dir() {
                return Err(invalid("project roots must be directories"));
            }
        }
        project.roots.sort();
        project.roots.dedup();
        let id = project.id.clone();
        self.validate_durable(&serde_json::to_value(&project)?)?;
        let body = serde_json::to_string(&project)?;
        self.inner.db.write(Priority::Interactive,move|c|{c.execute("INSERT INTO projects(id,name,alias,description,roots,state,vcs,settings,scope_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(id) DO UPDATE SET name=excluded.name,alias=excluded.alias,description=excluded.description,roots=excluded.roots,state=excluded.state,vcs=excluded.vcs,settings=excluded.settings",rusqlite::params![project.id,project.name,project.aliases.join(","),project.description,serde_json::to_string(&project.roots).map_err(|_|excalibur_store::Error::Invalid("project roots invalid"))?,project.status,project.vcs,project.settings.to_string(),format!("project:{}",project.id)])?;c.execute("INSERT INTO orchestration_projects VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",rusqlite::params![id,body])?;Ok(())}).await?;
        Ok(())
    }
    pub async fn projects(&self) -> Result<Vec<Project>> {
        let rows = self
            .inner
            .db
            .read(ReadLane::Interactive, |c| {
                let mut q = c.prepare("SELECT body FROM orchestration_projects ORDER BY id")?;
                Ok(q.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        rows.into_iter()
            .map(|s| Ok(serde_json::from_str(&s)?))
            .collect()
    }
    pub async fn project(&self, id: &str) -> Result<Project> {
        self.projects()
            .await?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or(Error::Missing)
    }
    pub async fn resolve_path(
        &self,
        path: &Path,
        run_project: Option<&str>,
        mentioned: Option<&str>,
    ) -> Result<ProjectResolution> {
        let canonical_path = path
            .canonicalize()
            .map_err(|e| invalid(&format!("resolve path: {e}")))?;
        let mut candidates: Vec<(String, usize)> = self
            .projects()
            .await?
            .into_iter()
            .filter_map(|p| {
                p.roots
                    .iter()
                    .filter(|r| canonical_path.starts_with(r))
                    .map(|r| r.components().count())
                    .max()
                    .map(|depth| (p.id, depth))
            })
            .collect();
        candidates.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let preferred = run_project
            .filter(|id| candidates.iter().any(|(c, _)| c == id))
            .or_else(|| mentioned.filter(|id| candidates.iter().any(|(c, _)| c == id)));
        let chosen = preferred
            .map(str::to_owned)
            .or_else(|| candidates.first().map(|v| v.0.clone()));
        Ok(ProjectResolution {
            chosen,
            candidates: candidates.into_iter().map(|v| v.0).collect(),
            canonical_path,
        })
    }
    /// An empty set is never shorthand for unrestricted reads.
    pub async fn scoped_agents(
        &self,
        requester: &str,
        scopes: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<Agent>> {
        let a = self.agent(requester).await?;
        if scopes.is_empty() || !scopes.is_subset(&a.token.data_scopes) {
            return Err(invalid("explicit authorized scopes required"));
        }
        Ok(self
            .list_agents()
            .await?
            .into_iter()
            .filter(|a| scopes.contains(&a.scope))
            .collect())
    }
    pub async fn roots_for_scopes(
        &self,
        scopes: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<PathBuf>> {
        if scopes.is_empty() {
            return Err(invalid("explicit scopes required"));
        }
        Ok(self
            .projects()
            .await?
            .into_iter()
            .filter(|p| scopes.contains(&format!("project:{}", p.id)))
            .flat_map(|p| p.roots)
            .collect())
    }
}

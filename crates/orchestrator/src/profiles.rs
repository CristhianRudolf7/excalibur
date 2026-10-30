use crate::*;
/// Host-owned style/identity overlay. The base role remains the enforcement
/// profile; custom identities cannot supply permissions or additional tools.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CustomProfile {
    pub id: String,
    pub name: String,
    pub base_profile: String,
    pub identity: String,
    pub icon: Option<String>,
}
pub(crate) fn builtin_profile(profile: &str) -> bool {
    [
        "coordinator",
        "planner",
        "explorer",
        "implementer",
        "verifier",
        "reviewer",
        "researcher",
        "browser-operator",
        "integrator",
        "security-auditor",
        "app-operator",
    ]
    .contains(&profile)
}
impl Orchestrator {
    pub async fn register_profile(&self, profile: CustomProfile) -> Result<()> {
        if profile.id.is_empty()
            || profile.id.len() > 128
            || !profile
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || builtin_profile(&profile.id)
            || !builtin_profile(&profile.base_profile)
            || profile.name.trim().is_empty()
            || profile.name.len() > 256
            || profile.identity.len() > 8192
            || profile.icon.as_ref().is_some_and(|i| i.len() > 128)
        {
            return Err(invalid("invalid bounded custom profile"));
        }
        self.validate_durable(&serde_json::to_value(&profile)?)?;
        let body = serde_json::to_string(&profile)?;
        self.inner
            .db
            .write(Priority::Interactive, move |c| {
                // Registration is immutable: running agents keep the exact identity
                // that was included in their durable initial context.
                let old: Option<String> = c
                    .query_row(
                        "SELECT body FROM orchestration_profiles WHERE id=?1",
                        [&profile.id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if old.as_ref().is_some_and(|s| s != &body) {
                    return Err(excalibur_store::Error::Invalid(
                        "profile id is already bound; register a new id",
                    ));
                }
                c.execute(
                    "INSERT OR IGNORE INTO orchestration_profiles VALUES(?1,?2)",
                    rusqlite::params![profile.id, body],
                )?;
                c.execute(
                    "INSERT OR IGNORE INTO agent_profiles(id,name,role) VALUES(?1,?2,?3)",
                    rusqlite::params![profile.id, profile.name, profile.base_profile],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    pub async fn custom_profiles(&self) -> Result<Vec<CustomProfile>> {
        let rows = self
            .inner
            .db
            .read(ReadLane::Interactive, |c| {
                let mut q = c.prepare("SELECT body FROM orchestration_profiles ORDER BY id")?;
                Ok(q.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        rows.into_iter()
            .map(|s| Ok(serde_json::from_str(&s)?))
            .collect()
    }
    pub async fn custom_profile(&self, id: &str) -> Result<CustomProfile> {
        self.custom_profiles()
            .await?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or(Error::Missing)
    }
}

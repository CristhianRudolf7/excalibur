use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VersionPolicy {
    pub stable_major: Option<u32>,
    pub installed_release_unix_secs: Option<u64>,
    /// Optional public CfT metadata endpoint; offline by default.
    pub stable_metadata_url: Option<String>,
}
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum VersionState {
    Current,
    Unknown,
    Warning,
    BlockedPersistent,
}
#[derive(Clone, Debug, Serialize)]
pub struct Assessment {
    pub installed_major: u32,
    pub installed_release_unix_secs: Option<u64>,
    pub stable_major: Option<u32>,
    pub state: VersionState,
    pub warning: Option<String>,
}
impl VersionPolicy {
    pub fn assess(&self, installed: u32, now: u64) -> Assessment {
        let gap = self
            .stable_major
            .unwrap_or(installed)
            .saturating_sub(installed);
        let old = self
            .installed_release_unix_secs
            .is_some_and(|date| now.saturating_sub(date) > 42 * 86400);
        let state = if gap >= 3 || old {
            VersionState::BlockedPersistent
        } else if gap == 2 {
            VersionState::Warning
        } else if self.stable_major.is_none() {
            VersionState::Unknown
        } else {
            VersionState::Current
        };
        let warning = match state {
            VersionState::BlockedPersistent => {
                Some("persistent profiles blocked: Chrome is outdated".into())
            }
            VersionState::Warning => Some("Chrome is two major releases behind stable".into()),
            VersionState::Unknown => {
                Some("stable release unavailable; freshness is not verified".into())
            }
            _ => None,
        };
        Assessment {
            installed_major: installed,
            installed_release_unix_secs: self.installed_release_unix_secs,
            stable_major: self.stable_major,
            state,
            warning,
        }
    }
    pub async fn refresh(&mut self) -> Result<(), String> {
        let Some(url) = &self.stable_metadata_url else {
            return Ok(());
        };
        let url = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "stable metadata URL must be public and contain no credentials or query".into(),
            );
        }
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && url
                    .host_str()
                    .is_some_and(|h| h == "127.0.0.1" || h == "localhost"))
        {
            return Err("stable metadata requires HTTPS or a local fixture".into());
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let mut response = client
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if bytes.len() + chunk.len() > 65536 {
                return Err("stable metadata too large".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let major = value["channels"]["Stable"]["version"]
            .as_str()
            .and_then(|v| v.split('.').next()?.parse::<u32>().ok())
            .ok_or("invalid stable metadata")?;
        self.stable_major = Some(major);
        Ok(())
    }
}

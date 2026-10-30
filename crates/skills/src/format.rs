//! `SKILL.md` parsing and validation (Agent Skills format plus Excalibur metadata).
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value as Yaml;
use std::collections::BTreeMap;

pub const MAX_SKILL_FILE_BYTES: usize = 256 * 1024;
pub const PLATFORMS: [&str; 5] = ["linux", "macos", "windows", "android", "ios"];

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
}
/// Lifecycle state of an index entry. Only `Active` skills are listed,
/// suggested or loadable.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SkillState {
    Active,
    Draft,
    Quarantine,
    Obsolete,
    Archived,
}
impl SkillState {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "active" => Self::Active,
            "draft" => Self::Draft,
            "quarantine" => Self::Quarantine,
            "obsolete" => Self::Obsolete,
            "archived" => Self::Archived,
            _ => return None,
        })
    }
    /// The more restrictive of two states (a skill can never promote itself).
    pub fn restrict(self, other: Self) -> Self {
        self.max(other)
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Requirements {
    pub binaries: Vec<String>,
    pub env: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayText {
    pub name: String,
    pub description: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExcaliburMetadata {
    pub group: String,
    pub version: Option<String>,
    /// Empty means every platform.
    pub platforms: Vec<String>,
    pub requires: Requirements,
    pub risk: Option<Risk>,
    /// Deferred tools whose schemas are resolved when the skill is loaded.
    pub tools: Vec<String>,
    /// Localized interface names keyed by language (`en`, `es`).
    pub display: BTreeMap<String, DisplayText>,
    pub state: Option<SkillState>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    /// Space-delimited `allowed-tools`, enforced as an extra restriction while active.
    pub allowed_tools: Vec<String>,
    /// Raw `metadata` map (string values).
    pub metadata: BTreeMap<String, String>,
    pub excalibur: ExcaliburMetadata,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParsedSkill {
    pub manifest: SkillManifest,
    pub body: String,
    /// Markdown headings of the body, indexed for search.
    pub headings: Vec<String>,
}

/// Agent Skills naming rule: 1..64 of `a-z`, `0-9`, `-`, without leading,
/// trailing or consecutive hyphens.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}
fn valid_tool(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}
fn scalar(value: &Yaml) -> Option<String> {
    match value {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Number(n) => Some(n.to_string()),
        Yaml::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}
fn list(value: &str) -> Vec<String> {
    value
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}
fn chars(text: &str) -> usize {
    text.chars().count()
}

/// Split front matter from body. The file must start with a `---` line.
fn split(content: &str) -> Result<(&str, &str), String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or("SKILL.md must start with YAML front matter delimited by ---")?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Ok((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    if rest[offset..].trim_end() == "---" {
        return Ok((&rest[..offset], ""));
    }
    Err("front matter is not closed by ---".into())
}

/// Parse and validate one skill. All problems are reported, not only the first.
pub fn parse_skill(folder: &str, content: &str) -> Result<ParsedSkill, Vec<String>> {
    let mut errors = Vec::new();
    if content.len() > MAX_SKILL_FILE_BYTES {
        return Err(vec![format!(
            "SKILL.md exceeds {MAX_SKILL_FILE_BYTES} bytes"
        )]);
    }
    let (front, body) = split(content).map_err(|e| vec![e])?;
    let yaml: Yaml = serde_yaml_ng::from_str(front)
        .map_err(|e| vec![format!("invalid YAML front matter: {e}")])?;
    let Yaml::Mapping(map) = yaml else {
        return Err(vec!["front matter must be a mapping".into()]);
    };
    let mut fields = BTreeMap::new();
    for (key, value) in map {
        match key.as_str() {
            Some(
                k @ ("name" | "description" | "license" | "compatibility" | "metadata"
                | "allowed-tools"),
            ) => {
                fields.insert(k.to_string(), value);
            }
            Some(other) => errors.push(format!("unexpected front matter field {other}")),
            None => errors.push("front matter keys must be strings".into()),
        }
    }
    let text = |key: &str, errors: &mut Vec<String>| -> Option<String> {
        match fields.get(key) {
            None | Some(Yaml::Null) => None,
            Some(value) => match scalar(value) {
                Some(s) => Some(s),
                None => {
                    errors.push(format!("{key} must be a string"));
                    None
                }
            },
        }
    };
    let name = text("name", &mut errors).unwrap_or_default();
    if !valid_name(&name) {
        errors.push(format!(
            "invalid name {name:?}: use 1-64 lowercase letters, digits and single inner hyphens"
        ));
    } else if name != folder {
        errors.push(format!("name {name} does not match its folder {folder}"));
    }
    let description = text("description", &mut errors).unwrap_or_default();
    let description_chars = chars(description.trim());
    if !(1..=1024).contains(&description_chars) || chars(&description) > 1024 {
        errors.push(format!(
            "description must have 1-1024 characters (has {})",
            chars(&description)
        ));
    }
    let compatibility = text("compatibility", &mut errors);
    if let Some(c) = &compatibility
        && (c.trim().is_empty() || chars(c) > 500)
    {
        errors.push(format!(
            "compatibility must have 1-500 characters (has {})",
            chars(c)
        ));
    }
    let license = text("license", &mut errors);
    let allowed_tools = text("allowed-tools", &mut errors)
        .map(|s| s.split_whitespace().map(String::from).collect::<Vec<_>>())
        .unwrap_or_default();
    if allowed_tools.len() > 128 || allowed_tools.iter().any(|t| t.len() > 256) {
        errors.push("allowed-tools is too long".into());
    }
    let mut metadata = BTreeMap::new();
    match fields.get("metadata") {
        None | Some(Yaml::Null) => {}
        Some(Yaml::Mapping(m)) => {
            for (k, v) in m {
                let Some(k) = k.as_str() else {
                    errors.push("metadata keys must be strings".into());
                    continue;
                };
                let value = match v {
                    Yaml::Mapping(_) | Yaml::Sequence(_) => serde_json::to_string(
                        &serde_yaml_ng::from_value::<serde_json::Value>(v.clone())
                            .unwrap_or_default(),
                    )
                    .ok(),
                    other => scalar(other),
                };
                match value {
                    Some(value) if value.len() <= 8192 => {
                        metadata.insert(k.to_string(), value);
                    }
                    _ => errors.push(format!("metadata {k} must be a bounded string")),
                }
            }
        }
        Some(_) => errors.push("metadata must be a mapping".into()),
    }
    let excalibur = excalibur_metadata(&metadata, &mut errors);
    if body.trim().is_empty() {
        errors.push("SKILL.md body is empty".into());
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let headings = body
        .lines()
        .filter(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(ParsedSkill {
        manifest: SkillManifest {
            name,
            description,
            license,
            compatibility,
            allowed_tools,
            metadata,
            excalibur: excalibur.expect("validated metadata"),
        },
        body: body.to_string(),
        headings,
    })
}
fn excalibur_metadata(
    metadata: &BTreeMap<String, String>,
    errors: &mut Vec<String>,
) -> Option<ExcaliburMetadata> {
    let get = |k: &str| metadata.get(&format!("excalibur.{k}"));
    let before = errors.len();
    let group = get("group").cloned().unwrap_or_default();
    if !valid_name(&group) {
        errors.push("metadata excalibur.group must name exactly one valid group".into());
    }
    let version = get("version").cloned();
    if version
        .as_ref()
        .is_some_and(|v| v.trim().is_empty() || v.len() > 64)
    {
        errors.push("excalibur.version must have 1-64 characters".into());
    }
    let platforms = get("platforms").map(|p| list(p)).unwrap_or_default();
    if let Some(bad) = platforms.iter().find(|p| !PLATFORMS.contains(&p.as_str())) {
        errors.push(format!("unknown platform {bad}"));
    }
    let mut requires = Requirements::default();
    for item in get("requires").map(|r| list(r)).unwrap_or_default() {
        match item.split_once(':') {
            Some(("bin", b)) if !b.is_empty() && !b.contains('/') => {
                requires.binaries.push(b.into())
            }
            Some(("env", e))
                if !e.is_empty() && e.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') =>
            {
                requires.env.push(e.into())
            }
            _ => errors.push(format!(
                "excalibur.requires item {item} must be bin:<name> or env:<NAME>"
            )),
        }
    }
    let risk = match get("risk").map(String::as_str) {
        None => None,
        Some("low") => Some(Risk::Low),
        Some("medium") => Some(Risk::Medium),
        Some("high") => Some(Risk::High),
        Some(other) => {
            errors.push(format!("unknown excalibur.risk {other}"));
            None
        }
    };
    let tools = get("tools").map(|t| list(t)).unwrap_or_default();
    if let Some(bad) = tools.iter().find(|t| !valid_tool(t)) {
        errors.push(format!("excalibur.tools entry {bad} is not a tool name"));
    }
    let mut display = BTreeMap::new();
    if let Some(raw) = get("display") {
        match serde_json::from_str::<BTreeMap<String, DisplayText>>(raw) {
            Ok(map)
                if map.keys().all(|k| matches!(k.as_str(), "en" | "es"))
                    && map.values().all(|d| {
                        (1..=128).contains(&chars(&d.name))
                            && (1..=256).contains(&chars(&d.description))
                    }) =>
            {
                display = map
            }
            _ => errors.push(
                "excalibur.display must map en/es to {name, description} (1-128/1-256 characters)"
                    .into(),
            ),
        }
    }
    let state = match get("state") {
        None => None,
        Some(s) => match SkillState::parse(s) {
            Some(state) => Some(state),
            None => {
                errors.push(format!("unknown excalibur.state {s}"));
                None
            }
        },
    };
    (errors.len() == before).then_some(ExcaliburMetadata {
        group,
        version,
        platforms,
        requires,
        risk,
        tools,
        display,
        state,
    })
}

use crate::{Error, Result};
use aho_corasick::AhoCorasick;
use chrono::{Datelike, Duration, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExplicitCommand {
    pub name: String,
    pub target: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LexiconEntry {
    pub alias: String,
    pub id: String,
    pub kind: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mention {
    pub literal: String,
    pub candidates: Vec<String>,
    pub kind: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Preanalysis {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<Classification>,
    pub command: Option<ExplicitCommand>,
    pub identifiers: Vec<String>,
    pub urls: Vec<String>,
    pub paths: Vec<String>,
    pub mentions: Vec<Mention>,
    pub temporal: Vec<TemporalReference>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TemporalReference {
    pub literal: String,
    pub timestamp: i64,
    pub timezone: String,
}
#[derive(Default)]
pub struct Lexicon {
    matcher: Option<AhoCorasick>,
    entries: Vec<(String, Vec<LexiconEntry>)>,
    pub(crate) fingerprint: String,
}
impl Lexicon {
    pub(crate) fn fingerprint(entries: &[LexiconEntry]) -> String {
        use sha2::Digest;
        format!(
            "{:x}",
            sha2::Sha256::digest(serde_json::to_vec(entries).unwrap_or_default())
        )
    }

    pub fn new(entries: Vec<LexiconEntry>) -> Result<Self> {
        if entries.len() > 100_000 {
            return Err(Error::Invalid("lexicon too large".into()));
        }
        let fingerprint = Self::fingerprint(&entries);
        let mut grouped: BTreeMap<String, Vec<LexiconEntry>> = BTreeMap::new();
        for entry in entries {
            if entry.alias.is_empty() || entry.alias.len() > 256 || entry.id.is_empty() {
                return Err(Error::Invalid("invalid lexicon entry".into()));
            }
            grouped
                .entry(entry.alias.to_lowercase())
                .or_default()
                .push(entry);
        }
        let entries = grouped.into_iter().collect::<Vec<_>>();
        let matcher = if entries.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::new(entries.iter().map(|(name, _)| name))
                    .map_err(|e| Error::Invalid(e.to_string()))?,
            )
        };
        Ok(Self {
            matcher,
            entries,
            fingerprint,
        })
    }
    pub fn analyze(&self, text: &str, timezone: &str) -> Result<Preanalysis> {
        self.analyze_at(text, timezone, Utc::now().timestamp())
    }
    pub fn analyze_at(&self, text: &str, timezone: &str, now: i64) -> Result<Preanalysis> {
        let timezone: chrono_tz::Tz = timezone
            .parse()
            .map_err(|_| Error::Invalid("invalid timezone".into()))?;
        let mut result = Preanalysis::default();
        let trimmed = text.trim();
        if let Some(command) = trimmed.strip_prefix('/') {
            let (name, target) = command
                .split_once(char::is_whitespace)
                .map(|(a, b)| (a, b.trim()))
                .unwrap_or((command, ""));
            let name = if name == "proyecto" { "project" } else { name };
            if matches!(
                name,
                "status" | "pause" | "resume" | "cancel" | "stop" | "approve" | "deny" | "project"
            ) {
                let target = split_remainder(target)
                    .map(|(head, _)| head)
                    .unwrap_or(target);
                result.command = Some(ExplicitCommand {
                    name: name.into(),
                    target: (!target.is_empty()).then(|| target.into()),
                });
            }
        }
        for word in text.split_whitespace() {
            let value = word.trim_matches(|c: char| {
                matches!(c, ',' | ';' | '(' | ')' | '!' | '?' | '"' | '\'')
            });
            if value.starts_with("https://") || value.starts_with("http://") {
                result.urls.push(value.into());
            } else if value.starts_with('/')
                || value.starts_with("./")
                || value.contains('/') && !value.contains("://")
            {
                result.paths.push(value.into());
            }
            if (value.len() == 26
                && format!("agt_{value}")
                    .parse::<excalibur_types::AgentId>()
                    .is_ok())
                || value.starts_with("run_")
                || value.starts_with("agt_")
                || value.starts_with("tsk_")
                || value.starts_with("prj_")
                || (value.len() > 1
                    && matches!(value.as_bytes()[0], b'T' | b'R')
                    && value[1..].chars().all(|c| c.is_ascii_digit()))
            {
                result.identifiers.push(value.into());
            }
        }
        if let Some(matcher) = &self.matcher {
            let lowered = text.to_lowercase();
            for found in matcher.find_overlapping_iter(&lowered) {
                let before = lowered[..found.start()].chars().next_back();
                let after = lowered[found.end()..].chars().next();
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_')
                    || after.is_some_and(|c| c.is_alphanumeric() || c == '_')
                {
                    continue;
                }
                let (literal, candidates) = &self.entries[found.pattern().as_usize()];
                let kind = candidates
                    .first()
                    .map(|e| e.kind.clone())
                    .unwrap_or_default();
                let mut ids = candidates.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
                ids.sort();
                ids.dedup();
                result.mentions.push(Mention {
                    literal: literal.clone(),
                    candidates: ids,
                    kind,
                });
            }
        }
        let local = timezone
            .timestamp_opt(now, 0)
            .single()
            .ok_or_else(|| Error::Invalid("invalid current time".into()))?;
        let lower = text.to_lowercase();
        let words = lower.split_whitespace().collect::<Vec<_>>();
        for (index, word) in words.iter().enumerate() {
            let days = match *word {
                "mañana" | "tomorrow" => Some(1),
                "hoy" | "today" => Some(0),
                day => weekday(day).map(|weekday| {
                    let difference =
                        (weekday as i64 - local.weekday().num_days_from_monday() as i64 + 7) % 7;
                    if difference == 0 { 7 } else { difference }
                }),
            };
            if let Some(days) = days {
                let mut clock = None;
                for token in words.iter().skip(index + 1).take(5) {
                    let token = token.trim_matches(|c: char| {
                        matches!(c, ',' | ';' | '.' | '!' | '?' | '(' | ')')
                    });
                    if !token.chars().all(|c| c.is_ascii_digit() || c == ':') {
                        continue;
                    }
                    let parts = token
                        .trim_matches(|c: char| !c.is_ascii_digit() && c != ':')
                        .split(':')
                        .collect::<Vec<_>>();
                    if let Ok(hour) = parts[0].parse::<u32>()
                        && let Ok(minute) = parts.get(1).unwrap_or(&"0").parse::<u32>()
                        && hour < 24
                        && minute < 60
                        && parts.len() <= 2
                    {
                        clock = Some((hour, minute));
                        break;
                    }
                }
                if let Some((hour, minute)) = clock {
                    let date = local.date_naive() + Duration::days(days);
                    let time = NaiveTime::from_hms_opt(hour, minute, 0).unwrap();
                    // Ambiguous or nonexistent wall time is left unresolved, never guessed.
                    if let Some(timestamp) =
                        timezone.from_local_datetime(&date.and_time(time)).single()
                    {
                        result.temporal.push(TemporalReference {
                            literal: words[index..words.len().min(index + 6)].join(" "),
                            timestamp: timestamp.timestamp(),
                            timezone: timezone.to_string(),
                        });
                    }
                }
            }
        }
        Ok(result)
    }
}
fn weekday(value: &str) -> Option<u32> {
    match value.trim_matches(|c: char| !c.is_alphabetic()) {
        "lunes" | "monday" => Some(0),
        "martes" | "tuesday" => Some(1),
        "miércoles" | "wednesday" => Some(2),
        "jueves" | "thursday" => Some(3),
        "viernes" | "friday" => Some(4),
        "sábado" | "saturday" => Some(5),
        "domingo" | "sunday" => Some(6),
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestCard {
    pub original: Vec<(String, String)>,
    pub interpretation: String,
    pub requirements: Vec<String>,
    pub criteria: Vec<String>,
    pub assumptions: Vec<String>,
    pub open_questions: Vec<String>,
    pub pinned_instructions: Vec<String>,
}
impl RequestCard {
    /// The small-task contract retains the entire source; detailed extraction is
    /// supplied by the task planner, never represented as inferred facts here.
    pub fn small_task(message_id: String, text: String) -> Self {
        Self {
            original: vec![(message_id, text.clone())],
            interpretation: text,
            requirements: vec!["Perform the requested task".into()],
            criteria: vec!["Verify the requested observable result".into()],
            assumptions: Vec::new(),
            open_questions: Vec::new(),
            pinned_instructions: Vec::new(),
        }
    }
}

/// Optional bounded decision service. It classifies the existing intake data;
/// implementations must not add a conversational-model call before the master.
pub trait IntakeDecider: Send + Sync {
    fn classify<'a>(
        &'a self,
        text: &'a str,
        analysis: &'a Preanalysis,
    ) -> excalibur_types::runtime::BoxFuture<'a, std::result::Result<Classification, String>>;
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntakeKind {
    Conversation,
    Status,
    Pause,
    Resume,
    Cancel,
    Task,
    Correction,
    Reminder,
    Memory,
    Configuration,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Classification {
    pub kind: IntakeKind,
    pub confidence: f64,
    pub target: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub literal_instructions: Vec<String>,
    #[serde(default)]
    pub requires_clarification: bool,
}
impl Classification {
    pub fn valid_for(&self, text: &str) -> bool {
        self.confidence.is_finite()
            && (0.0..=1.0).contains(&self.confidence)
            && self.scopes.len() <= 16
            && self.literal_instructions.len() <= 32
            && self.literal_instructions.iter().all(|literal| {
                !literal.is_empty() && literal.len() <= 8192 && text.contains(literal)
            })
            && self.target.as_ref().is_none_or(|id| id.len() <= 256)
    }
}

pub(crate) fn split_remainder(text: &str) -> Option<(&str, &str)> {
    for separator in [" and ", " y "] {
        if let Some((head, tail)) = text.split_once(separator) {
            let verb = tail.split_whitespace().next().unwrap_or("").to_lowercase();
            if [
                "change",
                "cambia",
                "cambiar",
                "update",
                "actualiza",
                "add",
                "añade",
                "write",
                "escribe",
                "make",
                "haz",
                "create",
                "crea",
                "fix",
                "corrige",
                "require",
                "exige",
            ]
            .contains(&verb.as_str())
            {
                return Some((head, tail));
            }
        }
    }
    None
}

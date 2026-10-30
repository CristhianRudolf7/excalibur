//! A run freezes its catalog; loads append bodies rather than mutate the prefix.
use crate::{
    groups::{CatalogScope, relevant_groups},
    index::{IndexSnapshot, SkillEntry, SkillIndex, search_snapshot},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub struct SkillSession {
    snapshot: Arc<IndexSnapshot>,
    loaded: BTreeMap<String, SkillEntry>,
    pub catalog: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedSkill {
    pub name: String,
    pub body: String,
    pub tools: Vec<String>,
    pub resources: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDelta {
    pub name: String,
    pub previous_hash: String,
    pub current_hash: Option<String>,
}
impl SkillSession {
    /// The host supplies the tokenizer used by the selected provider. Every
    /// candidate is measured including headings and separator overhead.
    pub fn new(
        index: &SkillIndex,
        scope: &CatalogScope,
        window: usize,
        tokens: impl Fn(&str) -> usize,
    ) -> Self {
        let snapshot = index.snapshot();
        let relevant = relevant_groups(scope);
        let budget = (window / 50).min(4000);
        let mut catalog = String::new();
        for group in index.groups().all() {
            let entries = snapshot
                .usable()
                .filter(|e| e.group == group.id)
                .collect::<Vec<_>>();
            if entries.is_empty() {
                continue;
            }
            let header = format!(
                "{} — {} skills; use search_skills\n",
                group.id,
                entries.len()
            );
            let mut block = header.clone();
            if relevant.contains(&group.id) {
                for entry in entries {
                    let description = entry.description.chars().take(120).collect::<String>();
                    block.push_str(&format!("- {}: {}\n", entry.name, description));
                }
            }
            let candidate = format!("{catalog}{block}");
            if tokens(&candidate) <= budget {
                catalog = candidate;
            } else {
                let candidate = format!("{catalog}{header}");
                if tokens(&candidate) <= budget {
                    catalog = candidate;
                }
            }
        }
        Self {
            snapshot,
            loaded: BTreeMap::new(),
            catalog,
        }
    }
    pub fn revision(&self) -> u64 {
        self.snapshot.revision
    }
    pub fn suggestions(
        &self,
        index: &SkillIndex,
        query: &str,
        model: &str,
    ) -> Vec<crate::index::SearchHit> {
        let disabled = index.disabled_groups(model, 20, 0.05);
        let mut hits = search_snapshot(&self.snapshot, query, None, 20, None);
        hits.retain(|hit| !disabled.contains(&hit.group));
        hits.truncate(3);
        for hit in &hits {
            index.record_suggestion(&hit.name);
        }
        hits
    }
    /// Schema resolution must succeed before activation. The host resolves the
    /// returned deferred tool names through its existing tool catalog.
    pub fn load(
        &mut self,
        index: &SkillIndex,
        name: &str,
        resolve: impl FnOnce(&[String]) -> Result<(), String>,
    ) -> Result<LoadedSkill, String> {
        let entry = self
            .snapshot
            .entries
            .get(name)
            .filter(|e| e.usable())
            .ok_or_else(|| "skill is unavailable in this session".to_string())?;
        let current = index
            .get(name)
            .filter(|e| e.usable())
            .ok_or_else(|| "skill is no longer available".to_string())?;
        if entry.hash != current.hash {
            return Err(
                "skill changed; acknowledge its delta in a new session before loading".into(),
            );
        }
        resolve(&entry.tools)?;
        self.loaded.insert(name.into(), entry.clone());
        index.record_load(name);
        Ok(LoadedSkill {
            name: name.into(),
            body: entry.body.clone(),
            tools: entry.tools.clone(),
            resources: entry.resources.clone(),
        })
    }
    /// Each active skill restricts the current authority by intersection. An
    /// empty allowed-tools field adds no restriction; it never grants tools.
    pub fn allowed_tools(&self, authority: &BTreeSet<String>) -> BTreeSet<String> {
        self.loaded
            .values()
            .fold(authority.clone(), |mut tools, skill| {
                if !skill.allowed_tools.is_empty() {
                    tools.retain(|t| skill.allowed_tools.contains(t));
                }
                tools
            })
    }
    pub fn deltas(&self, index: &SkillIndex) -> Vec<SkillDelta> {
        self.loaded
            .values()
            .filter_map(|old| {
                let current = index.get(&old.name).filter(|e| e.usable());
                let hash = current.map(|e| e.hash);
                (hash.as_ref() != Some(&old.hash)).then(|| SkillDelta {
                    name: old.name.clone(),
                    previous_hash: old.hash.clone(),
                    current_hash: hash,
                })
            })
            .collect()
    }
}

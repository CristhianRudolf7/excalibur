//! Entities, typed relations with per-user adaptive types, and deterministic mention links
//! (architecture 10.4). Relations stay inside their scope; traversal never crosses scopes.
use crate::{
    MemoryGrant, MemoryStore, Result, invalid, new_id, placeholders,
    text::{contains_words, normalize},
};
use excalibur_store::{
    Priority,
    rusqlite::{self, OptionalExtension},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// General seed relation types created per principal on first use.
pub const SEED_RELATION_TYPES: &[(&str, &str)] = &[
    ("works_at", "Person works at an organization"),
    ("participates_in", "Participates in a project or event"),
    ("prefers", "Holds a preference for something"),
    ("depends_on", "Depends on another entity"),
    ("decided", "Made a decision"),
    ("part_of", "Is part of a larger entity"),
    ("cites", "Cites a document or source"),
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub scope: String,
    pub kind: String,
    pub canonical_name: String,
    pub aliases: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationType {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub origin: String,
    pub state: String,
    pub usage_count: i64,
    pub merged_into: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelateOutcome {
    Linked {
        edge_id: String,
        relation: String,
    },
    /// No usable type exists; the need is recorded for consolidation to propose a type.
    NeedRecorded {
        name: String,
    },
}

pub(crate) fn ensure_seeds(c: &rusqlite::Connection, principal: &str) -> rusqlite::Result<()> {
    for (name, description) in SEED_RELATION_TYPES {
        c.execute(
            "INSERT OR IGNORE INTO relation_types(id,name,description,origin,state,principal_id) VALUES(?1,?2,?3,'seed','active',?4)",
            rusqlite::params![new_id("rel"), name, description, principal],
        )?;
    }
    Ok(())
}

fn entity_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entity> {
    let aliases: String = r.get(4)?;
    Ok(Entity {
        id: r.get(0)?,
        scope: r.get(1)?,
        kind: r.get(2)?,
        canonical_name: r.get(3)?,
        aliases: serde_json::from_str(&aliases).unwrap_or_default(),
    })
}

pub(crate) fn scope_entities(
    c: &rusqlite::Connection,
    principal: &str,
    scopes: &[String],
) -> rusqlite::Result<Vec<Entity>> {
    let sql = format!(
        "SELECT id,scope_id,COALESCE(kind,'concept'),canonical_name,aliases FROM entities WHERE principal_id=?1 AND scope_id IN ({}) ORDER BY id",
        placeholders(2, scopes.len())
    );
    let mut s = c.prepare(&sql)?;
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&principal];
    for scope in scopes {
        params.push(scope);
    }
    s.query_map(params.as_slice(), entity_from_row)?.collect()
}

/// Resolve an entity id or name inside one scope, creating a concept entity if needed.
pub(crate) fn resolve_entity(
    c: &rusqlite::Connection,
    principal: &str,
    scope: &str,
    reference: &str,
    now: i64,
) -> rusqlite::Result<String> {
    let exists: Option<String> = c
        .query_row(
            "SELECT id FROM entities WHERE id=?1 AND principal_id=?2 AND scope_id=?3",
            rusqlite::params![reference, principal, scope],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = exists {
        return Ok(id);
    }
    let wanted = normalize(reference);
    for entity in scope_entities(c, principal, &[scope.to_string()])? {
        if normalize(&entity.canonical_name) == wanted
            || entity.aliases.iter().any(|a| normalize(a) == wanted)
        {
            return Ok(entity.id);
        }
    }
    let id = new_id("ent");
    c.execute(
        "INSERT INTO entities(id,kind,canonical_name,aliases,principal_id,scope_id,created_at,updated_at) VALUES(?1,'concept',?2,'[]',?3,?4,?5,?5)",
        rusqlite::params![id, reference.trim(), principal, scope, now],
    )?;
    Ok(id)
}

/// Known entities mentioned on word boundaries (no model involved).
pub(crate) fn mentioned_entities(entities: &[Entity], text: &str) -> Vec<String> {
    let haystack = normalize(text);
    entities
        .iter()
        .filter(|e| {
            std::iter::once(&e.canonical_name)
                .chain(e.aliases.iter())
                .any(|name| name.chars().count() >= 2 && contains_words(&haystack, name))
        })
        .map(|e| e.id.clone())
        .collect()
}

pub(crate) fn resolve_relation(
    c: &rusqlite::Connection,
    principal: &str,
    name: &str,
) -> rusqlite::Result<Option<(String, String)>> {
    ensure_seeds(c, principal)?;
    let mut current = name.to_string();
    for _ in 0..8 {
        let row: Option<(String, String, Option<String>)> = c
            .query_row(
                "SELECT id,state,merged_into FROM relation_types WHERE principal_id=?1 AND name=?2",
                rusqlite::params![principal, current],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match row {
            Some((_, state, Some(target))) if state == "merged" => current = target,
            Some((id, state, _)) if state == "active" || state == "proposed" => {
                return Ok(Some((id, current)));
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

impl MemoryStore {
    /// Create or update an entity with aliases in one granted scope.
    pub async fn upsert_entity(
        &self,
        grant: &MemoryGrant,
        scope: &str,
        kind: &str,
        canonical_name: &str,
        aliases: &[String],
        now: i64,
    ) -> Result<Entity> {
        grant.require(scope)?;
        if canonical_name.trim().is_empty() || canonical_name.len() > 512 {
            return Err(invalid("entity name required"));
        }
        self.ensure_principal(&grant.principal).await?;
        let (principal, scope, kind, name, aliases) = (
            grant.principal.clone(),
            scope.to_string(),
            kind.to_string(),
            canonical_name.trim().to_string(),
            aliases.to_vec(),
        );
        Ok(self
            .db
            .write(Priority::Normal, move |c| {
                let id = resolve_entity(c, &principal, &scope, &name, now)?;
                let current: String = c.query_row("SELECT aliases FROM entities WHERE id=?1", [&id], |r| r.get(0))?;
                let mut set: BTreeSet<String> = serde_json::from_str(&current).unwrap_or_default();
                set.extend(aliases.into_iter().filter(|a| !a.trim().is_empty()));
                c.execute(
                    "UPDATE entities SET kind=?2,aliases=?3,updated_at=?4 WHERE id=?1",
                    rusqlite::params![id, kind, serde_json::to_string(&set).unwrap(), now],
                )?;
                Ok(c.query_row(
                    "SELECT id,scope_id,COALESCE(kind,'concept'),canonical_name,aliases FROM entities WHERE id=?1",
                    [&id],
                    entity_from_row,
                )?)
            })
            .await?)
    }
    /// Link two entities of one scope with a typed, bitemporal, provenance-carrying edge.
    #[allow(clippy::too_many_arguments)]
    pub async fn relate(
        &self,
        grant: &MemoryGrant,
        scope: &str,
        subject: &str,
        relation: &str,
        object: &str,
        valid_from: Option<i64>,
        provenance: &str,
        now: i64,
    ) -> Result<RelateOutcome> {
        grant.require(scope)?;
        self.ensure_principal(&grant.principal).await?;
        let relation = relation
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '-'], "_");
        if relation.is_empty() || relation.len() > 64 {
            return Err(invalid("relation name required"));
        }
        let (principal, scope, subject, object, provenance) = (
            grant.principal.clone(),
            scope.to_string(),
            subject.to_string(),
            object.to_string(),
            provenance.to_string(),
        );
        Ok(self
            .db
            .write(Priority::Normal, move |c| {
                let Some((type_id, name)) = resolve_relation(c, &principal, &relation)? else {
                    c.execute(
                        "INSERT INTO relation_needs(principal_id,name,memory_id,observed_at) VALUES(?1,?2,?3,?4)",
                        rusqlite::params![principal, relation, provenance, now],
                    )?;
                    return Ok(RelateOutcome::NeedRecorded { name: relation });
                };
                let source = resolve_entity(c, &principal, &scope, &subject, now)?;
                let destination = resolve_entity(c, &principal, &scope, &object, now)?;
                let edge = new_id("edge");
                c.execute(
                    "INSERT INTO edges(id,layer,source_type,source_id,relation_type,destination_type,destination_id,valid_from,provenance,principal_id,scope_id,created_at,updated_at) VALUES(?1,'memory','entity',?2,?3,'entity',?4,?5,?6,?7,?8,?9,?9)",
                    rusqlite::params![edge, source, name, destination, valid_from, provenance, principal, scope, now],
                )?;
                c.execute("UPDATE relation_types SET usage_count=usage_count+1,updated_at=?2 WHERE id=?1", rusqlite::params![type_id, now])?;
                Ok(RelateOutcome::Linked { edge_id: edge, relation: name })
            })
            .await?)
    }
    pub async fn relation_types(&self, grant: &MemoryGrant) -> Result<Vec<RelationType>> {
        grant.validate()?;
        self.ensure_principal(&grant.principal).await?;
        let principal = grant.principal.clone();
        Ok(self
            .db
            .write(Priority::Normal, move |c| {
                ensure_seeds(c, &principal)?;
                let mut s = c.prepare("SELECT id,name,description,COALESCE(origin,'seed'),state,usage_count,merged_into FROM relation_types WHERE principal_id=?1 ORDER BY name")?;
                let rows = s
                    .query_map([&principal], |r| {
                        Ok(RelationType {
                            id: r.get(0)?,
                            name: r.get(1)?,
                            description: r.get(2)?,
                            origin: r.get(3)?,
                            state: r.get(4)?,
                            usage_count: r.get(5)?,
                            merged_into: r.get(6)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }
    /// User confirmation activates a proposed type.
    pub async fn confirm_relation_type(
        &self,
        grant: &MemoryGrant,
        name: &str,
        now: i64,
    ) -> Result<bool> {
        grant.validate()?;
        let (principal, name) = (grant.principal.clone(), name.to_string());
        Ok(self
            .db
            .write(Priority::Normal, move |c| {
                Ok(c.execute("UPDATE relation_types SET state='active',updated_at=?3 WHERE principal_id=?1 AND name=?2 AND state='proposed'", rusqlite::params![principal, name, now])? == 1)
            })
            .await?)
    }
    /// Merge one type into another; existing edges keep their history, new uses resolve to the target.
    pub async fn merge_relation_type(
        &self,
        grant: &MemoryGrant,
        from: &str,
        into: &str,
        now: i64,
    ) -> Result<()> {
        grant.validate()?;
        let (principal, from, into) = (grant.principal.clone(), from.to_string(), into.to_string());
        let ok = self
            .db
            .write(Priority::Normal, move |c| {
                if resolve_relation(c, &principal, &into)?.is_none() || from == into {
                    return Ok(false);
                }
                Ok(c.execute("UPDATE relation_types SET state='merged',merged_into=?3,updated_at=?4 WHERE principal_id=?1 AND name=?2", rusqlite::params![principal, from, into, now])? == 1)
            })
            .await?;
        if ok {
            Ok(())
        } else {
            Err(invalid("cannot merge relation types"))
        }
    }
    /// Propose learned types (enough recent unmatched facts) and activate consistently used ones.
    pub async fn consolidate_relation_types(
        &self,
        principal: &str,
        now: i64,
    ) -> Result<Vec<String>> {
        let (principal, min, window, uses) = (
            principal.to_string(),
            self.config.relation_proposal_min_facts as i64,
            self.config.relation_proposal_window_seconds,
            self.config.relation_activation_uses as i64,
        );
        Ok(self
            .db
            .write(Priority::Background, move |c| {
                ensure_seeds(c, &principal)?;
                let mut s = c.prepare("SELECT name,count(*) FROM relation_needs WHERE principal_id=?1 AND observed_at>=?2 GROUP BY name HAVING count(*)>=?3 ORDER BY name")?;
                let needs = s
                    .query_map(rusqlite::params![principal, now - window, min], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut proposed = Vec::new();
                for name in needs {
                    if c.execute("INSERT OR IGNORE INTO relation_types(id,name,description,origin,state,principal_id,proposed_at) VALUES(?1,?2,'learned from repeated facts','learned','proposed',?3,?4)", rusqlite::params![new_id("rel"), name, principal, now])? == 1 {
                        proposed.push(name);
                    }
                }
                c.execute("UPDATE relation_types SET state='active',updated_at=?3 WHERE principal_id=?1 AND state='proposed' AND usage_count>=?2", rusqlite::params![principal, uses, now])?;
                Ok(proposed)
            })
            .await?)
    }
}

/// Entities reachable in one or two hops through in-scope memory edges, with hop distance.
pub(crate) fn neighbors(
    c: &rusqlite::Connection,
    principal: &str,
    scopes: &[String],
    seeds: &[String],
    hops: usize,
) -> rusqlite::Result<BTreeMap<String, usize>> {
    let mut found: BTreeMap<String, usize> = seeds.iter().map(|s| (s.clone(), 0)).collect();
    let mut frontier: Vec<String> = seeds.to_vec();
    let sql = format!(
        "SELECT source_id,destination_id FROM edges WHERE layer='memory' AND principal_id=?1 AND (source_id=?2 OR destination_id=?2) AND (valid_to IS NULL) AND scope_id IN ({})",
        placeholders(3, scopes.len())
    );
    let mut s = c.prepare(&sql)?;
    for hop in 1..=hops {
        let mut next = Vec::new();
        for node in &frontier {
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![&principal, node];
            for scope in scopes {
                params.push(scope);
            }
            let rows = s
                .query_map(params.as_slice(), |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (a, b) in rows {
                let other = if &a == node { b } else { a };
                if !found.contains_key(&other) {
                    found.insert(other.clone(), hop);
                    next.push(other);
                }
            }
        }
        frontier = next;
    }
    Ok(found)
}

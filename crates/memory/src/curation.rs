//! Explicit curation actions of `memory_write` besides `add`: supersede, retire, resolve a
//! dispute, merge entities and define a relation type. None of them destroys a record: every
//! change is a status transition or an added record, and each one is logged.
use crate::{
    AdmissionResult, Error, MemoryGrant, MemoryInput, MemoryStatus, MemoryStore, Result, Trust,
    graph::ensure_seeds, invalid, load_memory, new_id, transition,
};
use excalibur_store::{
    Priority,
    rusqlite::{self, OptionalExtension},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurationResult {
    pub action: String,
    pub subject: String,
    pub changed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<AdmissionResult>,
}

#[allow(clippy::too_many_arguments)]
fn log(
    c: &rusqlite::Connection,
    principal: &str,
    scope: &str,
    action: &str,
    subject: &str,
    detail: Value,
    actor: Option<&str>,
    now: i64,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO memory_curations(principal_id,scope_id,action,subject,detail,actor,at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![principal, scope, action, subject, detail.to_string(), actor, now],
    )?;
    Ok(())
}

impl MemoryStore {
    /// Replace a current record with a new statement. The old record keeps its history and
    /// gets `valid_to` (the new `valid_from`, or now) and `superseded_by`.
    pub async fn supersede(
        &self,
        grant: &MemoryGrant,
        old_id: &str,
        input: MemoryInput,
        now: i64,
    ) -> Result<CurationResult> {
        let old = self
            .memory(grant, old_id)
            .await?
            .ok_or_else(|| Error::NotFound(old_id.into()))?;
        if !matches!(
            old.status,
            MemoryStatus::Active | MemoryStatus::Disputed | MemoryStatus::Candidate
        ) {
            return Err(invalid("only current records can be superseded"));
        }
        if input.scope != old.scope {
            return Err(invalid(
                "a replacement must stay in the scope of the record",
            ));
        }
        if old.trust > grant.max_trust {
            return Err(Error::Scope(
                "this caller cannot supersede a record asserted with higher trust".into(),
            ));
        }
        let (new_id_, _, _) = self.insert_candidate(grant, input, false, now).await?;
        let (principal, scope, old2, new2, run) = (
            grant.principal.clone(),
            old.scope.clone(),
            old.clone(),
            new_id_.clone(),
            grant.run_id.clone(),
        );
        self.db
            .write(Priority::Normal, move |c| {
                let fresh = load_memory(c, &old2.id)?
                    .ok_or(excalibur_store::Error::Invalid("record disappeared"))?;
                if fresh.status != old2.status {
                    return Err(excalibur_store::Error::Invalid("record changed concurrently"));
                }
                let new = load_memory(c, &new2)?
                    .ok_or(excalibur_store::Error::Invalid("replacement disappeared"))?;
                transition(c, &new2, Some(MemoryStatus::Candidate), MemoryStatus::Active, &format!("explicit replacement of {}", old2.id), now)?;
                let end = new.valid_from.unwrap_or(now);
                c.execute(
                    "UPDATE memories SET superseded_by=?2,valid_to=COALESCE(valid_to,?3),invalidated_at=?4 WHERE id=?1",
                    rusqlite::params![old2.id, new2, end, now],
                )?;
                transition(c, &old2.id, Some(old2.status), MemoryStatus::Superseded, &format!("superseded by {new2}"), now)?;
                close_questions(c, &principal, &old2.id, now)?;
                c.execute(
                    "INSERT INTO memory_admissions(id,principal_id,scope_id,memory_id,decision,similarity,against_id,classifier,detail,at) VALUES(?1,?2,?3,?4,'refinement',NULL,?5,'explicit',?6,?7)",
                    rusqlite::params![new_id("adm"), principal, scope, new2, old2.id, json!({"action":"supersede"}).to_string(), now],
                )?;
                log(c, &principal, &scope, "supersede", &old2.id, json!({"replacement": new2}), run.as_deref(), now)?;
                Ok(())
            })
            .await?;
        Ok(CurationResult {
            action: "supersede".into(),
            subject: old.id.clone(),
            changed: vec![old.id, new_id_],
            admission: None,
        })
    }

    /// Withdraw a record that is wrong or no longer supported. Content and history are kept.
    pub async fn retire(
        &self,
        grant: &MemoryGrant,
        id: &str,
        reason: &str,
        now: i64,
    ) -> Result<CurationResult> {
        let memory = self
            .memory(grant, id)
            .await?
            .ok_or_else(|| Error::NotFound(id.into()))?;
        if matches!(memory.status, MemoryStatus::Retired) {
            return Ok(CurationResult {
                action: "retire".into(),
                subject: memory.id,
                changed: vec![],
                admission: None,
            });
        }
        if memory.trust > grant.max_trust {
            return Err(Error::Scope(
                "this caller cannot retire a record asserted with higher trust".into(),
            ));
        }
        let reason = self.redactor.redact_text(reason.trim());
        if reason.is_empty() || reason.len() > 1024 {
            return Err(invalid(
                "a retirement reason of 1 to 1024 bytes is required",
            ));
        }
        let (principal, m, run) = (
            grant.principal.clone(),
            memory.clone(),
            grant.run_id.clone(),
        );
        self.db
            .write(Priority::Normal, move |c| {
                c.execute(
                    "UPDATE memories SET invalidated_at=COALESCE(invalidated_at,?2) WHERE id=?1",
                    rusqlite::params![m.id, now],
                )?;
                transition(
                    c,
                    &m.id,
                    Some(m.status),
                    MemoryStatus::Retired,
                    &format!("retired: {reason}"),
                    now,
                )?;
                close_questions(c, &principal, &m.id, now)?;
                log(
                    c,
                    &principal,
                    &m.scope,
                    "retire",
                    &m.id,
                    json!({"reason": reason}),
                    run.as_deref(),
                    now,
                )?;
                Ok(())
            })
            .await?;
        Ok(CurationResult {
            action: "retire".into(),
            subject: memory.id.clone(),
            changed: vec![memory.id],
            admission: None,
        })
    }

    /// Settle a dispute: `keep` becomes current and every record disputed with it in an open
    /// question is superseded by it.
    pub async fn resolve_dispute(
        &self,
        grant: &MemoryGrant,
        keep: &str,
        now: i64,
    ) -> Result<CurationResult> {
        let memory = self
            .memory(grant, keep)
            .await?
            .ok_or_else(|| Error::NotFound(keep.into()))?;
        if memory.status != MemoryStatus::Disputed {
            return Err(invalid("only a disputed record can settle a dispute"));
        }
        if memory.trust > grant.max_trust {
            return Err(Error::Scope(
                "this caller cannot resolve a higher-trust dispute".into(),
            ));
        }
        let max_trust = grant.max_trust;
        let (principal, m, run) = (
            grant.principal.clone(),
            memory.clone(),
            grant.run_id.clone(),
        );
        let changed = self
            .db
            .write(Priority::Normal, move |c| {
                let mut s = c.prepare("SELECT id,memory_ids FROM memory_questions WHERE principal_id=?1 AND scope_id=?2 AND state='open'")?;
                let questions = s
                    .query_map(rusqlite::params![principal, m.scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut others = BTreeSet::new();
                for (qid, ids) in questions {
                    let ids: Vec<String> = serde_json::from_str(&ids).unwrap_or_default();
                    if ids.contains(&m.id) {
                        others.extend(ids.into_iter().filter(|i| i != &m.id));
                        c.execute("UPDATE memory_questions SET state='resolved' WHERE id=?1", [&qid])?;
                    }
                }
                let mut changed = vec![m.id.clone()];
                transition(c, &m.id, Some(MemoryStatus::Disputed), MemoryStatus::Active, "dispute resolved", now)?;
                for other in others {
                    if let Some(o) = load_memory(c, &other)?
                        && o.principal == m.principal
                        && o.scope == m.scope
                        && o.status == MemoryStatus::Disputed
                    {
                        if o.trust > max_trust {
                            return Err(excalibur_store::Error::Invalid("cannot resolve a higher-trust dispute"));
                        }
                        c.execute(
                            "UPDATE memories SET superseded_by=?2,valid_to=COALESCE(valid_to,?3),invalidated_at=?3 WHERE id=?1",
                            rusqlite::params![o.id, m.id, now],
                        )?;
                        transition(c, &o.id, Some(MemoryStatus::Disputed), MemoryStatus::Superseded, &format!("dispute resolved in favour of {}", m.id), now)?;
                        changed.push(o.id);
                    }
                }
                log(c, &principal, &m.scope, "resolve_dispute", &m.id, json!({"superseded": &changed[1..]}), run.as_deref(), now)?;
                Ok(changed)
            })
            .await?;
        Ok(CurationResult {
            action: "resolve_dispute".into(),
            subject: memory.id,
            changed,
            admission: None,
        })
    }

    /// Merge `merge` into `keep` (same scope). Links, edges and aliases move to `keep`;
    /// the merged entity stays with `merged_into` for audit.
    pub async fn merge_entities(
        &self,
        grant: &MemoryGrant,
        keep: &str,
        merge: &str,
        now: i64,
    ) -> Result<CurationResult> {
        if keep == merge {
            return Err(invalid("cannot merge an entity into itself"));
        }
        grant.validate()?;
        let (principal, keep2, merge2, scopes, run) = (
            grant.principal.clone(),
            keep.to_string(),
            merge.to_string(),
            grant.scopes.clone(),
            grant.run_id.clone(),
        );
        let changed = self
            .db
            .write(Priority::Normal, move |c| {
                let row = |id: &str| -> rusqlite::Result<Option<(String, String, String, Option<String>)>> {
                    c.query_row(
                        "SELECT COALESCE(scope_id,''),canonical_name,aliases,merged_into FROM entities WHERE id=?1 AND principal_id=?2",
                        rusqlite::params![id, principal],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()
                };
                let (Some(k), Some(m)) = (row(&keep2)?, row(&merge2)?) else {
                    return Err(excalibur_store::Error::Invalid("unknown entity"));
                };
                if k.0 != m.0 || !scopes.contains(&k.0) {
                    return Err(excalibur_store::Error::Invalid("entities must share one granted scope"));
                }
                if k.3.is_some() || m.3.is_some() {
                    return Err(excalibur_store::Error::Invalid("entity already merged"));
                }
                let mut aliases: BTreeSet<String> = serde_json::from_str(&k.2).unwrap_or_default();
                aliases.extend(serde_json::from_str::<BTreeSet<String>>(&m.2).unwrap_or_default());
                aliases.insert(m.1.clone());
                aliases.remove(&k.1);
                c.execute(
                    "UPDATE entities SET aliases=?2,updated_at=?3 WHERE id=?1",
                    rusqlite::params![keep2, serde_json::to_string(&aliases).unwrap(), now],
                )?;
                let mut s = c.prepare("SELECT memory_id FROM memory_entity_links WHERE entity_id=?1")?;
                let memories = s.query_map([&merge2], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                for memory in &memories {
                    c.execute("INSERT OR IGNORE INTO memory_entity_links(memory_id,entity_id) VALUES(?1,?2)", rusqlite::params![memory, keep2])?;
                    c.execute("DELETE FROM memory_entity_links WHERE memory_id=?1 AND entity_id=?2", rusqlite::params![memory, merge2])?;
                    let mut s2 = c.prepare("SELECT entity_id FROM memory_entity_links WHERE memory_id=?1 ORDER BY entity_id")?;
                    let all = s2.query_map([memory], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                    c.execute("UPDATE memories SET entities=?2 WHERE id=?1", rusqlite::params![memory, serde_json::to_string(&all).unwrap()])?;
                }
                c.execute("UPDATE edges SET source_id=?2,updated_at=?4 WHERE layer='memory' AND principal_id=?3 AND source_id=?1", rusqlite::params![merge2, keep2, principal, now])?;
                c.execute("UPDATE edges SET destination_id=?2,updated_at=?4 WHERE layer='memory' AND principal_id=?3 AND destination_id=?1", rusqlite::params![merge2, keep2, principal, now])?;
                c.execute("UPDATE entities SET merged_into=?2,updated_at=?3 WHERE id=?1", rusqlite::params![merge2, keep2, now])?;
                log(c, &principal, &k.0, "merge_entities", &keep2, json!({"merged": merge2, "memories": memories}), run.as_deref(), now)?;
                Ok(memories)
            })
            .await?;
        Ok(CurationResult {
            action: "merge_entities".into(),
            subject: keep.into(),
            changed,
            admission: None,
        })
    }

    /// Register a relation type for this principal. User-asserted types are active; agent
    /// definitions stay proposed until used consistently or confirmed.
    #[allow(clippy::too_many_arguments)]
    pub async fn define_relation_type(
        &self,
        grant: &MemoryGrant,
        name: &str,
        description: &str,
        domain: Option<&str>,
        range: Option<&str>,
        inverse: Option<&str>,
        now: i64,
    ) -> Result<CurationResult> {
        grant.validate()?;
        let name = name.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        if name.is_empty()
            || name.len() > 64
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(invalid(
                "relation names use 1 to 64 letters, digits or underscores",
            ));
        }
        self.ensure_principal(&grant.principal).await?;
        let state = if grant.max_trust == Trust::User {
            "active"
        } else {
            "proposed"
        };
        let (principal, description, domain, range, inverse, run) = (
            grant.principal.clone(),
            self.redactor.redact_text(description.trim()),
            domain.map(str::to_string),
            range.map(str::to_string),
            inverse.map(str::to_string),
            grant.run_id.clone(),
        );
        let name2 = name.clone();
        let created = self
            .db
            .write(Priority::Normal, move |c| {
                ensure_seeds(c, &principal)?;
                let inverse_id: Option<String> = match &inverse {
                    Some(inv) => c
                        .query_row(
                            "SELECT id FROM relation_types WHERE principal_id=?1 AND name=?2",
                            rusqlite::params![principal, inv],
                            |r| r.get(0),
                        )
                        .optional()?,
                    None => None,
                };
                let inserted = c.execute(
                    "INSERT OR IGNORE INTO relation_types(id,name,description,domain,range,inverse_id,origin,state,principal_id,proposed_at,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,'learned',?7,?8,?9,?9,?9)",
                    rusqlite::params![new_id("rel"), name2, description, domain, range, inverse_id, state, principal, now],
                )? == 1;
                if inserted {
                    log(c, &principal, "general", "define_relation_type", &name2, json!({"state": state}), run.as_deref(), now)?;
                }
                Ok(inserted)
            })
            .await?;
        Ok(CurationResult {
            action: "define_relation_type".into(),
            subject: name.clone(),
            changed: if created { vec![name] } else { vec![] },
            admission: None,
        })
    }
}

fn close_questions(
    c: &rusqlite::Connection,
    principal: &str,
    id: &str,
    _now: i64,
) -> rusqlite::Result<()> {
    let mut s = c.prepare(
        "SELECT id,memory_ids FROM memory_questions WHERE principal_id=?1 AND state='open'",
    )?;
    let rows = s
        .query_map([principal], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (qid, ids) in rows {
        let ids: Vec<String> = serde_json::from_str(&ids).unwrap_or_default();
        if ids.iter().any(|i| i == id) {
            c.execute(
                "UPDATE memory_questions SET state='resolved' WHERE id=?1",
                [&qid],
            )?;
        }
    }
    Ok(())
}

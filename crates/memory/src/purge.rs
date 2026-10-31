//! Memory-owned derived rows for the shared purge service (`excalibur-retention`).
//! Covers extracted keys, embeddings of memories/messages/chunks/keys, admission and
//! transition history, open questions and entity links.
use crate::embeddings::EmbeddingRegistry;
use excalibur_retention::{Collection, MemberKind, PurgeParticipant, SubjectKind, SubjectSnapshot};
use excalibur_store::rusqlite::{self, Connection, OptionalExtension};
use serde_json::Value;

/// Register with `PurgeService::with_participant` whenever the memory schema is installed.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryPurgeParticipant;

fn exists(c: &Connection, table: &str) -> rusqlite::Result<bool> {
    c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )
}
fn list<'a>(ids: impl Iterator<Item = &'a str>) -> String {
    Value::Array(ids.map(|id| Value::String(id.into())).collect()).to_string()
}
impl PurgeParticipant for MemoryPurgeParticipant {
    fn name(&self) -> &'static str {
        "memory"
    }
    fn collect(
        &self,
        c: &Connection,
        subject: &SubjectSnapshot,
        out: &mut Collection,
    ) -> excalibur_store::Result<()> {
        if !exists(c, "memory_keys")? {
            return Ok(());
        }
        let mut keys = |sql: &str, id: &str| -> excalibur_store::Result<()> {
            let mut statement = c.prepare(sql)?;
            let rows = statement
                .query_map([id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (key, scope, text) in rows {
                out.add_derived(key, scope);
                out.add_text(text);
            }
            Ok(())
        };
        match subject.kind {
            SubjectKind::Memory => {
                keys(
                    "SELECT id,scope_id,key_text FROM memory_keys WHERE memory_id=?1",
                    &subject.id,
                )?;
                let normalized: Option<String> = c
                    .query_row(
                        "SELECT normalized FROM memories WHERE id=?1",
                        [&subject.id],
                        |r| r.get(0),
                    )
                    .optional()?;
                out.add_text(normalized.unwrap_or_default());
            }
            SubjectKind::Conversation => {
                for message in subject.members_of(MemberKind::Message) {
                    keys(
                        "SELECT id,scope_id,key_text FROM memory_keys WHERE message_id=?1",
                        &message.id,
                    )?;
                }
            }
            SubjectKind::Document => {}
        }
        Ok(())
    }
    fn purge(
        &self,
        c: &Connection,
        subject: &SubjectSnapshot,
        touched: &mut Vec<String>,
    ) -> excalibur_store::Result<u64> {
        let mut removed = 0u64;
        touched.extend(
            [
                "memory_keys",
                "memory_vector_objects",
                "memory_index_queue",
                "memory_admissions",
                "memory_transitions",
                "memory_questions",
                "memory_entity_links",
            ]
            .map(String::from),
        );
        if exists(c, "memory_vector_objects")? {
            for member in &subject.members {
                let objects: Vec<String> = match member.kind {
                    MemberKind::Memory => vec![format!("memory:{}", member.id)],
                    MemberKind::Message => vec![
                        format!("message:{}", member.id),
                        format!("msg:{}", member.id),
                    ],
                    MemberKind::Chunk => vec![format!("chunk:{}", member.id)],
                    MemberKind::Derived => vec![format!("key:{}", member.id)],
                    MemberKind::Conversation | MemberKind::Document => Vec::new(),
                };
                for object in objects {
                    EmbeddingRegistry::remove_in(c, &member.scope, &object)?;
                }
            }
        }
        if !exists(c, "memory_keys")? {
            return Ok(removed);
        }
        let derived = list(
            subject
                .members_of(MemberKind::Derived)
                .map(|m| m.id.as_str()),
        );
        removed += c.execute(
            "DELETE FROM memory_keys WHERE id IN (SELECT value FROM json_each(?1))",
            [&derived],
        )? as u64;
        match subject.kind {
            SubjectKind::Memory => {
                let id = &subject.id;
                c.execute(
                    "UPDATE memories SET superseded_by=NULL WHERE superseded_by=?1",
                    [id],
                )?;
                removed += c.execute("DELETE FROM memory_keys WHERE memory_id=?1", [id])? as u64;
                removed += c.execute(
                    "DELETE FROM memory_admissions WHERE memory_id=?1 OR against_id=?1",
                    [id],
                )? as u64;
                removed +=
                    c.execute("DELETE FROM memory_transitions WHERE memory_id=?1", [id])? as u64;
                c.execute(
                    "UPDATE memory_questions SET state='closed',question='[purged]' WHERE memory_ids LIKE '%' || ?1 || '%'",
                    [id],
                )?;
                removed +=
                    c.execute("DELETE FROM memory_entity_links WHERE memory_id=?1", [id])? as u64;
                if exists(c, "relation_needs")? {
                    removed +=
                        c.execute("DELETE FROM relation_needs WHERE memory_id=?1", [id])? as u64;
                }
                if exists(c, "memory_curations")? {
                    removed +=
                        c.execute("DELETE FROM memory_curations WHERE subject=?1", [id])? as u64;
                }
            }
            SubjectKind::Conversation => {
                let messages = list(
                    subject
                        .members_of(MemberKind::Message)
                        .map(|m| m.id.as_str()),
                );
                removed += c.execute(
                    "DELETE FROM memory_keys WHERE message_id IN (SELECT value FROM json_each(?1))",
                    [&messages],
                )? as u64;
            }
            SubjectKind::Document => {
                if exists(c, "chunk_retrievals")? {
                    let chunks = list(subject.members_of(MemberKind::Chunk).map(|m| m.id.as_str()));
                    removed += c.execute(
                        "DELETE FROM chunk_retrievals WHERE chunk_id IN (SELECT value FROM json_each(?1))",
                        [&chunks],
                    )? as u64;
                }
            }
        }
        Ok(removed)
    }
}

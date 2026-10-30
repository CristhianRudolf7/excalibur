//! Identity documents and notebook stored as hashed, versioned fragments (architecture 10.2).
use crate::{
    Error, MemoryGrant, MemoryStore, Result, Trust, digest, invalid, new_id,
    text::{fts_or, normalize, query_terms},
};
use excalibur_store::{
    Priority, ReadLane,
    rusqlite::{self, OptionalExtension},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Profile fragments belong to the principal, not to a scope.
pub const PRINCIPAL_SCOPE: &str = "principal";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "doc", content = "id")]
pub enum DocName {
    AgentProfile,
    UserProfile,
    Notebook,
    Persona(String),
}
impl DocName {
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "agent_profile" => Self::AgentProfile,
            "user_profile" => Self::UserProfile,
            "notebook" => Self::Notebook,
            other => match other.strip_prefix("persona:") {
                Some(id) if !id.is_empty() => Self::Persona(id.into()),
                _ => return Err(invalid(format!("unknown document {value}"))),
            },
        })
    }
    fn kind(&self) -> &'static str {
        match self {
            Self::AgentProfile => "agent_profile",
            Self::UserProfile => "user_profile",
            Self::Notebook => "notebook",
            Self::Persona(_) => "persona",
        }
    }
    fn is_profile(&self) -> bool {
        !matches!(self, Self::Notebook)
    }
}
fn document_id(principal: &str, name: &DocName) -> String {
    match name {
        DocName::Persona(id) => id.clone(),
        other => format!(
            "idoc_{}",
            &digest(format!("{principal}\0{}", other.kind()).as_bytes())[..24]
        ),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fragment {
    pub id: String,
    pub document_id: String,
    pub heading_path: Vec<String>,
    pub ordinal: i64,
    pub text: String,
    pub version: i64,
    pub hash: String,
    pub scope: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadingEntry {
    pub heading_path: Vec<String>,
    pub fragment_id: String,
    pub hash: String,
    pub chars: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum DocSelector {
    Index,
    Section(Vec<String>),
    Search(String),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum DocReadResult {
    Index { headings: Vec<HeadingEntry> },
    Fragments { fragments: Vec<Fragment> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum DocEdit {
    Replace {
        fragment_id: String,
        expected_hash: String,
        text: String,
    },
    InsertAfter {
        fragment_id: String,
        expected_hash: String,
        text: String,
    },
    Append {
        heading_path: Vec<String>,
        #[serde(default)]
        scope: Option<String>,
        text: String,
    },
    Delete {
        fragment_id: String,
        expected_hash: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditContext {
    /// Run (or host actor) performing the edit; mismatches are counted per actor.
    pub actor: String,
    /// Message that motivates a profile change.
    pub provenance: Option<String>,
    pub explicit_user_request: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum EditOutcome {
    Applied {
        fragment_id: String,
        version: i64,
        hash: String,
    },
    /// Not applied: the fragment changed since it was read.
    Mismatch {
        current: Option<Fragment>,
        consecutive: u32,
        escalate: bool,
    },
    Proposed {
        proposal_id: String,
        evidence_count: i64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileProposal {
    pub id: String,
    pub document_id: String,
    pub fragment_id: Option<String>,
    pub heading_path: Vec<String>,
    pub operation: String,
    pub text: String,
    pub evidence_count: i64,
    pub state: String,
    pub provenance: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreSnapshot {
    pub document_id: String,
    pub text: String,
    pub hash: String,
    pub fragments: Vec<String>,
}

fn fragment_hash(path: &[String], text: &str) -> String {
    digest(format!("{}\0{text}", serde_json::to_string(path).unwrap()).as_bytes())
}
const FRAGMENT_COLUMNS: &str = "id,document_id,COALESCE(heading_path,'[]'),ordinal,text,version,COALESCE(hash,''),COALESCE(scope_id,'principal')";
fn fragment_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Fragment> {
    let path: String = r.get(2)?;
    Ok(Fragment {
        id: r.get(0)?,
        document_id: r.get(1)?,
        heading_path: serde_json::from_str(&path).unwrap_or_default(),
        ordinal: r.get(3)?,
        text: r.get(4)?,
        version: r.get(5)?,
        hash: r.get(6)?,
        scope: r.get(7)?,
    })
}
fn readable(fragment: &Fragment, grant: &MemoryGrant) -> bool {
    fragment.scope == PRINCIPAL_SCOPE || grant.scopes.contains(&fragment.scope)
}
fn load_fragment(
    c: &rusqlite::Connection,
    id: &str,
    document: &str,
) -> rusqlite::Result<Option<Fragment>> {
    c.query_row(
        &format!("SELECT {FRAGMENT_COLUMNS} FROM doc_fragments WHERE id=?1 AND document_id=?2 AND deleted=0"),
        rusqlite::params![id, document],
        fragment_row,
    )
    .optional()
}
fn record_version(
    c: &rusqlite::Connection,
    f: &Fragment,
    deleted: bool,
    operation: &str,
    ctx: &EditContext,
    now: i64,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO doc_fragment_versions(fragment_id,version,document_id,heading_path,ordinal,text,hash,deleted,operation,actor,provenance,recorded_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        rusqlite::params![f.id, f.version, f.document_id, serde_json::to_string(&f.heading_path).unwrap(), f.ordinal, f.text, f.hash, deleted, operation, ctx.actor, json!({"message": ctx.provenance, "explicit": ctx.explicit_user_request}).to_string(), now],
    )?;
    Ok(())
}
/// Ordinal immediately after `anchor`, renumbering the document when no gap remains.
fn ordinal_after(c: &rusqlite::Connection, document: &str, anchor: i64) -> rusqlite::Result<i64> {
    let next: Option<i64> = c.query_row(
        "SELECT min(ordinal) FROM doc_fragments WHERE document_id=?1 AND ordinal>?2",
        rusqlite::params![document, anchor],
        |r| r.get(0),
    )?;
    match next {
        None => Ok(anchor + 1024),
        Some(next) if next - anchor > 1 => Ok(anchor + (next - anchor) / 2),
        Some(_) => {
            let mut s = c.prepare(
                "SELECT id,ordinal FROM doc_fragments WHERE document_id=?1 ORDER BY ordinal,id",
            )?;
            let rows = s
                .query_map([document], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut new_anchor = anchor;
            for (i, (id, ordinal)) in rows.iter().enumerate() {
                let value = (i as i64 + 1) * 1024;
                if *ordinal == anchor {
                    new_anchor = value;
                }
                c.execute(
                    "UPDATE doc_fragments SET ordinal=?2 WHERE id=?1",
                    rusqlite::params![id, value],
                )?;
            }
            Ok(new_anchor + 512)
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn insert_fragment(
    c: &rusqlite::Connection,
    principal: &str,
    document: &str,
    path: &[String],
    ordinal: i64,
    scope: &str,
    text: &str,
    ctx: &EditContext,
    operation: &str,
    now: i64,
) -> rusqlite::Result<Fragment> {
    let f = Fragment {
        id: new_id("frag"),
        document_id: document.into(),
        heading_path: path.to_vec(),
        ordinal,
        text: text.into(),
        version: 1,
        hash: fragment_hash(path, text),
        scope: scope.into(),
    };
    c.execute(
        "INSERT INTO doc_fragments(id,document_id,heading_path,ordinal,text,version,hash,principal_id,scope_id,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,1,?6,?7,?8,?9,?9)",
        rusqlite::params![f.id, document, serde_json::to_string(path).unwrap(), ordinal, text, f.hash, principal, scope, now],
    )?;
    record_version(c, &f, false, operation, ctx, now)?;
    Ok(f)
}
fn ensure_document(
    c: &rusqlite::Connection,
    principal: &str,
    name: &DocName,
) -> rusqlite::Result<String> {
    let id = document_id(principal, name);
    if let DocName::Persona(_) = name {
        return Ok(id);
    }
    c.execute(
        "INSERT OR IGNORE INTO identity_docs(id,name,kind,principal_id,scope_id) VALUES(?1,?2,?2,?3,'principal')",
        rusqlite::params![id, name.kind(), principal],
    )?;
    Ok(id)
}

enum Applied {
    Done(EditOutcome),
    Fail(String),
}

#[allow(clippy::too_many_arguments)]
fn apply_edit(
    c: &rusqlite::Connection,
    principal: &str,
    grant_scopes: &std::collections::BTreeSet<String>,
    document: &str,
    profile: bool,
    edit: &DocEdit,
    ctx: &EditContext,
    escalation: u32,
    now: i64,
) -> rusqlite::Result<Applied> {
    let check = |fragment_id: &str,
                 expected: &str|
     -> rusqlite::Result<std::result::Result<Fragment, EditOutcome>> {
        let current = load_fragment(c, fragment_id, document)?
            .filter(|f| f.scope == PRINCIPAL_SCOPE || grant_scopes.contains(&f.scope));
        match current {
            Some(f) if f.hash == expected => {
                c.execute(
                    "DELETE FROM doc_edit_mismatches WHERE actor=?1 AND fragment_id=?2",
                    rusqlite::params![ctx.actor, fragment_id],
                )?;
                Ok(Ok(f))
            }
            other => {
                c.execute("INSERT INTO doc_edit_mismatches(actor,fragment_id,count) VALUES(?1,?2,1) ON CONFLICT(actor,fragment_id) DO UPDATE SET count=count+1", rusqlite::params![ctx.actor, fragment_id])?;
                let count: u32 = c.query_row(
                    "SELECT count FROM doc_edit_mismatches WHERE actor=?1 AND fragment_id=?2",
                    rusqlite::params![ctx.actor, fragment_id],
                    |r| r.get(0),
                )?;
                Ok(Err(EditOutcome::Mismatch {
                    current: other,
                    consecutive: count,
                    escalate: count >= escalation,
                }))
            }
        }
    };
    Ok(Applied::Done(match edit {
        DocEdit::Replace {
            fragment_id,
            expected_hash,
            text,
        } => {
            match check(fragment_id, expected_hash)? {
                Err(outcome) => outcome,
                Ok(mut f) => {
                    f.text = text.clone();
                    f.version += 1;
                    f.hash = fragment_hash(&f.heading_path, text);
                    c.execute("UPDATE doc_fragments SET text=?2,version=?3,hash=?4,updated_at=?5 WHERE id=?1", rusqlite::params![f.id, f.text, f.version, f.hash, now])?;
                    record_version(c, &f, false, "replace", ctx, now)?;
                    EditOutcome::Applied {
                        fragment_id: f.id,
                        version: f.version,
                        hash: f.hash,
                    }
                }
            }
        }
        DocEdit::Delete {
            fragment_id,
            expected_hash,
        } => match check(fragment_id, expected_hash)? {
            Err(outcome) => outcome,
            Ok(mut f) => {
                f.version += 1;
                c.execute(
                    "UPDATE doc_fragments SET deleted=1,version=?2,updated_at=?3 WHERE id=?1",
                    rusqlite::params![f.id, f.version, now],
                )?;
                record_version(c, &f, true, "delete", ctx, now)?;
                EditOutcome::Applied {
                    fragment_id: f.id,
                    version: f.version,
                    hash: f.hash,
                }
            }
        },
        DocEdit::InsertAfter {
            fragment_id,
            expected_hash,
            text,
        } => match check(fragment_id, expected_hash)? {
            Err(outcome) => outcome,
            Ok(anchor) => {
                let ordinal = ordinal_after(c, document, anchor.ordinal)?;
                let f = insert_fragment(
                    c,
                    principal,
                    document,
                    &anchor.heading_path,
                    ordinal,
                    &anchor.scope,
                    text,
                    ctx,
                    "insert",
                    now,
                )?;
                EditOutcome::Applied {
                    fragment_id: f.id,
                    version: f.version,
                    hash: f.hash,
                }
            }
        },
        DocEdit::Append {
            heading_path,
            scope,
            text,
        } => {
            let scope = if profile {
                PRINCIPAL_SCOPE.to_string()
            } else {
                match scope {
                    Some(s) if grant_scopes.contains(s) => s.clone(),
                    Some(s) => return Ok(Applied::Fail(format!("scope {s} is not granted"))),
                    None => return Ok(Applied::Fail("notebook entries need a scope".into())),
                }
            };
            let last: Option<i64> = c.query_row(
                "SELECT max(ordinal) FROM doc_fragments WHERE document_id=?1 AND heading_path=?2",
                rusqlite::params![document, serde_json::to_string(heading_path).unwrap()],
                |r| r.get(0),
            )?;
            let ordinal = match last {
                Some(anchor) => ordinal_after(c, document, anchor)?,
                None => c.query_row(
                    "SELECT COALESCE(max(ordinal),0)+1024 FROM doc_fragments WHERE document_id=?1",
                    [document],
                    |r| r.get(0),
                )?,
            };
            let f = insert_fragment(
                c,
                principal,
                document,
                heading_path,
                ordinal,
                &scope,
                text,
                ctx,
                "insert",
                now,
            )?;
            EditOutcome::Applied {
                fragment_id: f.id,
                version: f.version,
                hash: f.hash,
            }
        }
    }))
}

impl MemoryStore {
    async fn resolve_document(&self, grant: &MemoryGrant, name: &DocName) -> Result<String> {
        grant.validate()?;
        self.ensure_principal(&grant.principal).await?;
        let (principal, name2) = (grant.principal.clone(), name.clone());
        let id = self
            .db
            .write(Priority::Normal, move |c| {
                let id = ensure_document(c, &principal, &name2)?;
                let owner: Option<String> = c
                    .query_row(
                        "SELECT principal_id FROM identity_docs WHERE id=?1",
                        [&id],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                Ok((owner == Some(principal)).then_some(id))
            })
            .await?;
        id.ok_or_else(|| Error::NotFound("document".into()))
    }

    /// `doc_read`: heading index, one section or matching fragments; never the whole document.
    pub async fn doc_read(
        &self,
        grant: &MemoryGrant,
        name: &DocName,
        selector: DocSelector,
    ) -> Result<DocReadResult> {
        let document = self.resolve_document(grant, name).await?;
        let grant2 = grant.clone();
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                let all = || -> rusqlite::Result<Vec<Fragment>> {
                    let mut s = c.prepare(&format!("SELECT {FRAGMENT_COLUMNS} FROM doc_fragments WHERE document_id=?1 AND deleted=0 ORDER BY ordinal,id"))?;
                    let rows = s.query_map([&document], fragment_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows.into_iter().filter(|f| readable(f, &grant2)).collect())
                };
                Ok(match selector {
                    DocSelector::Index => DocReadResult::Index {
                        headings: all()?
                            .into_iter()
                            .map(|f| HeadingEntry { chars: f.text.chars().count(), heading_path: f.heading_path, fragment_id: f.id, hash: f.hash })
                            .collect(),
                    },
                    DocSelector::Section(prefix) => DocReadResult::Fragments {
                        fragments: all()?.into_iter().filter(|f| f.heading_path.starts_with(&prefix)).take(50).collect(),
                    },
                    DocSelector::Search(query) => {
                        let mut fragments = Vec::new();
                        if let Some(q) = fts_or(&query_terms(&query)) {
                            let mut s = c.prepare("SELECT f.id,f.document_id,COALESCE(f.heading_path,'[]'),f.ordinal,f.text,f.version,COALESCE(f.hash,''),COALESCE(f.scope_id,'principal') FROM doc_fragments_fts x JOIN doc_fragments f ON f.rowid=x.rowid WHERE doc_fragments_fts MATCH ?1 AND f.document_id=?2 AND f.deleted=0 ORDER BY bm25(doc_fragments_fts) LIMIT 50")?;
                            fragments = s.query_map(rusqlite::params![q, document], fragment_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
                        }
                        DocReadResult::Fragments { fragments: fragments.into_iter().filter(|f| readable(f, &grant2)).take(20).collect() }
                    }
                })
            })
            .await?)
    }

    /// `doc_edit`: hash-checked replace/insert/delete. Profile changes need provenance and,
    /// unless explicitly requested by the user, become proposals.
    pub async fn doc_edit(
        &self,
        grant: &MemoryGrant,
        name: &DocName,
        edit: DocEdit,
        ctx: EditContext,
        now: i64,
    ) -> Result<EditOutcome> {
        if ctx.actor.trim().is_empty() {
            return Err(invalid("edit actor required"));
        }
        let document = self.resolve_document(grant, name).await?;
        let profile = name.is_profile();
        let mut edit = edit;
        match &mut edit {
            DocEdit::Replace { text, .. }
            | DocEdit::InsertAfter { text, .. }
            | DocEdit::Append { text, .. } => {
                *text = self.redactor.redact_text(text.trim());
                if text.is_empty() || text.len() > 16 * 1024 {
                    return Err(invalid("fragment text must contain 1 to 16384 bytes"));
                }
            }
            DocEdit::Delete { .. } => {}
        }
        if profile && ctx.provenance.is_none() {
            return Err(invalid("profile changes require the motivating message"));
        }
        let escalation = self.config.doc_edit_escalation_mismatches;
        let (principal, scopes) = (grant.principal.clone(), grant.scopes.clone());
        if profile && !ctx.explicit_user_request && !matches!(name, DocName::Persona(_)) {
            return self
                .propose_profile_change(grant, &document, edit, ctx, now)
                .await;
        }
        match self
            .db
            .write(Priority::Normal, move |c| {
                apply_edit(
                    c, &principal, &scopes, &document, profile, &edit, &ctx, escalation, now,
                )
                .map_err(Into::into)
            })
            .await?
        {
            Applied::Done(outcome) => Ok(outcome),
            Applied::Fail(message) => Err(Error::Scope(message)),
        }
    }

    async fn propose_profile_change(
        &self,
        grant: &MemoryGrant,
        document: &str,
        edit: DocEdit,
        ctx: EditContext,
        now: i64,
    ) -> Result<EditOutcome> {
        let allow_auto = grant.max_trust == Trust::User;
        let (principal, document, scopes) = (
            grant.principal.clone(),
            document.to_string(),
            grant.scopes.clone(),
        );
        Ok(self
            .db
            .write(Priority::Normal, move |c| {
                let (fragment, path, operation, text) = match &edit {
                    DocEdit::Replace {
                        fragment_id,
                        expected_hash,
                        text,
                    }
                    | DocEdit::InsertAfter {
                        fragment_id,
                        expected_hash,
                        text,
                    } => {
                        let current = load_fragment(c, fragment_id, &document)?;
                        match current {
                            Some(f) if &f.hash == expected_hash => (
                                Some(f.id.clone()),
                                f.heading_path.clone(),
                                if matches!(edit, DocEdit::Replace { .. }) {
                                    "replace"
                                } else {
                                    "insert_after"
                                },
                                text.clone(),
                            ),
                            other => {
                                let _ = &scopes;
                                return Ok(EditOutcome::Mismatch {
                                    current: other,
                                    consecutive: 1,
                                    escalate: false,
                                });
                            }
                        }
                    }
                    DocEdit::Append {
                        heading_path, text, ..
                    } => (None, heading_path.clone(), "append", text.clone()),
                    DocEdit::Delete { fragment_id, .. } => (
                        Some(fragment_id.clone()),
                        Vec::new(),
                        "delete",
                        String::new(),
                    ),
                };
                Ok(upsert_proposal(
                    c,
                    &principal,
                    &document,
                    fragment.as_deref(),
                    &path,
                    operation,
                    &text,
                    ctx.provenance.as_deref().unwrap_or_default(),
                    allow_auto,
                    now,
                )?)
            })
            .await?)
    }

    /// Record repeated evidence for a profile change (consolidation step 8).
    pub(crate) async fn record_profile_evidence(
        &self,
        principal: &str,
        heading_path: Vec<String>,
        text: &str,
        provenance: &str,
        now: i64,
    ) -> Result<EditOutcome> {
        let (principal, text, provenance) = (
            principal.to_string(),
            text.to_string(),
            provenance.to_string(),
        );
        Ok(self
            .db
            .write(Priority::Background, move |c| {
                let document = ensure_document(c, &principal, &DocName::UserProfile)?;
                Ok(upsert_proposal(
                    c,
                    &principal,
                    &document,
                    None,
                    &heading_path,
                    "append",
                    &text,
                    &provenance,
                    true,
                    now,
                )?)
            })
            .await?)
    }

    pub async fn profile_proposals(&self, grant: &MemoryGrant) -> Result<Vec<ProfileProposal>> {
        grant.validate()?;
        let principal = grant.principal.clone();
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut s = c.prepare("SELECT id,document_id,fragment_id,heading_path,operation,text,evidence_count,state,provenance FROM profile_proposals WHERE principal_id=?1 ORDER BY created_at,id")?;
                let rows = s
                    .query_map([&principal], |r| {
                        let path: String = r.get(3)?;
                        let provenance: String = r.get(8)?;
                        Ok(ProfileProposal { id: r.get(0)?, document_id: r.get(1)?, fragment_id: r.get(2)?, heading_path: serde_json::from_str(&path).unwrap_or_default(), operation: r.get(4)?, text: r.get(5)?, evidence_count: r.get(6)?, state: r.get(7)?, provenance: serde_json::from_str(&provenance).unwrap_or_default() })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }

    /// Apply proposals with enough evidence (or one proposal confirmed by the user).
    pub async fn confirm_profile_proposals(
        &self,
        grant: &MemoryGrant,
        only: Option<&str>,
        now: i64,
    ) -> Result<Vec<String>> {
        grant.validate()?;
        if only.is_some() && grant.max_trust != Trust::User {
            return Err(Error::Scope(
                "only the user can confirm a profile proposal".into(),
            ));
        }
        let min = if only.is_some() {
            1
        } else {
            self.config.profile_confirmation_evidence as i64
        };
        let (principal, only) = (grant.principal.clone(), only.map(str::to_string));
        Ok(self
            .db
            .write(Priority::Normal, move |c| {
                let mut s = c.prepare("SELECT id,document_id,fragment_id,heading_path,operation,text,provenance FROM profile_proposals WHERE principal_id=?1 AND state='proposed' AND evidence_count>=?2 AND (?3 IS NULL OR id=?3) AND (?3 IS NOT NULL OR auto_confirmable=1) ORDER BY created_at,id")?;
                let rows = s
                    .query_map(rusqlite::params![principal, min, only], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut applied = Vec::new();
                for (id, document, fragment, path, operation, text, provenance) in rows {
                    let ctx = EditContext { actor: "consolidation".into(), provenance: Some(provenance), explicit_user_request: false };
                    let path: Vec<String> = serde_json::from_str(&path).unwrap_or_default();
                    let edit = match (operation.as_str(), fragment) {
                        ("append", _) => Some(DocEdit::Append { heading_path: path, scope: None, text }),
                        (op, Some(fragment)) => load_fragment(c, &fragment, &document)?.map(|f| match op {
                            "replace" => DocEdit::Replace { fragment_id: f.id, expected_hash: f.hash, text },
                            "insert_after" => DocEdit::InsertAfter { fragment_id: f.id, expected_hash: f.hash, text },
                            _ => DocEdit::Delete { fragment_id: f.id, expected_hash: f.hash },
                        }),
                        _ => None,
                    };
                    let state = match edit {
                        Some(edit) => match apply_edit(c, &principal, &Default::default(), &document, true, &edit, &ctx, u32::MAX, now)? {
                            Applied::Done(EditOutcome::Applied { .. }) => "applied",
                            _ => "stale",
                        },
                        None => "stale",
                    };
                    c.execute("UPDATE profile_proposals SET state=?2,updated_at=?3 WHERE id=?1", rusqlite::params![id, state, now])?;
                    if state == "applied" {
                        applied.push(id);
                    }
                }
                Ok(applied)
            })
            .await?)
    }

    /// Restore an earlier fragment version as a new version.
    pub async fn revert_fragment(
        &self,
        grant: &MemoryGrant,
        name: &DocName,
        fragment_id: &str,
        version: i64,
        ctx: EditContext,
        now: i64,
    ) -> Result<EditOutcome> {
        let document = self.resolve_document(grant, name).await?;
        let (principal, scopes, fragment, profile) = (
            grant.principal.clone(),
            grant.scopes.clone(),
            fragment_id.to_string(),
            name.is_profile(),
        );
        Ok(self
            .db
            .write(Priority::Normal, move |c| {
                let old: Option<(String, bool)> = c.query_row("SELECT text,deleted FROM doc_fragment_versions WHERE fragment_id=?1 AND version=?2 AND document_id=?3", rusqlite::params![fragment, version, document], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                let Some((text, deleted)) = old else {
                    return Err(excalibur_store::Error::Invalid("unknown fragment version"));
                };
                let current: Option<Fragment> = c.query_row(&format!("SELECT {FRAGMENT_COLUMNS} FROM doc_fragments WHERE id=?1 AND document_id=?2"), rusqlite::params![fragment, document], fragment_row).optional()?;
                let Some(mut f) = current.filter(|f| f.scope == PRINCIPAL_SCOPE || scopes.contains(&f.scope)) else {
                    return Err(excalibur_store::Error::Invalid("fragment not visible"));
                };
                let _ = (&principal, profile);
                f.version += 1;
                f.text = text;
                f.hash = fragment_hash(&f.heading_path, &f.text);
                c.execute("UPDATE doc_fragments SET text=?2,version=?3,hash=?4,deleted=?5,updated_at=?6 WHERE id=?1", rusqlite::params![f.id, f.text, f.version, f.hash, deleted, now])?;
                record_version(c, &f, deleted, "revert", &ctx, now)?;
                Ok(EditOutcome::Applied { fragment_id: f.id, version: f.version, hash: f.hash })
            })
            .await?)
    }

    /// Every retained version of a fragment, oldest first.
    pub async fn fragment_history(
        &self,
        grant: &MemoryGrant,
        name: &DocName,
        fragment_id: &str,
    ) -> Result<Vec<(i64, String, bool, String)>> {
        let document = self.resolve_document(grant, name).await?;
        let fragment = fragment_id.to_string();
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut s = c.prepare("SELECT version,text,deleted,operation FROM doc_fragment_versions WHERE fragment_id=?1 AND document_id=?2 ORDER BY version")?;
                let rows = s.query_map(rusqlite::params![fragment, document], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }

    /// A persona is a variant of the agent profile; user memory is untouched.
    pub async fn create_persona(
        &self,
        grant: &MemoryGrant,
        name: &str,
        icon: Option<&str>,
        now: i64,
    ) -> Result<String> {
        if name.trim().is_empty() || name.len() > 128 {
            return Err(invalid("persona name required"));
        }
        let base = self.resolve_document(grant, &DocName::AgentProfile).await?;
        let (principal, name, icon) = (
            grant.principal.clone(),
            name.trim().to_string(),
            icon.map(str::to_string),
        );
        Ok(self
            .db
            .write(Priority::Normal, move |c| {
                let id = new_id("persona");
                c.execute("INSERT INTO identity_docs(id,name,kind,icon,base_document_id,principal_id,scope_id,created_at,updated_at) VALUES(?1,?2,'persona',?3,?4,?5,'principal',?6,?6)", rusqlite::params![id, name, icon, base, principal, now])?;
                let mut s = c.prepare(&format!("SELECT {FRAGMENT_COLUMNS} FROM doc_fragments WHERE document_id=?1 AND deleted=0 ORDER BY ordinal,id"))?;
                let rows = s.query_map([&base], fragment_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
                let ctx = EditContext { actor: "persona".into(), provenance: None, explicit_user_request: true };
                for f in rows {
                    insert_fragment(c, &principal, &id, &f.heading_path, f.ordinal, PRINCIPAL_SCOPE, &f.text, &ctx, "persona_copy", now)?;
                }
                Ok(id)
            })
            .await?)
    }

    /// Stable agent-profile core for context layer 1, bounded and hashed so a session can
    /// freeze it at start without breaking the prompt cache.
    pub async fn agent_core(
        &self,
        grant: &MemoryGrant,
        persona: Option<&str>,
        max_chars: usize,
    ) -> Result<CoreSnapshot> {
        let name = persona
            .map(|p| DocName::Persona(p.into()))
            .unwrap_or(DocName::AgentProfile);
        let document = self.resolve_document(grant, &name).await?;
        let doc2 = document.clone();
        let fragments = self
            .db
            .read(ReadLane::Interactive, move |c| {
                let mut s = c.prepare(&format!("SELECT {FRAGMENT_COLUMNS} FROM doc_fragments WHERE document_id=?1 AND deleted=0 ORDER BY ordinal,id"))?;
                let rows = s.query_map([&doc2], fragment_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        let mut text = String::new();
        let mut ids = Vec::new();
        for f in fragments {
            if text.chars().count() + f.text.chars().count() + 1 > max_chars {
                break;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&f.text);
            ids.push(f.id);
        }
        Ok(CoreSnapshot {
            hash: digest(text.as_bytes()),
            document_id: document,
            text,
            fragments: ids,
        })
    }

    /// Notebook section entries of one scope (for compaction).
    pub(crate) async fn notebook_section(
        &self,
        principal: &str,
        scope: &str,
    ) -> Result<(String, Vec<Fragment>)> {
        let (principal, scope) = (principal.to_string(), scope.to_string());
        Ok(self
            .db
            .write(Priority::Background, move |c| {
                let document = ensure_document(c, &principal, &DocName::Notebook)?;
                let mut s = c.prepare(&format!("SELECT {FRAGMENT_COLUMNS} FROM doc_fragments WHERE document_id=?1 AND scope_id=?2 AND deleted=0 ORDER BY ordinal,id"))?;
                let rows = s.query_map(rusqlite::params![document, scope], fragment_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok((document, rows))
            })
            .await?)
    }

    /// Replace removed notebook entries with a compiled truth, keeping an append-only timeline.
    pub(crate) async fn apply_compaction(
        &self,
        principal: &str,
        scope: &str,
        document: &str,
        removed: Vec<Fragment>,
        compiled: String,
        now: i64,
    ) -> Result<String> {
        let (principal, scope, document) = (
            principal.to_string(),
            scope.to_string(),
            document.to_string(),
        );
        let compiled = self.redactor.redact_text(&compiled);
        Ok(self
            .db
            .write(Priority::Background, move |c| {
                let ctx = EditContext { actor: "consolidation".into(), provenance: None, explicit_user_request: false };
                for mut f in removed {
                    c.execute("INSERT INTO notebook_timeline(document_id,scope_id,fragment_id,text,recorded_at) VALUES(?1,?2,?3,?4,?5)", rusqlite::params![document, scope, f.id, f.text, now])?;
                    f.version += 1;
                    c.execute("UPDATE doc_fragments SET deleted=1,version=?2,updated_at=?3 WHERE id=?1", rusqlite::params![f.id, f.version, now])?;
                    record_version(c, &f, true, "compacted", &ctx, now)?;
                }
                let path = vec!["Compiled".to_string()];
                let existing: Option<Fragment> = c.query_row(&format!("SELECT {FRAGMENT_COLUMNS} FROM doc_fragments WHERE document_id=?1 AND scope_id=?2 AND heading_path=?3 AND deleted=0"), rusqlite::params![document, scope, serde_json::to_string(&path).unwrap()], fragment_row).optional()?;
                let id = match existing {
                    Some(mut f) => {
                        f.version += 1;
                        f.text = compiled;
                        f.hash = fragment_hash(&f.heading_path, &f.text);
                        c.execute("UPDATE doc_fragments SET text=?2,version=?3,hash=?4,updated_at=?5 WHERE id=?1", rusqlite::params![f.id, f.text, f.version, f.hash, now])?;
                        record_version(c, &f, false, "compiled", &ctx, now)?;
                        f.id
                    }
                    None => insert_fragment(c, &principal, &document, &path, 0, &scope, &compiled, &ctx, "compiled", now)?.id,
                };
                Ok(id)
            })
            .await?)
    }

    /// Append-only notebook timeline of one scope.
    pub async fn notebook_timeline(
        &self,
        grant: &MemoryGrant,
        scope: &str,
    ) -> Result<Vec<(String, String, i64)>> {
        grant.require(scope)?;
        let (principal, scope) = (grant.principal.clone(), scope.to_string());
        Ok(self
            .db
            .read(ReadLane::Interactive, move |c| {
                let document = document_id(&principal, &DocName::Notebook);
                let mut s = c.prepare("SELECT fragment_id,text,recorded_at FROM notebook_timeline WHERE document_id=?1 AND scope_id=?2 ORDER BY id")?;
                let rows = s.query_map(rusqlite::params![document, scope], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }
}

#[allow(clippy::too_many_arguments)]
fn upsert_proposal(
    c: &rusqlite::Connection,
    principal: &str,
    document: &str,
    fragment: Option<&str>,
    path: &[String],
    operation: &str,
    text: &str,
    provenance: &str,
    allow_auto: bool,
    now: i64,
) -> rusqlite::Result<EditOutcome> {
    let normalized = normalize(text);
    let existing: Option<(String, String)> = c
        .query_row("SELECT id,provenance FROM profile_proposals WHERE principal_id=?1 AND document_id=?2 AND normalized=?3 AND operation=?4 AND state='proposed' AND auto_confirmable=?5", rusqlite::params![principal, document, normalized, operation, allow_auto], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    match existing {
        Some((id, sources)) => {
            let mut list: Vec<String> = serde_json::from_str(&sources).unwrap_or_default();
            if !list.iter().any(|s| s == provenance) {
                list.push(provenance.to_string());
            }
            c.execute("UPDATE profile_proposals SET provenance=?2,evidence_count=?3,updated_at=?4 WHERE id=?1", rusqlite::params![id, serde_json::to_string(&list).unwrap(), list.len() as i64, now])?;
            Ok(EditOutcome::Proposed {
                proposal_id: id,
                evidence_count: list.len() as i64,
            })
        }
        None => {
            let id = new_id("prop");
            c.execute("INSERT INTO profile_proposals(id,principal_id,document_id,fragment_id,heading_path,operation,text,normalized,provenance,evidence_count,state,created_at,updated_at,auto_confirmable) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,1,'proposed',?10,?10,?11)", rusqlite::params![id, principal, document, fragment, serde_json::to_string(path).unwrap(), operation, text, normalized, json!([provenance]).to_string(), now, allow_auto])?;
            Ok(EditOutcome::Proposed {
                proposal_id: id,
                evidence_count: 1,
            })
        }
    }
}

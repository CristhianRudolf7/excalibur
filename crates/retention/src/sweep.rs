//! Synchronous SQL helpers that run inside short writer commands or reader snapshots.
//! Redaction only ever touches rows that provably belong to the subject (its linked runs
//! or exact references to its member IDs), never identifier, key, hash or indexed
//! columns, and never the serialized structure of JSON values.
use aho_corasick::{AhoCorasick, MatchKind};
use excalibur_store::{
    Error as StoreError, Result,
    rusqlite::{self, Connection, OptionalExtension, types::Value as SqlValue},
    schema,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Fixed replacement for purged text inside derived stores.
pub const REDACTION_MARK: &str = "[purged]";
/// Whole subject texts shorter than this are deleted from their base rows but are not
/// searched for in derived stores, so short structural values are never rewritten.
pub const MIN_TEXT_NEEDLE: usize = 12;
/// Sentences/lines at least this long are searched independently to catch partial copies.
pub const MIN_SEGMENT_NEEDLE: usize = 24;
const MAX_NEEDLES: usize = 20_000;
/// Vector object prefixes used by the shared core and the memory crate.
pub const VECTOR_PREFIXES: &[&str] = &[
    "memory",
    "message",
    "msg",
    "chunk",
    "key",
    "document",
    "conversation",
];

pub(crate) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
fn json_escape(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    quoted
        .get(1..quoted.len().saturating_sub(1))
        .unwrap_or("")
        .to_string()
}
pub(crate) fn invalid(reason: &'static str) -> StoreError {
    StoreError::Invalid(reason)
}
pub(crate) fn json_list<I, S>(values: I) -> String
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    Value::Array(
        values
            .into_iter()
            .map(|v| Value::String(v.into()))
            .collect(),
    )
    .to_string()
}

/// Exact texts plus long sentences/lines, longest first, deduplicated.
pub(crate) fn derive_needles(texts: &[String]) -> Vec<String> {
    let mut set = BTreeSet::new();
    for text in texts {
        let text = text.trim();
        if text.chars().count() >= MIN_TEXT_NEEDLE {
            set.insert(text.to_string());
        }
        for segment in text.split(['\n', '\r', '.', '!', '?', ';']) {
            let segment = segment.trim();
            if segment.chars().count() >= MIN_SEGMENT_NEEDLE {
                set.insert(segment.to_string());
            }
        }
    }
    let mut needles: Vec<String> = set.into_iter().collect();
    needles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    needles.truncate(MAX_NEEDLES);
    needles
}

/// Outcome of redacting one stored value.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Redaction {
    Unchanged,
    Changed(String),
    /// A match spans JSON structure (keys, numbers, nesting) rather than a string leaf.
    /// It is left as is and flagged; the serialized document is never rewritten.
    Structured,
}

/// ASCII-case-insensitive multi-pattern matcher over raw and JSON-escaped spellings.
pub(crate) struct Matcher {
    automaton: AhoCorasick,
    patterns: usize,
}
impl Matcher {
    pub(crate) fn new(needles: &[String]) -> Result<Self> {
        let mut patterns = Vec::new();
        for needle in needles {
            patterns.push(needle.clone());
            let escaped = json_escape(needle);
            if escaped != *needle {
                patterns.push(escaped);
            }
        }
        patterns.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        patterns.dedup();
        let automaton = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .ascii_case_insensitive(true)
            .build(&patterns)
            .map_err(|_| invalid("purge needles could not be compiled"))?;
        Ok(Self {
            automaton,
            patterns: patterns.len(),
        })
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.patterns == 0
    }
    pub(crate) fn is_match(&self, bytes: &[u8]) -> bool {
        self.patterns > 0 && self.automaton.is_match(bytes)
    }
    fn replace(&self, text: &str) -> String {
        let marks = vec![REDACTION_MARK; self.patterns];
        self.automaton.replace_all(text, &marks)
    }
    /// Any JSON value is redacted only in its string leaves. Text that looks like JSON
    /// but does not parse is left untouched; other plain text is replaced directly.
    pub(crate) fn redact_value(&self, text: &str) -> Redaction {
        if !self.is_match(text.as_bytes()) {
            return Redaction::Unchanged;
        }
        match serde_json::from_str::<Value>(text) {
            Ok(mut value) => {
                if !self.redact_leaves(&mut value) {
                    return Redaction::Structured;
                }
                match serde_json::to_string(&value) {
                    Ok(out) => Redaction::Changed(out),
                    Err(_) => Redaction::Structured,
                }
            }
            Err(_) => {
                let trimmed = text.trim_start();
                if trimmed.starts_with('{') || trimmed.starts_with('[') {
                    Redaction::Structured
                } else {
                    Redaction::Changed(self.replace(text))
                }
            }
        }
    }
    pub(crate) fn redact_leaves(&self, value: &mut Value) -> bool {
        match value {
            Value::String(text) if self.is_match(text.as_bytes()) => {
                *text = self.replace(text);
                true
            }
            Value::Array(values) => values
                .iter_mut()
                .fold(false, |changed, v| self.redact_leaves(v) | changed),
            Value::Object(map) => map
                .values_mut()
                .fold(false, |changed, v| self.redact_leaves(v) | changed),
            _ => false,
        }
    }
}

/// Names that hold identifiers, keys, hashes or enumerations: never rewritten.
fn protected_name(column: &str) -> bool {
    let lower = column.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "id" | "key"
            | "hash"
            | "checksum"
            | "fingerprint"
            | "kind"
            | "state"
            | "status"
            | "source"
            | "role"
            | "type"
            | "class"
            | "cursor"
            | "sequence"
            | "path"
            | "channel"
            | "layer"
            | "code_version"
    ) || ["_id", "_key", "_hash", "_ids", "_at", "_code", "_version"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

/// A regular table that may hold derived copies of subject text.
#[derive(Clone, Debug)]
pub(crate) struct SweepTable {
    pub name: String,
    pub columns: Vec<String>,
    /// Columns that may be rewritten (not protected, not part of any key or index).
    pub writable: BTreeSet<String>,
    pub run: bool,
    /// Member-reference columns present (`conversation_id`, `message_id`, ...).
    pub members: Vec<String>,
    pub principal: bool,
}
/// Primary content of independent subjects. Those rows are deleted only when they are
/// the purge subject; a copy inside another subject is that subject's own content.
const INDEPENDENT: &[&str] = &[
    "messages",
    "conversations",
    "memories",
    "documents",
    "doc_chunks",
    "document_versions",
    "identity_docs",
    "doc_fragments",
    "doc_fragment_versions",
    "notebook_timeline",
];
/// Structural metadata and the hash-chained audit table.
const STRUCTURAL: &[&str] = &[
    "audit",
    "blobs",
    "blob_refs",
    "blob_integrity",
    "blob_incidents",
    "storage_recoveries",
    "storage_metadata",
    "schema_migrations",
    "vector_indexes",
    "embedding_spaces",
    "short_codes",
    "durable_operations",
    "task_observed_events",
    "journal_payload_redirects",
];
pub(crate) const MEMBER_COLUMNS: &[&str] = &[
    "conversation_id",
    "message_id",
    "document_id",
    "chunk_id",
    "memory_id",
];
fn excluded(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("sqlite_")
        || lower.starts_with("retention_")
        || lower.ends_with("_schema")
        || STRUCTURAL.contains(&lower.as_str())
        || INDEPENDENT.contains(&lower.as_str())
}
pub(crate) fn table_names(c: &Connection) -> Result<Vec<(String, String)>> {
    let mut statement = c.prepare(
        "SELECT name,coalesce(sql,'') FROM sqlite_schema WHERE type='table' ORDER BY name",
    )?;
    let rows = statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
    Ok(rows)
}
fn is_virtual(sql: &str) -> bool {
    sql.trim_start()
        .to_ascii_uppercase()
        .starts_with("CREATE VIRTUAL TABLE")
}
/// Regular tables, excluding virtual tables and their shadow tables.
pub(crate) fn regular_tables(c: &Connection) -> Result<Vec<String>> {
    let all = table_names(c)?;
    let virtuals: Vec<String> = all
        .iter()
        .filter(|(_, sql)| is_virtual(sql))
        .map(|(name, _)| format!("{}_", name.to_ascii_lowercase()))
        .collect();
    Ok(all
        .into_iter()
        .filter(|(name, sql)| {
            let lower = name.to_ascii_lowercase();
            !is_virtual(sql)
                && !lower.starts_with("sqlite_")
                && !virtuals.iter().any(|prefix| lower.starts_with(prefix))
        })
        .map(|(name, _)| name)
        .collect())
}
pub(crate) fn columns(c: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement = c.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
    let rows = statement
        .query_map([table], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(rows)
}
fn key_columns(c: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    let mut statement = c.prepare("SELECT name FROM pragma_table_info(?1) WHERE pk>0")?;
    for name in statement.query_map([table], |r| r.get::<_, String>(0))? {
        keys.insert(name?);
    }
    let indexes: Vec<String> = c
        .prepare("SELECT name FROM pragma_index_list(?1)")?
        .query_map([table], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for index in indexes {
        let mut statement = c.prepare("SELECT name FROM pragma_index_info(?1)")?;
        for name in statement.query_map([index], |r| r.get::<_, Option<String>>(0))? {
            keys.extend(name?);
        }
    }
    let mut statement = c.prepare("SELECT \"from\" FROM pragma_foreign_key_list(?1)")?;
    for name in statement.query_map([table], |r| r.get::<_, String>(0))? {
        keys.insert(name?);
    }
    Ok(keys)
}
pub(crate) fn table_exists(c: &Connection, table: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )?)
}
/// Content tables of external-content FTS5 indexes. Their rows are never rewritten
/// in place: they are either independent content or deleted with the subject.
pub(crate) fn fts_content_tables(c: &Connection) -> Result<BTreeMap<String, Vec<String>>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, sql) in table_names(c)? {
        if is_virtual(&sql)
            && sql.to_ascii_uppercase().contains("USING FTS5")
            && let Some(content) = fts_content(&sql).filter(|c| !c.is_empty())
        {
            out.entry(content.to_ascii_lowercase())
                .or_default()
                .push(name);
        }
    }
    Ok(out)
}
pub(crate) fn sweep_tables(c: &Connection) -> Result<Vec<SweepTable>> {
    let fts = fts_content_tables(c)?;
    let mut out = Vec::new();
    for name in regular_tables(c)? {
        if excluded(&name) || fts.contains_key(&name.to_ascii_lowercase()) {
            continue;
        }
        let columns = columns(c, &name)?;
        let keys = key_columns(c, &name)?;
        let writable = columns
            .iter()
            .filter(|col| !protected_name(col) && !keys.contains(*col))
            .cloned()
            .collect::<BTreeSet<_>>();
        let members: Vec<String> = columns
            .iter()
            .filter(|col| MEMBER_COLUMNS.contains(&col.as_str()))
            .cloned()
            .collect();
        let run = columns.iter().any(|col| col == "run_id");
        if writable.is_empty() || (!run && members.is_empty()) {
            continue;
        }
        out.push(SweepTable {
            principal: columns.iter().any(|col| col == "principal_id"),
            name,
            columns,
            writable,
            run,
            members,
        });
    }
    Ok(out)
}

/// Rows provably owned by the subject: produced by a linked run or referencing a member.
pub(crate) struct Targets<'a> {
    pub runs_json: &'a str,
    pub members_json: &'a str,
    pub principal: &'a str,
}
fn target_condition(table: &SweepTable) -> String {
    let mut parts = Vec::new();
    if table.run {
        parts.push("run_id IN (SELECT value FROM json_each(?2))".to_string());
    }
    for member in &table.members {
        parts.push(format!(
            "{} IN (SELECT value FROM json_each(?3))",
            quote(member)
        ));
    }
    let mut sql = format!(" AND ({})", parts.join(" OR "));
    if table.principal {
        sql.push_str(" AND (principal_id IS NULL OR principal_id=?4)");
    } else {
        sql.push_str(" AND ?4 IS NOT NULL");
    }
    sql
}
fn target_params(targets: &Targets<'_>, first: i64) -> Vec<SqlValue> {
    vec![
        SqlValue::Integer(first),
        SqlValue::Text(targets.runs_json.into()),
        SqlValue::Text(targets.members_json.into()),
        SqlValue::Text(targets.principal.into()),
    ]
}
fn writable_matches(
    table: &SweepTable,
    row: &rusqlite::Row<'_>,
    matcher: &Matcher,
) -> Result<bool> {
    for (index, column) in table.columns.iter().enumerate() {
        if !table.writable.contains(column) {
            continue;
        }
        if let SqlValue::Text(text) = row.get::<_, SqlValue>(index + 1)?
            && matcher.is_match(text.as_bytes())
        {
            return Ok(true);
        }
    }
    Ok(false)
}
/// One page of a targeted scan. Returns matching rowids and the last scanned rowid.
pub(crate) fn scan_page(
    c: &Connection,
    table: &SweepTable,
    targets: &Targets<'_>,
    after: i64,
    limit: u32,
    matcher: &Matcher,
) -> Result<(Vec<i64>, Option<i64>)> {
    let sql = format!(
        "SELECT rowid,* FROM {} WHERE rowid>?1{} ORDER BY rowid LIMIT {limit}",
        quote(&table.name),
        target_condition(table)
    );
    let mut statement = c.prepare(&sql)?;
    let mut rows = statement.query(rusqlite::params_from_iter(target_params(targets, after)))?;
    let (mut hits, mut last) = (Vec::new(), None);
    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        last = Some(rowid);
        if writable_matches(table, row, matcher)? {
            hits.push(rowid);
        }
    }
    Ok((hits, last))
}

/// Remove BEFORE triggers that abort the given mutation; restored by `restore_guards`.
/// Purge is the only sanctioned exception to append-only tables, inside one transaction.
pub(crate) fn lift_guards(c: &Connection, table: &str, event: &str) -> Result<Vec<String>> {
    let mut statement = c.prepare(
        "SELECT name,coalesce(sql,'') FROM sqlite_schema WHERE type='trigger' AND tbl_name=?1 COLLATE NOCASE",
    )?;
    let triggers = statement
        .query_map([table], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let wanted = format!("BEFORE{event}");
    let mut lifted = Vec::new();
    for (name, sql) in triggers {
        let compact: String = sql
            .chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_uppercase)
            .collect();
        if compact.contains(&wanted) && compact.contains("RAISE(") {
            c.execute_batch(&format!("DROP TRIGGER {}", quote(&name)))?;
            lifted.push(sql);
        }
    }
    Ok(lifted)
}
pub(crate) fn restore_guards(c: &Connection, lifted: &[String]) -> Result<()> {
    for sql in lifted {
        c.execute_batch(sql)?;
    }
    Ok(())
}
pub(crate) fn guarded<T>(
    c: &Connection,
    table: &str,
    event: &str,
    work: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let lifted = lift_guards(c, table, event)?;
    let result = work();
    let restored = restore_guards(c, &lifted);
    let value = result?;
    restored?;
    Ok(value)
}
/// Zero freed cells and pages, and remove FTS5 entries immediately instead of leaving
/// delete markers in older segments, for the duration of one purge command.
pub(crate) fn with_secure_delete<T>(
    c: &Connection,
    fts: bool,
    work: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let previous: i64 = c.query_row("PRAGMA secure_delete", [], |r| r.get(0))?;
    c.pragma_update(None, "secure_delete", 1)?;
    let mut indexes = Vec::new();
    let all = if fts {
        fts_content_tables(c)?
    } else {
        BTreeMap::new()
    };
    for tables in all.into_values() {
        for index in tables {
            let config = format!("{index}_config");
            let before: Option<i64> = if table_exists(c, &config)? {
                c.query_row(
                    &format!("SELECT v FROM {} WHERE k='secure-delete'", quote(&config)),
                    [],
                    |r| r.get(0),
                )
                .optional()?
            } else {
                None
            };
            c.execute(
                &format!(
                    "INSERT INTO {q}({q},rank) VALUES('secure-delete',1)",
                    q = quote(&index)
                ),
                [],
            )?;
            indexes.push((index, before.unwrap_or(0)));
        }
    }
    let result = work();
    let mut restored: Result<()> = Ok(());
    for (index, before) in indexes {
        if let Err(error) = c.execute(
            &format!(
                "INSERT INTO {q}({q},rank) VALUES('secure-delete',?1)",
                q = quote(&index)
            ),
            [before],
        ) {
            restored = Err(error.into());
        }
    }
    let pragma = c.pragma_update(None, "secure_delete", previous);
    let value = result?;
    restored?;
    pragma?;
    Ok(value)
}

/// Callback for each redacted row: (table, rowid, changed columns, old values).
pub(crate) type AfterRedaction<'a> =
    dyn FnMut(&Connection, &str, i64, &BTreeMap<String, String>) -> Result<()> + 'a;
#[derive(Debug, Default)]
pub(crate) struct RedactStats {
    pub rows: u64,
    pub structured: u64,
    /// Rows where a match remains inside JSON structure after leaf redaction.
    pub structured_rows: Vec<i64>,
}
/// Rewrite matching text in writable columns of the given targeted rows.
pub(crate) fn redact_rows(
    c: &Connection,
    table: &SweepTable,
    targets: &Targets<'_>,
    rowids: &[i64],
    matcher: &Matcher,
    after: &mut AfterRedaction<'_>,
) -> Result<RedactStats> {
    let mut stats = RedactStats::default();
    if rowids.is_empty() || matcher.is_empty() {
        return Ok(stats);
    }
    let select = format!(
        "SELECT rowid,* FROM {} WHERE rowid=?1{}",
        quote(&table.name),
        target_condition(table)
    );
    guarded(c, &table.name, "UPDATE", || {
        for rowid in rowids {
            let values: Option<Vec<SqlValue>> = {
                let mut statement = c.prepare(&select)?;
                let mut rows =
                    statement.query(rusqlite::params_from_iter(target_params(targets, *rowid)))?;
                match rows.next()? {
                    Some(row) => Some(
                        (1..=table.columns.len())
                            .map(|i| row.get::<_, SqlValue>(i))
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    ),
                    None => None,
                }
            };
            let Some(values) = values else { continue };
            let mut updates = Vec::new();
            let mut old = BTreeMap::new();
            let mut structured = false;
            for (index, value) in values.iter().enumerate() {
                let column = &table.columns[index];
                let SqlValue::Text(text) = value else {
                    continue;
                };
                if !table.writable.contains(column) {
                    continue;
                }
                match matcher.redact_value(text) {
                    Redaction::Unchanged => {}
                    Redaction::Structured => structured = true,
                    Redaction::Changed(new) => {
                        structured |= matcher.is_match(new.as_bytes());
                        old.insert(column.clone(), text.clone());
                        updates.push((column.clone(), new));
                    }
                }
            }
            if structured {
                stats.structured += 1;
                stats.structured_rows.push(*rowid);
            }
            if updates.is_empty() {
                continue;
            }
            let set = updates
                .iter()
                .enumerate()
                .map(|(n, (column, _))| format!("{}=?{}", quote(column), n + 2))
                .collect::<Vec<_>>()
                .join(",");
            let mut params = vec![SqlValue::Integer(*rowid)];
            params.extend(updates.iter().map(|(_, v)| SqlValue::Text(v.clone())));
            c.execute(
                &format!("UPDATE {} SET {set} WHERE rowid=?1", quote(&table.name)),
                rusqlite::params_from_iter(params),
            )?;
            after(c, &table.name, *rowid, &old)?;
            stats.rows += 1;
        }
        Ok(stats)
    })
}

/// Single-column foreign key edge from `child.column` to `parent.parent_column`.
#[derive(Clone, Debug)]
struct FkEdge {
    child: String,
    column: String,
    parent_column: String,
    nullable: bool,
}
pub(crate) struct ForeignKeys {
    edges: BTreeMap<String, Vec<FkEdge>>,
    composite: BTreeSet<String>,
}
impl ForeignKeys {
    pub(crate) fn load(c: &Connection) -> Result<Self> {
        let tables = regular_tables(c)?;
        let mut edges: BTreeMap<String, Vec<FkEdge>> = BTreeMap::new();
        let mut composite = BTreeSet::new();
        for child in &tables {
            let mut statement = c.prepare(
                "SELECT id,seq,\"table\",\"from\",\"to\" FROM pragma_foreign_key_list(?1) ORDER BY id,seq",
            )?;
            let rows = statement
                .query_map([child], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            let mut groups: BTreeMap<i64, Vec<(String, String, Option<String>)>> = BTreeMap::new();
            for (id, _, parent, from, to) in rows {
                groups.entry(id).or_default().push((parent, from, to));
            }
            for (_, group) in groups {
                let parent = group[0].0.to_ascii_lowercase();
                if group.len() > 1 {
                    composite.insert(parent);
                    continue;
                }
                let (_, from, to) = group.into_iter().next().unwrap();
                let parent_column = match to {
                    Some(to) if !to.is_empty() => to,
                    _ => primary_key(c, &parent)?,
                };
                let nullable: bool = c.query_row(
                    "SELECT \"notnull\"=0 FROM pragma_table_info(?1) WHERE name=?2",
                    [child.as_str(), from.as_str()],
                    |r| r.get(0),
                )?;
                edges.entry(parent).or_default().push(FkEdge {
                    child: child.clone(),
                    column: from,
                    parent_column,
                    nullable,
                });
            }
        }
        Ok(Self { edges, composite })
    }
    /// Static plan check: every table reachable from `roots` through required
    /// references must be deletable with single-column keys within the depth bound.
    pub(crate) fn validate(&self, roots: &[&str]) -> Result<()> {
        let mut stack: Vec<(String, u32)> =
            roots.iter().map(|r| (r.to_ascii_lowercase(), 0)).collect();
        let mut seen = BTreeSet::new();
        while let Some((table, depth)) = stack.pop() {
            if depth > 16 {
                return Err(invalid("foreign key cascade exceeds purge depth"));
            }
            if self.composite.contains(&table) {
                return Err(invalid("composite foreign key references a purged table"));
            }
            if !seen.insert(table.clone()) {
                continue;
            }
            for edge in self.edges.get(&table).into_iter().flatten() {
                if !edge.nullable {
                    if edge.child.eq_ignore_ascii_case(&table) {
                        return Err(invalid("required self reference cannot be purged"));
                    }
                    stack.push((edge.child.to_ascii_lowercase(), depth + 1));
                }
            }
        }
        Ok(())
    }
}
fn primary_key(c: &Connection, table: &str) -> Result<String> {
    let mut statement =
        c.prepare("SELECT name FROM pragma_table_info(?1) WHERE pk>0 ORDER BY pk")?;
    let keys = statement
        .query_map([table], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(if keys.len() == 1 {
        keys[0].clone()
    } else {
        "rowid".into()
    })
}
#[derive(Debug, Default)]
pub(crate) struct DeleteStats {
    pub deleted: u64,
    pub nulled: u64,
}
fn sql_to_json(value: SqlValue) -> Option<Value> {
    match value {
        SqlValue::Integer(v) => Some(Value::from(v)),
        SqlValue::Real(v) => serde_json::Number::from_f64(v).map(Value::Number),
        SqlValue::Text(v) => Some(Value::String(v)),
        _ => None,
    }
}
/// Delete rows and, first, every row that cannot exist without them. Nullable references
/// from other rows are cleared instead, so unrelated subjects survive with a broken
/// reference that the tombstone explains.
pub(crate) fn remove_rows(
    c: &Connection,
    keys: &ForeignKeys,
    table: &str,
    column: &str,
    values: &[Value],
    depth: u32,
    stats: &mut DeleteStats,
) -> Result<()> {
    if values.is_empty() {
        return Ok(());
    }
    if depth > 16 {
        return Err(invalid("foreign key cascade exceeds purge depth"));
    }
    let lower = table.to_ascii_lowercase();
    if keys.composite.contains(&lower) {
        return Err(invalid("composite foreign key references a purged table"));
    }
    let list = Value::Array(values.to_vec()).to_string();
    for edge in keys.edges.get(&lower).cloned().unwrap_or_default() {
        let parents: Vec<Value> = {
            let mut statement = c.prepare(&format!(
                "SELECT DISTINCT {} FROM {} WHERE {} IN (SELECT value FROM json_each(?1))",
                quote(&edge.parent_column),
                quote(table),
                quote(column)
            ))?;
            statement
                .query_map([&list], |r| r.get::<_, SqlValue>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter_map(sql_to_json)
                .collect()
        };
        if parents.is_empty() {
            continue;
        }
        let parent_list = Value::Array(parents.clone()).to_string();
        if edge.nullable {
            let nulled = guarded(c, &edge.child, "UPDATE", || {
                Ok(c.execute(
                    &format!(
                        "UPDATE {} SET {col}=NULL WHERE {col} IN (SELECT value FROM json_each(?1))",
                        quote(&edge.child),
                        col = quote(&edge.column)
                    ),
                    [&parent_list],
                )?)
            })?;
            stats.nulled += nulled as u64;
        } else {
            remove_rows(
                c,
                keys,
                &edge.child,
                &edge.column,
                &parents,
                depth + 1,
                stats,
            )?;
        }
    }
    let deleted = guarded(c, table, "DELETE", || {
        Ok(c.execute(
            &format!(
                "DELETE FROM {} WHERE {} IN (SELECT value FROM json_each(?1))",
                quote(table),
                quote(column)
            ),
            [&list],
        )?)
    })?;
    stats.deleted += deleted as u64;
    Ok(())
}

/// Delete vec0 rows by key lookups (no table scans): scoped spaces through their
/// registered index, legacy `vectors_*` tables by object ID. sqlite-vec zeroes the
/// deleted vector bytes in its chunk storage.
pub(crate) fn delete_vectors(c: &Connection, objects: &BTreeSet<(String, String)>) -> Result<u64> {
    let mut removed = 0u64;
    let spaces: Vec<String> = if table_exists(c, "vector_indexes")? {
        c.prepare("SELECT space_id FROM vector_indexes ORDER BY space_id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    } else {
        Vec::new()
    };
    for space in &spaces {
        for (scope, object) in objects {
            removed += schema::delete_vector(c, space, scope, object)? as u64;
        }
    }
    for (name, sql) in table_names(c)? {
        let upper = sql.to_ascii_uppercase();
        if !upper.contains("USING VEC0")
            || !name.starts_with("vectors_")
            || !columns(c, &name)?.iter().any(|c| c == "object_id")
        {
            continue;
        }
        for (_, object) in objects {
            removed += c.execute(
                &format!("DELETE FROM {} WHERE object_id=?1", quote(&name)),
                [object],
            )? as u64;
        }
    }
    Ok(removed)
}
fn fts_content(sql: &str) -> Option<String> {
    let lower = sql.to_ascii_lowercase();
    for (start, _) in lower.match_indices("content") {
        let rest = sql[start + "content".len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next().filter(|q| *q == '\'' || *q == '"') else {
            continue;
        };
        let body = &rest[1..];
        return body.find(quote).map(|end| body[..end].to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn matcher(texts: &[&str]) -> Matcher {
        let texts: Vec<String> = texts.iter().map(|t| t.to_string()).collect();
        Matcher::new(&derive_needles(&texts)).unwrap()
    }
    #[test]
    fn json_redaction_touches_only_string_leaves() {
        let m = matcher(&["The vault code is quartz-falcon-77."]);
        let text = r#"{"state":"running","quote":"user said: The vault code is quartz-falcon-77. ok","n":3}"#;
        let Redaction::Changed(out) = m.redact_value(text) else {
            panic!("expected a change")
        };
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["state"], "running");
        assert_eq!(value["n"], 3);
        assert!(!out.contains("quartz-falcon"));
        assert_eq!(
            m.redact_value(r#"{"state":"running"}"#),
            Redaction::Unchanged
        );
        assert!(derive_needles(&["short".into()]).is_empty());
    }
    #[test]
    fn structural_matches_are_never_rewritten() {
        // The needle is a serialized object; it appears only as JSON structure.
        let m = matcher(&[r#"{"alpha":1,"beta":2}"#]);
        let text = r#"{"call":{"arguments":{"alpha":1,"beta":2}}}"#;
        assert_eq!(m.redact_value(text), Redaction::Structured);
        assert_eq!(
            m.redact_value(r#"{"call": broken {"alpha":1,"beta":2}"#),
            Redaction::Structured
        );
    }
    #[test]
    fn ascii_case_variants_and_escaped_copies_are_matched() {
        let m = matcher(&["line one with \"quotes\"\nline two is longer than needed"]);
        let encoded =
            serde_json::to_string("line one with \"quotes\"\nline two is longer than needed")
                .unwrap();
        assert!(m.is_match(encoded.as_bytes()));
        let m = matcher(&["Remember The Kestrel Vault"]);
        assert!(m.is_match(b"remember the KESTREL vault"));
    }
    #[test]
    fn protected_columns_are_recognized() {
        for column in [
            "id",
            "run_id",
            "idempotency_key",
            "payload_hash",
            "code_version",
            "state",
            "created_at",
        ] {
            assert!(protected_name(column), "{column}");
        }
        for column in ["payload", "content", "text", "result", "metadata"] {
            assert!(!protected_name(column), "{column}");
        }
    }
    #[test]
    fn content_option_is_parsed() {
        assert_eq!(
            fts_content(
                "CREATE VIRTUAL TABLE x USING fts5(text,content='messages',content_rowid='rowid')"
            ),
            Some("messages".into())
        );
        assert_eq!(
            fts_content(
                "CREATE VIRTUAL TABLE files_fts USING fts5(path,content,content='files',content_rowid='rowid',tokenize='trigram')"
            ),
            Some("files".into())
        );
    }
}

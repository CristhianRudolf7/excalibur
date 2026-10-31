//! Synchronous SQL helpers that run inside short writer commands or reader snapshots.
//! Every helper works on the schema actually present, so purge coverage follows the
//! tables other crates add instead of a hand-maintained list of derived copies.
use aho_corasick::{AhoCorasick, MatchKind};
use excalibur_store::{
    Error as StoreError, Result,
    rusqlite::{self, Connection, types::Value as SqlValue},
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

pub(crate) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
fn json_escape(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    quoted[1..quoted.len().saturating_sub(1).max(1)].to_string()
}
pub(crate) fn invalid(reason: &'static str) -> StoreError {
    StoreError::Invalid(reason)
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

/// Multi-pattern matcher over raw and JSON-escaped spellings of every needle.
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
    fn replace(&self, text: &str, mark: &str) -> String {
        let marks = vec![mark; self.patterns];
        self.automaton.replace_all(text, &marks)
    }
    /// JSON documents keep their structure: only string values and keys change.
    pub(crate) fn redact_text(&self, text: &str, mark: &str) -> Option<String> {
        if !self.is_match(text.as_bytes()) {
            return None;
        }
        let trimmed = text.trim_start();
        if (trimmed.starts_with('{') || trimmed.starts_with('['))
            && let Ok(mut value) = serde_json::from_str::<Value>(text)
        {
            self.redact_json(&mut value, mark);
            if let Ok(out) = serde_json::to_string(&value) {
                return Some(if self.is_match(out.as_bytes()) {
                    self.replace(&out, mark)
                } else {
                    out
                });
            }
        }
        Some(self.replace(text, mark))
    }
    pub(crate) fn redact_json(&self, value: &mut Value, mark: &str) -> bool {
        match value {
            Value::String(text) => {
                if self.is_match(text.as_bytes()) {
                    *text = self.replace(text, mark);
                    true
                } else {
                    false
                }
            }
            Value::Array(values) => {
                let mut changed = false;
                for value in values {
                    changed |= self.redact_json(value, mark);
                }
                changed
            }
            Value::Object(map) => {
                let mut changed = false;
                let entries = std::mem::take(map);
                for (key, mut value) in entries {
                    changed |= self.redact_json(&mut value, mark);
                    let key = if self.is_match(key.as_bytes()) {
                        changed = true;
                        self.replace(&key, mark)
                    } else {
                        key
                    };
                    map.insert(key, value);
                }
                changed
            }
            _ => false,
        }
    }
}

/// A regular table that may hold derived copies of subject text.
#[derive(Clone, Debug)]
pub(crate) struct SweepTable {
    pub name: String,
    pub columns: Vec<String>,
    pub scope: bool,
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
/// Structural metadata and hash-chained audit (see the retention documentation).
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
];
fn excluded(name: &str, main: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("sqlite_")
        || lower.starts_with("retention_")
        || lower.ends_with("_schema")
        || STRUCTURAL.contains(&lower.as_str())
        || (main && INDEPENDENT.contains(&lower.as_str()))
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
pub(crate) fn table_exists(c: &Connection, table: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )?)
}
pub(crate) fn sweep_tables(c: &Connection, main: bool) -> Result<Vec<SweepTable>> {
    let mut out = Vec::new();
    for name in regular_tables(c)? {
        if excluded(&name, main) {
            continue;
        }
        let columns = columns(c, &name)?;
        out.push(SweepTable {
            scope: columns.iter().any(|c| c == "scope_id"),
            principal: columns.iter().any(|c| c == "principal_id"),
            columns,
            name,
        });
    }
    Ok(out)
}
pub(crate) fn max_rowid(c: &Connection, table: &str) -> Result<i64> {
    Ok(c.query_row(
        &format!("SELECT coalesce(max(rowid),0) FROM {}", quote(table)),
        [],
        |r| r.get(0),
    )?)
}

/// Scope filter: unscoped rows and rows of the subject's own scopes; never other scopes.
pub(crate) struct Filter<'a> {
    pub scopes_json: &'a str,
    pub principal: &'a str,
}
fn conditions(table: &SweepTable, first: usize) -> (String, usize) {
    let mut sql = String::new();
    let mut next = first;
    if table.scope {
        sql.push_str(&format!(
            " AND (scope_id IS NULL OR scope_id IN (SELECT value FROM json_each(?{next})))"
        ));
        next += 1;
    }
    if table.principal {
        sql.push_str(&format!(
            " AND (principal_id IS NULL OR principal_id=?{next})"
        ));
        next += 1;
    }
    (sql, next)
}
fn bind<'a>(
    table: &SweepTable,
    filter: &'a Filter<'a>,
    mut params: Vec<SqlValue>,
) -> Vec<SqlValue> {
    if table.scope {
        params.push(SqlValue::Text(filter.scopes_json.to_string()));
    }
    if table.principal {
        params.push(SqlValue::Text(filter.principal.to_string()));
    }
    params
}
fn value_matches(value: &SqlValue, matcher: &Matcher) -> bool {
    match value {
        SqlValue::Text(text) => matcher.is_match(text.as_bytes()),
        SqlValue::Blob(bytes) => matcher.is_match(bytes),
        _ => false,
    }
}
/// One page of a scoped scan. Returns matching rowids and the last scanned rowid.
pub(crate) fn scan_page(
    c: &Connection,
    table: &SweepTable,
    filter: &Filter<'_>,
    after: i64,
    limit: u32,
    matcher: &Matcher,
) -> Result<(Vec<i64>, Option<i64>)> {
    let (cond, _) = conditions(table, 2);
    let sql = format!(
        "SELECT rowid,* FROM {} WHERE rowid>?1{cond} ORDER BY rowid LIMIT {limit}",
        quote(&table.name)
    );
    let params = bind(table, filter, vec![SqlValue::Integer(after)]);
    let mut statement = c.prepare(&sql)?;
    let width = statement.column_count();
    let mut rows = statement.query(rusqlite::params_from_iter(params))?;
    let (mut hits, mut last) = (Vec::new(), None);
    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        last = Some(rowid);
        for index in 1..width {
            let value: SqlValue = row.get(index)?;
            if value_matches(&value, matcher) {
                hits.push(rowid);
                break;
            }
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
/// Zero freed cells and pages for the duration of a purge command.
pub(crate) fn with_secure_delete<T>(c: &Connection, work: impl FnOnce() -> Result<T>) -> Result<T> {
    let previous: i64 = c.query_row("PRAGMA secure_delete", [], |r| r.get(0))?;
    c.pragma_update(None, "secure_delete", 1)?;
    let result = work();
    let restored = c.pragma_update(None, "secure_delete", previous);
    let value = result?;
    restored?;
    Ok(value)
}

/// Callback for each redacted row: (table, rowid, changed column names).
pub(crate) type AfterRedaction<'a> =
    dyn FnMut(&Connection, &str, i64, &[String]) -> Result<()> + 'a;

/// Rewrite matching text in the given rows. Rows outside the scope filter are skipped.
pub(crate) fn redact_rows(
    c: &Connection,
    table: &SweepTable,
    filter: &Filter<'_>,
    rowids: &[i64],
    matcher: &Matcher,
    after: &mut AfterRedaction<'_>,
) -> Result<u64> {
    if rowids.is_empty() || matcher.is_empty() {
        return Ok(0);
    }
    let (cond, _) = conditions(table, 2);
    let select = format!("SELECT * FROM {} WHERE rowid=?1{cond}", quote(&table.name));
    guarded(c, &table.name, "UPDATE", || {
        let mut changed_rows = 0;
        for rowid in rowids {
            let params = bind(table, filter, vec![SqlValue::Integer(*rowid)]);
            let values: Option<Vec<SqlValue>> = {
                let mut statement = c.prepare(&select)?;
                let width = statement.column_count();
                let mut rows = statement.query(rusqlite::params_from_iter(params))?;
                match rows.next()? {
                    Some(row) => Some(
                        (0..width)
                            .map(|i| row.get::<_, SqlValue>(i))
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    ),
                    None => None,
                }
            };
            let Some(values) = values else { continue };
            let attempt = |mark: &str| -> Vec<(usize, SqlValue)> {
                let mut updates = Vec::new();
                for (index, value) in values.iter().enumerate() {
                    match value {
                        SqlValue::Text(text) => {
                            if let Some(new) = matcher.redact_text(text, mark) {
                                updates.push((index, SqlValue::Text(new)));
                            }
                        }
                        SqlValue::Blob(bytes) if matcher.is_match(bytes) => {
                            let text = String::from_utf8_lossy(bytes);
                            let new = matcher
                                .redact_text(&text, mark)
                                .unwrap_or_else(|| mark.to_string());
                            updates.push((index, SqlValue::Blob(new.into_bytes())));
                        }
                        _ => {}
                    }
                }
                updates
            };
            let updates = attempt(REDACTION_MARK);
            if updates.is_empty() {
                continue;
            }
            let apply = |updates: &[(usize, SqlValue)]| -> rusqlite::Result<usize> {
                let set = updates
                    .iter()
                    .enumerate()
                    .map(|(n, (index, _))| format!("{}=?{}", quote(&table.columns[*index]), n + 2))
                    .collect::<Vec<_>>()
                    .join(",");
                let mut params = vec![SqlValue::Integer(*rowid)];
                params.extend(updates.iter().map(|(_, v)| v.clone()));
                c.execute(
                    &format!("UPDATE {} SET {set} WHERE rowid=?1", quote(&table.name)),
                    rusqlite::params_from_iter(params),
                )
            };
            let used = match apply(&updates) {
                Ok(_) => updates,
                Err(error)
                    if error.sqlite_error_code()
                        == Some(rusqlite::ErrorCode::ConstraintViolation) =>
                {
                    // A redacted key may collide with another redacted key; keep rows distinct.
                    let unique = attempt(&format!("[purged:{}:{rowid}]", table.name));
                    apply(&unique)?;
                    unique
                }
                Err(error) => return Err(error.into()),
            };
            let names: Vec<String> = used
                .iter()
                .map(|(index, _)| table.columns[*index].clone())
                .collect();
            after(c, &table.name, *rowid, &names)?;
            changed_rows += 1;
        }
        Ok(changed_rows)
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
    pub tables: BTreeSet<String>,
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
            if nulled > 0 {
                stats.nulled += nulled as u64;
                stats.tables.insert(edge.child.clone());
            }
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
    if deleted > 0 {
        stats.deleted += deleted as u64;
        stats.tables.insert(table.to_string());
    }
    Ok(())
}

/// Delete vec0 rows whose object ID is a subject ID or `<prefix>:<subject id>`.
/// sqlite-vec zeroes the deleted vector bytes in its chunk storage.
pub(crate) fn delete_vectors(
    c: &Connection,
    ids: &BTreeSet<String>,
    scopes: &[String],
) -> Result<u64> {
    let mut removed = 0u64;
    for (name, sql) in table_names(c)? {
        if !sql.to_ascii_uppercase().contains("USING VEC0") {
            continue;
        }
        let columns = columns(c, &name)?;
        if !columns.iter().any(|c| c == "object_id") {
            continue;
        }
        let key = if columns.iter().any(|c| c == "vector_key") {
            "vector_key"
        } else {
            "object_id"
        };
        let scoped = columns.iter().any(|c| c == "scope_id");
        let select = if scoped {
            format!("SELECT {key},object_id,scope_id FROM {}", quote(&name))
        } else {
            format!("SELECT {key},object_id,NULL FROM {}", quote(&name))
        };
        let victims: Vec<String> = {
            let mut statement = c.prepare(&select)?;
            statement
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(_, object, scope)| {
                    let tail = object.split_once(':').map_or(object.as_str(), |(_, t)| t);
                    (ids.contains(object) || ids.contains(tail))
                        && scope.as_ref().is_none_or(|s| scopes.contains(s))
                })
                .map(|(key, _, _)| key)
                .collect()
        };
        for victim in victims {
            removed += c.execute(
                &format!("DELETE FROM {} WHERE {key}=?1", quote(&name)),
                [victim],
            )? as u64;
        }
    }
    Ok(removed)
}

/// Rebuild external-content FTS5 indexes over modified tables so deleted terms leave
/// the index segments instead of lingering behind delete markers until a merge.
pub(crate) fn rebuild_fts(c: &Connection, content_tables: &BTreeSet<String>) -> Result<u64> {
    let mut rebuilt = 0;
    for (name, sql) in table_names(c)? {
        let upper = sql.to_ascii_uppercase();
        if !is_virtual(&sql) || !upper.contains("USING FTS5") {
            continue;
        }
        let Some(content) = fts_content(&sql) else {
            continue;
        };
        if content.is_empty() || !content_tables.contains(&content.to_ascii_lowercase()) {
            continue;
        }
        c.execute(
            &format!("INSERT INTO {q}({q}) VALUES('rebuild')", q = quote(&name)),
            [],
        )?;
        rebuilt += 1;
    }
    Ok(rebuilt)
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
    #[test]
    fn json_redaction_keeps_structure_and_short_values() {
        let needles = derive_needles(&["The vault code is quartz-falcon-77.".into()]);
        let matcher = Matcher::new(&needles).unwrap();
        let text = r#"{"state":"running","quote":"user said: The vault code is quartz-falcon-77. ok","n":3}"#;
        let out = matcher.redact_text(text, REDACTION_MARK).unwrap();
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["state"], "running");
        assert_eq!(value["n"], 3);
        assert!(!out.contains("quartz-falcon"));
        assert!(
            matcher
                .redact_text(r#"{"state":"running"}"#, REDACTION_MARK)
                .is_none()
        );
        assert!(derive_needles(&["short".into()]).is_empty());
    }
    #[test]
    fn escaped_copies_are_matched() {
        let needles =
            derive_needles(&["line one with \"quotes\"\nline two is longer than needed".into()]);
        let matcher = Matcher::new(&needles).unwrap();
        let encoded = serde_json::to_string(&needles[0]).unwrap();
        assert!(matcher.is_match(encoded.as_bytes()));
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

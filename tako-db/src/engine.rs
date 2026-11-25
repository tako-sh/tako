//! Native Turso execution and response budgets.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::proto::{Col, DescribeParam, DescribeResult, Error, NamedArg, StmtResult, Value};

#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// How long a statement waits for another connection's write lock.
    pub busy_timeout: Duration,
    /// The longest a single statement may run, including reading its rows.
    pub statement_timeout: Duration,
    /// `PRAGMA synchronous` for client connections (`FULL` or `NORMAL`).
    pub synchronous: Synchronous,
    /// The largest string, blob, or row a statement may produce. Larger
    /// values fail with `SQLITE_TOOBIG` instead of being copied.
    pub max_value_bytes: usize,
    /// Page cache each connection may keep (`PRAGMA cache_size`). Every
    /// stream holds a connection, so this bounds what thousands of them
    /// retain; apps cannot raise it.
    pub cache_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Synchronous {
    Normal,
    Full,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            busy_timeout: Duration::from_secs(5),
            statement_timeout: Duration::from_secs(30),
            synchronous: Synchronous::Full,
            // No larger value fits in a response.
            max_value_bytes: 64 * 1024 * 1024,
            cache_bytes: 1024 * 1024,
        }
    }
}

/// Positional or named statement arguments.
pub enum Args<'a> {
    Positional(&'a [Value]),
    Named(&'a [NamedArg]),
}

/// Bounds the encoded size of one response and, when the response shares a
/// host's buffer budget, reserves its bytes there as they are charged.
pub struct Budget {
    remaining: usize,
    shared: Option<Shared>,
}

/// Bytes reserved from a host's buffer budget for one response.
struct Shared {
    semaphore: Arc<tokio::sync::Semaphore>,
    held: Option<tokio::sync::OwnedSemaphorePermit>,
    /// Reserved but not yet charged.
    spare: usize,
    exhausted: bool,
}

/// Bytes reserved from the host at a time, so small rows do not each take
/// the semaphore.
const SHARED_CHUNK: usize = 64 * 1024;

impl Budget {
    pub fn new(limit: usize) -> Self {
        Self {
            remaining: limit,
            shared: None,
        }
    }

    /// A budget whose charges also reserve bytes from `semaphore`, without
    /// waiting: a charge fails when the host has no bytes left.
    pub(crate) fn shared(limit: usize, semaphore: Arc<tokio::sync::Semaphore>) -> Self {
        Self {
            remaining: limit,
            shared: Some(Shared {
                semaphore,
                held: None,
                spare: 0,
                exhausted: false,
            }),
        }
    }

    pub(crate) fn charge(&mut self, bytes: usize) -> bool {
        let Some(remaining) = self.remaining.checked_sub(bytes) else {
            return false;
        };
        if let Some(shared) = &mut self.shared
            && !shared.reserve(bytes)
        {
            return false;
        }
        self.remaining = remaining;
        true
    }

    /// Charges only this response's own limit, for results that must be
    /// returned although the host has no bytes left.
    pub(crate) fn charge_unshared(&mut self, bytes: usize) -> bool {
        let Some(remaining) = self.remaining.checked_sub(bytes) else {
            return false;
        };
        self.remaining = remaining;
        true
    }

    /// A charge failed because the host had no bytes left, rather than
    /// because this response reached its own limit.
    pub(crate) fn host_exhausted(&self) -> bool {
        self.shared.as_ref().is_some_and(|shared| shared.exhausted)
    }

    /// The bytes reserved from the host for what was charged. Holding the
    /// permit keeps them reserved, until the response has been sent.
    pub(crate) fn into_reservation(self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let mut shared = self.shared?;
        let mut held = shared.held.take()?;
        // Return what was reserved ahead but never charged.
        if shared.spare > 0 {
            drop(held.split(shared.spare));
        }
        Some(held)
    }
}

impl Shared {
    fn reserve(&mut self, bytes: usize) -> bool {
        if self.exhausted {
            return false;
        }
        if let Some(spare) = self.spare.checked_sub(bytes) {
            self.spare = spare;
            return true;
        }
        let need = bytes - self.spare;
        // Ask for a chunk, and settle for exactly what is needed.
        let permit = [need.max(SHARED_CHUNK), need]
            .into_iter()
            .filter_map(|ask| u32::try_from(ask).ok().map(|n| (ask, n)))
            .find_map(|(ask, n)| {
                self.semaphore
                    .clone()
                    .try_acquire_many_owned(n)
                    .ok()
                    .map(|permit| (ask, permit))
            });
        let Some((ask, permit)) = permit else {
            self.exhausted = true;
            return false;
        };
        match &mut self.held {
            Some(held) => held.merge(permit),
            None => self.held = Some(permit),
        }
        self.spare = ask - need;
        true
    }
}

/// Creates an empty database in WAL mode. Fails if the file already exists.
pub fn create_database(path: &Path) -> Result<(), String> {
    crate::mvcc::create_empty_portable(path).map_err(|e| e.to_string())
}

/// Checks that a file is a SQLite database that passes `PRAGMA integrity_check`,
/// and switches it to WAL mode so it can be served.
pub fn verify_database(path: &Path) -> Result<(), String> {
    let conn =
        crate::PortableDatabase::open(path).map_err(|e| format!("cannot open database: {e}"))?;
    let rows = conn
        .query("PRAGMA integrity_check(1)", &[])
        .map_err(|e| format!("integrity check failed: {e}"))?;
    if rows.len() != 1 || rows[0].first().and_then(|v| v.to_text()) != Some("ok") {
        return Err(format!("integrity check failed: {rows:?}"));
    }
    conn.execute_batch("PRAGMA journal_mode=WAL")
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Replaces the database at `path` with the verified file `replacement`.
///
/// The caller must hold the database paused, with every connection in every
/// process closed. The current file is kept as `previous` (a hard link) until
/// the next replacement. The write-ahead log is checkpointed first, so a crash
/// at any step leaves either the old or the new database complete.
pub fn replace_database(path: &Path, replacement: &Path, previous: &Path) -> Result<(), String> {
    if path.exists() {
        crate::PortableDatabase::open(path)
            .and_then(|db| db.checkpoint())
            .map_err(|e| format!("checkpoint before replacing: {e}"))?;
        let _ = std::fs::remove_file(previous);
        std::fs::hard_link(path, previous)
            .map_err(|e| format!("keep the previous database: {e}"))?;
    }
    std::fs::rename(replacement, path).map_err(|e| format!("replace the database: {e}"))?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        match std::fs::remove_file(&sidecar) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove stale {suffix} file: {error}")),
        }
    }
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Exports a private, consistent Raft checkout, including an initial portable image.
pub(crate) fn snapshot_database(source: &Path, dest: &Path) -> Result<(), String> {
    let owner = crate::mvcc::Owner::replica(source).map_err(|e| e.to_string())?;
    crate::mvcc::export_copy(owner, dest).map_err(|e| e.to_string())
}

mod hosted;
pub use hosted::Connection;

/// What a running statement produces: its columns first, then each row.
pub enum Event<'a> {
    Begin(PendingColumns<'a>),
    Row(PendingRow<'a>),
}

/// A statement's columns as SQLite holds them. Like a row's, their size is
/// known before their names are copied.
pub enum PendingColumns<'a> {
    Turso(&'a turso_core::Statement),
}

impl PendingColumns<'_> {
    /// The columns' JSON size, used for the response budget.
    pub fn encoded_size(&self) -> usize {
        match self {
            Self::Turso(stmt) => (0..stmt.num_columns())
                .map(|i| {
                    col_size(
                        Some(&stmt.get_column_name(i)),
                        stmt.get_column_decltype(i).as_deref(),
                    )
                })
                .sum(),
        }
    }

    pub fn to_cols(&self) -> Vec<Col> {
        match self {
            Self::Turso(stmt) => (0..stmt.num_columns())
                .map(|i| Col {
                    name: Some(stmt.get_column_name(i).into_owned()),
                    decltype: stmt.get_column_decltype(i),
                })
                .collect(),
        }
    }
}

/// The JSON size of one column: an upper bound for its fixed fields, and the
/// exact length of its name and declared type.
pub(crate) fn col_size(name: Option<&str>, decltype: Option<&str>) -> usize {
    let text = |text: Option<&str>| text.map_or(4, escaped_len) + 2;
    32 + text(name) + text(decltype)
}

#[derive(Debug, Clone, Copy)]
enum ValueRef<'a> {
    Null,
    Integer(i64),
    Real(f64),
    Text(&'a [u8]),
    Blob(&'a [u8]),
}

/// A row as SQLite holds it. Its size is known before its values are
/// copied, so a sink can refuse it, or reserve memory for it, first.
pub struct PendingRow<'a>(Vec<ValueRef<'a>>);

impl PendingRow<'_> {
    /// The row's JSON size, used for the response budget: an upper bound,
    /// including the row's brackets and the comma after it.
    pub fn encoded_size(&self) -> usize {
        ROW_FRAMING + self.0.iter().copied().map(value_size).sum::<usize>()
    }

    pub fn to_values(&self) -> Vec<Value> {
        self.0.iter().copied().map(from_sql).collect()
    }
}

struct Outcome {
    rows: u64,
    affected: u64,
    last_insert_rowid: Option<i64>,
}

fn check_not_empty(sql: &str) -> Result<(), Error> {
    if sql
        .trim_matches(|c: char| c.is_whitespace() || c == ';')
        .is_empty()
    {
        return Err(Error::new(
            "SQL_PARSE_ERROR",
            "SQL string does not contain any statement",
        ));
    }
    Ok(())
}

/// Whether `sql` is an `INSERT` or `REPLACE`, also after a `WITH` clause.
/// Statements whose effect on the database `total_changes` fully counts:
/// row changes, and transaction control, which changes nothing by itself.
/// Any other statement SQLite does not call read-only (DDL, a writing
/// `PRAGMA`, `VACUUM`) counts as a change.
fn changes_only_rows(verb: &str) -> bool {
    [
        "insert",
        "replace",
        "update",
        "delete",
        "begin",
        "commit",
        "end",
        "rollback",
        "savepoint",
        "release",
    ]
    .iter()
    .any(|candidate| verb.eq_ignore_ascii_case(candidate))
}

fn starts_with_insert(sql: &str) -> bool {
    statement_verb(sql).is_some_and(|verb| {
        verb.eq_ignore_ascii_case("insert") || verb.eq_ignore_ascii_case("replace")
    })
}

/// The keyword a statement starts with, skipping a leading `WITH` clause:
/// `WITH [RECURSIVE] name [(columns)] AS [[NOT] MATERIALIZED] (select), …`.
/// `None` when the text does not have that shape.
fn statement_verb(sql: &str) -> Option<&str> {
    statement_head(sql).map(|(verb, _)| verb)
}

/// The statement's keyword and the tokens after it; see [`statement_verb`].
fn statement_head(sql: &str) -> Option<(&str, SqlTokens<'_>)> {
    let mut tokens = SqlTokens { rest: sql };
    let first = tokens.next()?;
    let SqlToken::Word(first) = first else {
        return None;
    };
    if !first.eq_ignore_ascii_case("with") {
        return Some((first, tokens));
    }
    let mut token = tokens.next()?;
    if matches!(token, SqlToken::Word(word) if word.eq_ignore_ascii_case("recursive")) {
        token = tokens.next()?;
    }
    loop {
        // The table name, then its optional column list.
        if !matches!(token, SqlToken::Word(_) | SqlToken::Quoted(_)) {
            return None;
        }
        token = tokens.next()?;
        if token == SqlToken::Open {
            tokens.skip_group()?;
            token = tokens.next()?;
        }
        if !matches!(token, SqlToken::Word(word) if word.eq_ignore_ascii_case("as")) {
            return None;
        }
        token = tokens.next()?;
        if matches!(token, SqlToken::Word(word) if word.eq_ignore_ascii_case("not")) {
            token = tokens.next()?;
        }
        if matches!(token, SqlToken::Word(word) if word.eq_ignore_ascii_case("materialized")) {
            token = tokens.next()?;
        }
        if token != SqlToken::Open {
            return None;
        }
        tokens.skip_group()?;
        token = tokens.next()?;
        match token {
            SqlToken::Comma => token = tokens.next()?,
            SqlToken::Word(verb) => return Some((verb, tokens)),
            _ => return None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SqlToken<'a> {
    Word(&'a str),
    /// A quoted identifier, without its quotes.
    Quoted(&'a str),
    Open,
    Close,
    Comma,
    Dot,
    Other,
}

/// Splits SQL into the tokens [`statement_verb`] needs, skipping
/// whitespace, comments, and the insides of strings and quoted names.
struct SqlTokens<'a> {
    rest: &'a str,
}

impl<'a> SqlTokens<'a> {
    /// Skips to just past the `)` that closes an open `(`.
    fn skip_group(&mut self) -> Option<()> {
        let mut depth = 1usize;
        while depth > 0 {
            match self.next()? {
                SqlToken::Open => depth += 1,
                SqlToken::Close => depth -= 1,
                _ => {}
            }
        }
        Some(())
    }

    /// Advances past a quoted run that ends at `close`, where a doubled
    /// `close` stands for itself, and returns what was between the quotes.
    fn skip_quoted(&mut self, close: char) -> &'a str {
        let text = self.rest;
        let mut chars = text.char_indices().skip(1).peekable();
        while let Some((index, c)) = chars.next() {
            if c == close {
                if close != ']' && chars.peek().is_some_and(|&(_, next)| next == close) {
                    chars.next();
                    continue;
                }
                self.rest = &text[index + c.len_utf8()..];
                return &text[1..index];
            }
        }
        self.rest = "";
        text.get(1..).unwrap_or_default()
    }
}

impl<'a> Iterator for SqlTokens<'a> {
    type Item = SqlToken<'a>;

    fn next(&mut self) -> Option<SqlToken<'a>> {
        self.rest = skip_comments(self.rest);
        let c = self.rest.chars().next()?;
        let token = match c {
            '(' => SqlToken::Open,
            ')' => SqlToken::Close,
            ',' => SqlToken::Comma,
            '.' => SqlToken::Dot,
            '\'' => {
                self.skip_quoted('\'');
                return Some(SqlToken::Other);
            }
            '"' | '`' | '[' => {
                let name = self.skip_quoted(match c {
                    '[' => ']',
                    other => other,
                });
                return Some(SqlToken::Quoted(name));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let end = self
                    .rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
                    .unwrap_or(self.rest.len());
                let (word, rest) = self.rest.split_at(end);
                self.rest = rest;
                return Some(SqlToken::Word(word));
            }
            _ => SqlToken::Other,
        };
        self.rest = &self.rest[c.len_utf8()..];
        Some(token)
    }
}

/// `sql` after leading whitespace and comments.
fn skip_comments(sql: &str) -> &str {
    let mut rest = sql.trim_start();
    loop {
        if let Some(line) = rest.strip_prefix("--") {
            rest = line
                .split_once('\n')
                .map_or("", |(_, after)| after)
                .trim_start();
        } else if let Some(block) = rest.strip_prefix("/*") {
            rest = block
                .split_once("*/")
                .map_or("", |(_, after)| after)
                .trim_start();
        } else {
            return rest;
        }
    }
}

fn as_value_ref(value: &Value) -> ValueRef<'_> {
    match value {
        Value::Null => ValueRef::Null,
        Value::Integer(value) => ValueRef::Integer(*value),
        Value::Float(value) => ValueRef::Real(*value),
        Value::Text(value) => ValueRef::Text(value.as_bytes()),
        Value::Blob(value) => ValueRef::Blob(value),
    }
}
fn from_sql(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::Integer(value),
        ValueRef::Real(value) => Value::Float(value),
        ValueRef::Text(bytes) => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Value::Blob(bytes.to_vec()),
    }
}

/// A row's brackets and the comma after it.
const ROW_FRAMING: usize = 8;
/// A value's object around its content (`{"type":"blob","base64":""}` is
/// the longest) and the comma after it.
const VALUE_FRAMING: usize = 32;

/// JSON size of a value, used for the response budget: an upper bound, exact
/// for the content of text and blobs. Fixed-size values take at most 24
/// bytes, such as `"-9223372036854775808"` or `-2.2250738585072014e-308`.
fn value_size(value: ValueRef<'_>) -> usize {
    VALUE_FRAMING
        + match value {
            ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => 24,
            ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => escaped_len(text),
                // Without copying: a byte becomes at most a six-byte escape or
                // a three-byte replacement character.
                Err(_) => bytes.len() * 6,
            },
            ValueRef::Blob(blob) => blob.len().div_ceil(3) * 4,
        }
}

/// Length of `text` as a JSON string body: quotes and backslashes take two
/// bytes, control characters two or six.
pub(crate) fn escaped_len(text: &str) -> usize {
    text.len()
        + text
            .bytes()
            .map(|byte| match byte {
                b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 1,
                0x00..=0x1f => 5,
                _ => 0,
            })
            .sum::<usize>()
}

/// PRAGMAs that only read schema or run checks, with or without an argument.
const INTROSPECTION_PRAGMAS: &[&str] = &[
    "collation_list",
    "compile_options",
    "database_list",
    "foreign_key_check",
    "foreign_key_list",
    "freelist_count",
    "function_list",
    "incremental_vacuum",
    "index_info",
    "index_list",
    "index_xinfo",
    "integrity_check",
    "module_list",
    "optimize",
    "page_count",
    "pragma_list",
    "quick_check",
    "table_info",
    "table_list",
    "table_xinfo",
];

/// PRAGMAs that change state of the current connection only. Setting one marks
/// the connection so it is not reused by another stream.
const SESSION_PRAGMAS: &[&str] = &[
    // Read by later ANALYZE runs on the same connection.
    "analysis_limit",
    "automatic_index",
    "case_sensitive_like",
    "cell_size_check",
    "count_changes",
    "defer_foreign_keys",
    "foreign_keys",
    "full_column_names",
    "ignore_check_constraints",
    "legacy_alter_table",
    "max_page_count",
    "query_only",
    "recursive_triggers",
    "reverse_unordered_selects",
    "secure_delete",
    "short_column_names",
];

/// Whether the authorizer refuses transaction statements, and whether it
/// refused one.
pub(crate) const STOP: &str = "TAKO_STOP";

/// PRAGMAs that write a persistent database setting the app owns.
const PERSISTENT_PRAGMAS: &[&str] = &["application_id", "auto_vacuum", "user_version"];

// Preserve the standard result-code names on the Hrana wire.
fn primary_code_name(code: i32) -> &'static str {
    match code {
        1 => "SQLITE_ERROR",
        4 => "SQLITE_ABORT",
        5 => "SQLITE_BUSY",
        6 => "SQLITE_LOCKED",
        8 => "SQLITE_READONLY",
        9 => "SQLITE_INTERRUPT",
        11 => "SQLITE_CORRUPT",
        13 => "SQLITE_FULL",
        14 => "SQLITE_CANTOPEN",
        17 => "SQLITE_SCHEMA",
        18 => "SQLITE_TOOBIG",
        19 => "SQLITE_CONSTRAINT",
        23 => "SQLITE_AUTH",
        26 => "SQLITE_NOTADB",
        _ => "SQLITE_UNKNOWN",
    }
}
fn extended_code_name(code: i32) -> Option<&'static str> {
    Some(match code {
        275 => "SQLITE_CONSTRAINT_CHECK",
        3091 => "SQLITE_CONSTRAINT_DATATYPE",
        787 => "SQLITE_CONSTRAINT_FOREIGNKEY",
        1299 => "SQLITE_CONSTRAINT_NOTNULL",
        1555 => "SQLITE_CONSTRAINT_PRIMARYKEY",
        1811 => "SQLITE_CONSTRAINT_TRIGGER",
        2067 => "SQLITE_CONSTRAINT_UNIQUE",
        _ => return None,
    })
}

#[cfg(test)]
mod tests;

pub(crate) fn replication_error(error: std::io::Error) -> Error {
    Error::new("DATABASE_UNAVAILABLE", error.to_string())
}

//! The SQLite engine behind Tako databases.
//!
//! Everything that depends on `rusqlite` lives in this module. The protocol
//! layer only sees [`Connection`] and protocol values, so the engine can be
//! replaced without touching streams, batons, or HTTP handling.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fallible_iterator::FallibleIterator as _;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::{ToSql, ToSqlOutput, ValueRef};
use rusqlite::{OpenFlags, ffi};

use crate::proto::{Col, DescribeParam, DescribeResult, Error, NamedArg, StmtResult, Value};

/// How often (in virtual machine instructions) the statement deadline is checked.
const PROGRESS_INTERVAL: i32 = 1_000;
/// Prepared statements kept per connection; ORMs repeat a small set of queries.
const STATEMENT_CACHE: usize = 32;
/// Longer statements are prepared without the cache, so its texts take at
/// most 128 KiB per connection, well under SQLite's own page cache; open
/// connections are bounded by the host's stream and pool limits.
const MAX_CACHED_SQL_BYTES: usize = 4 * 1024;

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

/// Caps the memory SQLite uses across every connection in this process:
/// past `bytes`, page caches reuse their pages instead of growing. It is
/// process-wide, so the last host created sets it.
pub fn set_soft_heap_limit(bytes: usize) {
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
    // SAFETY: sqlite3_soft_heap_limit64 takes a plain integer and is safe
    // to call at any time from any thread.
    unsafe {
        ffi::sqlite3_soft_heap_limit64(bytes);
    }
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

/// Empties a database in place: drops every table, view, and trigger and
/// resets `user_version`. Connections other processes hold stay valid and
/// see the new, empty schema.
pub fn reset_database_file(path: &Path) -> Result<(), String> {
    let mut conn = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_EXRESCODE,
    )
    .map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA foreign_keys = OFF")
        .map_err(|e| e.to_string())?;
    let objects = |conn: &rusqlite::Connection,
                   filter: &str|
     -> Result<Vec<(String, String)>, String> {
        let mut statement = conn
            .prepare(&format!(
                "SELECT type, name FROM sqlite_schema WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\' AND {filter}"
            ))
            .map_err(|e| e.to_string())?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())
    };
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    // Virtual tables first: dropping one drops its shadow tables.
    for filter in [
        "type IN ('view', 'trigger')",
        "type = 'table' AND sql LIKE 'CREATE VIRTUAL TABLE%'",
        "type = 'table'",
    ] {
        for (kind, name) in objects(&tx, filter)? {
            let kind = kind.to_uppercase();
            let quoted = name.replace('"', "\"\"");
            tx.execute_batch(&format!("DROP {kind} IF EXISTS \"{quoted}\""))
                .map_err(|e| format!("drop {name}: {e}"))?;
        }
    }
    let sequence: bool = tx
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'sqlite_sequence'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .map_err(|e| e.to_string())?;
    if sequence {
        tx.execute_batch("DELETE FROM sqlite_sequence")
            .map_err(|e| e.to_string())?;
    }
    tx.execute_batch("PRAGMA user_version = 0")
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    // Returns the freed pages; other readers may delay it, which is harmless.
    let _ = conn.execute_batch("VACUUM");
    Ok(())
}

/// Creates an empty database in WAL mode. Fails if the file already exists.
pub fn create_database(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    let conn = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_EXRESCODE,
    )
    .map_err(|e| e.to_string())?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(format!("database refused WAL mode ({mode})"));
    }
    conn.execute_batch("PRAGMA user_version = 0")
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Checks that a file is a SQLite database that passes `PRAGMA integrity_check`,
/// and switches it to WAL mode so it can be served.
pub fn verify_database(path: &Path) -> Result<(), String> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_EXRESCODE,
    )
    .map_err(|e| e.to_string())?;
    let _ = conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA,
        false,
    );
    let result: String = conn
        .query_row("PRAGMA integrity_check(1)", [], |row| row.get(0))
        .map_err(|e| format!("not a readable SQLite database: {e}"))?;
    if result != "ok" {
        return Err(format!("integrity check failed: {result}"));
    }
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(format!("database refused WAL mode ({mode})"));
    }
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
        let conn = rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_EXRESCODE,
        )
        .map_err(|e| e.to_string())?;
        let (busy, _, _): (i64, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|e| format!("checkpoint before replacing: {e}"))?;
        if busy != 0 {
            return Err("the database is still in use; cannot replace it".into());
        }
        drop(conn);
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

/// Writes a compact, transactionally consistent copy of `source` to `dest`
/// with `VACUUM INTO`, then verifies the copy. `dest` must not exist.
pub fn snapshot_database(source: &Path, dest: &Path, busy_timeout: Duration) -> Result<(), String> {
    if dest.exists() {
        return Err(format!("{} already exists", dest.display()));
    }
    let conn = rusqlite::Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_EXRESCODE,
    )
    .map_err(|e| e.to_string())?;
    conn.busy_timeout(busy_timeout).map_err(|e| e.to_string())?;
    let _ = conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA,
        false,
    );
    let dest_text = dest
        .to_str()
        .ok_or_else(|| "snapshot path is not UTF-8".to_string())?;
    conn.execute("VACUUM main INTO ?1", [dest_text])
        .map_err(|e| e.to_string())?;
    drop(conn);
    let copy = rusqlite::Connection::open_with_flags(
        dest,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_EXRESCODE,
    )
    .map_err(|e| e.to_string())?;
    let result: String = copy
        .query_row("PRAGMA integrity_check(1)", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if result != "ok" {
        return Err(format!("snapshot integrity check failed: {result}"));
    }
    Ok(())
}

/// A client connection: one SQLite connection hardened for untrusted SQL.
pub struct Connection {
    conn: rusqlite::Connection,
    epoch: Instant,
    /// Deadline in nanoseconds since `epoch`; zero means none.
    deadline: Arc<AtomicU64>,
    /// Set when a statement changed session state that outlives a stream.
    dirty: Arc<AtomicBool>,
    /// Whether statements may begin or end transactions; see
    /// [`Connection::sequence_in_transaction`].
    transactions: Arc<TransactionControl>,
    /// Set when a statement that may have changed the database ran, whether
    /// or not it succeeded. Cleared by `take_wrote`.
    wrote: bool,
    statement_timeout: Duration,
    /// Whether a statement inserted a row with a given rowid; see
    /// [`RowidWatch`].
    rowids: Arc<RowidWatch>,
}

/// Watches inserts for one rowid. An insert whose rowid equals the previous
/// `last_insert_rowid` leaves that value unchanged, and so does one into a
/// `WITHOUT ROWID` table, which reports no rowid at all; SQLite's update
/// hook runs only for the first, so it tells them apart.
#[derive(Default)]
struct RowidWatch {
    rowid: AtomicI64,
    inserted: AtomicBool,
}

impl Connection {
    pub fn open(path: &Path, config: &EngineConfig) -> Result<Self, Error> {
        let mut conn = rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_EXRESCODE,
        )
        .map_err(map_error)?;
        use rusqlite::config::DbConfig;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
            .map_err(map_error)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)
            .map_err(map_error)?;
        // VACUUM attaches one temporary database; nothing else may be attached.
        conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 1)
            .map_err(map_error)?;
        conn.set_limit(
            Limit::SQLITE_LIMIT_LENGTH,
            i32::try_from(config.max_value_bytes).unwrap_or(i32::MAX),
        )
        .map_err(map_error)?;
        conn.busy_timeout(config.busy_timeout).map_err(map_error)?;
        conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE);
        let synchronous = match config.synchronous {
            Synchronous::Normal => "NORMAL",
            Synchronous::Full => "FULL",
        };
        // A negative cache_size is in KiB.
        let cache_kib = (config.cache_bytes / 1024).max(1);
        conn.execute_batch(&format!(
            "PRAGMA synchronous = {synchronous}; PRAGMA foreign_keys = ON; PRAGMA cache_size = -{cache_kib};"
        ))
        .map_err(map_error)?;

        let dirty = Arc::new(AtomicBool::new(false));
        let transactions = Arc::new(TransactionControl::default());
        let authorizer_dirty = dirty.clone();
        let authorizer_transactions = transactions.clone();
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(ctx.action, AuthAction::Transaction { .. })
                && authorizer_transactions.forbidden.load(Ordering::Relaxed)
            {
                authorizer_transactions
                    .refused
                    .store(true, Ordering::Relaxed);
                return Authorization::Deny;
            }
            authorize(&ctx.action, &authorizer_dirty)
        }))
        .map_err(map_error)?;

        let epoch = Instant::now();
        let deadline = Arc::new(AtomicU64::new(0));
        let progress_deadline = deadline.clone();
        conn.progress_handler(
            PROGRESS_INTERVAL,
            Some(move || {
                let deadline = progress_deadline.load(Ordering::Relaxed);
                deadline != 0 && epoch.elapsed().as_nanos() as u64 > deadline
            }),
        )
        .map_err(map_error)?;

        let rowids = Arc::new(RowidWatch::default());
        let hook_rowids = rowids.clone();
        conn.update_hook(Some(
            move |action: rusqlite::hooks::Action, _: &str, _: &str, rowid: i64| {
                if action == rusqlite::hooks::Action::SQLITE_INSERT
                    && rowid == hook_rowids.rowid.load(Ordering::Relaxed)
                {
                    hook_rowids.inserted.store(true, Ordering::Relaxed);
                }
            },
        ))
        .map_err(map_error)?;

        Ok(Self {
            conn,
            epoch,
            deadline,
            dirty,
            transactions,
            wrote: false,
            statement_timeout: config.statement_timeout,
            rowids,
        })
    }

    pub fn is_autocommit(&self) -> bool {
        self.conn.is_autocommit()
    }

    /// Whether this connection carries session state (from a PRAGMA) that a
    /// later stream must not inherit.
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Whether a statement since the last call may have changed the database,
    /// including one that then failed or was stopped early.
    pub fn take_wrote(&mut self) -> bool {
        std::mem::take(&mut self.wrote)
    }

    /// Rolls back any open transaction.
    pub fn rollback(&mut self) {
        if !self.conn.is_autocommit() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }

    pub fn execute(
        &mut self,
        sql: &str,
        args: &Args<'_>,
        want_rows: bool,
        budget: &mut Budget,
    ) -> Result<StmtResult, Error> {
        let started = Instant::now();
        let mut rows = Vec::new();
        let mut cols = Vec::new();
        let outcome = self.run(sql, args, want_rows, |event| match event {
            Event::Begin(columns) => {
                cols = columns;
                Ok(())
            }
            Event::Row(row) => {
                if want_rows {
                    // Charged before the values are copied out of SQLite.
                    if !budget.charge(row.encoded_size()) {
                        return Err(Error::new(
                            "RESPONSE_TOO_LARGE",
                            "result exceeds the response size limit; use the cursor endpoint or LIMIT",
                        ));
                    }
                    rows.push(row.to_values());
                }
                Ok(())
            }
        })?;
        Ok(StmtResult {
            cols,
            rows,
            affected_row_count: outcome.affected,
            last_insert_rowid: outcome.last_insert_rowid,
            rows_read: outcome.rows,
            rows_written: outcome.affected,
            query_duration_ms: started.elapsed().as_secs_f64() * 1000.0,
        })
    }

    /// Executes a statement, passing its columns and then its rows to `sink`
    /// as they are produced. Returns `(affected_row_count, last_insert_rowid)`.
    pub fn execute_streaming(
        &mut self,
        sql: &str,
        args: &Args<'_>,
        sink: impl FnMut(Event<'_>) -> Result<(), Error>,
    ) -> Result<(u64, Option<i64>), Error> {
        let outcome = self.run(sql, args, true, sink)?;
        Ok((outcome.affected, outcome.last_insert_rowid))
    }

    /// Runs [`Connection::sequence`] inside the caller's open transaction,
    /// refusing `BEGIN`, `COMMIT`, `END`, and `ROLLBACK` when they are
    /// prepared, before they can end it. Returns `Ok(false)` when a statement
    /// was refused; the statements before it ran and remain uncommitted.
    pub(crate) fn sequence_in_transaction(&mut self, sql: &str) -> Result<bool, Error> {
        self.transactions.forbidden.store(true, Ordering::Relaxed);
        self.transactions.refused.store(false, Ordering::Relaxed);
        let result = self.sequence(sql);
        self.transactions.forbidden.store(false, Ordering::Relaxed);
        let refused = self.transactions.refused.swap(false, Ordering::Relaxed);
        match result {
            Err(_) if refused => Ok(false),
            Err(error) => Err(error),
            Ok(()) => Ok(true),
        }
    }

    /// Executes semicolon-separated statements, discarding rows, and stops at
    /// the first failure.
    pub fn sequence(&mut self, sql: &str) -> Result<(), Error> {
        let mut wrote = false;
        let mut batch = rusqlite::Batch::new(&self.conn, sql);
        loop {
            self.arm_deadline();
            let next = batch.next();
            let result = match next {
                Ok(Some(mut stmt)) => {
                    wrote |= !stmt.readonly();
                    let in_transaction = !self.conn.is_autocommit();
                    let mut rows = stmt.raw_query();
                    let result = loop {
                        match rows.next() {
                            Ok(Some(_)) => continue,
                            Ok(None) => break Ok(true),
                            Err(error) => break Err(error),
                        }
                    };
                    drop(rows);
                    wrote |= in_transaction && self.conn.is_autocommit();
                    result
                }
                Ok(None) => Ok(false),
                Err(error) => Err(error),
            };
            self.disarm_deadline();
            self.wrote |= wrote;
            match result {
                Ok(true) => continue,
                Ok(false) => return Ok(()),
                Err(error) => return Err(self.map_run_error(error)),
            }
        }
    }

    pub fn describe(&mut self, sql: &str) -> Result<DescribeResult, Error> {
        check_not_empty(sql)?;
        let (mut cached, mut fresh) = (None, None);
        let stmt = prepare(&self.conn, sql, &mut cached, &mut fresh)?;
        let params = (1..=stmt.parameter_count())
            .map(|index| DescribeParam {
                name: stmt
                    .parameter_name(index)
                    .filter(|name| !name.starts_with('?'))
                    .map(str::to_owned),
            })
            .collect();
        Ok(DescribeResult {
            params,
            cols: columns(stmt),
            is_explain: stmt.is_explain() != 0,
            is_readonly: stmt.readonly(),
        })
    }

    fn run(
        &mut self,
        sql: &str,
        args: &Args<'_>,
        copy_rows: bool,
        mut sink: impl FnMut(Event<'_>) -> Result<(), Error>,
    ) -> Result<Outcome, Error> {
        check_not_empty(sql)?;
        let before_changes = self.conn.total_changes();
        let before_rowid = self.conn.last_insert_rowid();
        self.rowids.rowid.store(before_rowid, Ordering::Relaxed);
        self.rowids.inserted.store(false, Ordering::Relaxed);
        let in_transaction = !self.conn.is_autocommit();
        let (mut cached, mut fresh) = (None, None);
        let stmt = prepare(&self.conn, sql, &mut cached, &mut fresh)?;
        bind(stmt, args)?;
        let readonly = stmt.readonly();
        let inserts = !readonly && starts_with_insert(sql);
        // Before stepping: the statement may write and then fail.
        self.wrote |= !readonly;
        sink(Event::Begin(columns(stmt)))?;
        let width = stmt.column_count();

        self.deadline.store(
            (self.epoch.elapsed() + self.statement_timeout).as_nanos() as u64,
            Ordering::Relaxed,
        );
        let mut count = 0u64;
        let mut failure = None;
        {
            let mut rows = stmt.raw_query();
            loop {
                match rows.next() {
                    Ok(Some(row)) => {
                        count += 1;
                        // Rows nobody reads are counted, not copied.
                        if !copy_rows {
                            continue;
                        }
                        // References into SQLite's row: the sink checks the
                        // size before anything is copied.
                        let mut values = Vec::with_capacity(width);
                        for index in 0..width {
                            match row.get_ref(index) {
                                Ok(value) => values.push(value),
                                Err(error) => {
                                    failure = Some(map_error(error));
                                    break;
                                }
                            }
                        }
                        if failure.is_some() {
                            break;
                        }
                        if let Err(error) = sink(Event::Row(PendingRow(values))) {
                            if error.code != STOP {
                                failure = Some(error);
                            }
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        failure = Some(self.map_timeout(error));
                        break;
                    }
                }
            }
        }
        self.deadline.store(0, Ordering::Relaxed);
        // Reset the statement (returning a cached one to the cache) before
        // reading the transaction state.
        drop((cached, fresh));
        // COMMIT is read-only to SQLite, yet it is what makes earlier writes
        // durable; a snapshot taken mid-transaction must not lose them.
        self.wrote |= in_transaction && self.conn.is_autocommit();
        if let Some(error) = failure {
            return Err(error);
        }

        let changed = self.conn.total_changes() > before_changes;
        let affected = if changed { self.conn.changes() } else { 0 };
        let after_rowid = self.conn.last_insert_rowid();
        // An unchanged rowid counts only when this statement inserted a row
        // with it, not when it inserted into a WITHOUT ROWID table.
        let reinserted = inserts && changed && self.rowids.inserted.load(Ordering::Relaxed);
        let last_insert_rowid = (after_rowid != before_rowid || reinserted).then_some(after_rowid);
        Ok(Outcome {
            rows: count,
            affected,
            last_insert_rowid,
        })
    }

    fn arm_deadline(&self) {
        self.deadline.store(
            (self.epoch.elapsed() + self.statement_timeout).as_nanos() as u64,
            Ordering::Relaxed,
        );
    }

    fn disarm_deadline(&self) {
        self.deadline.store(0, Ordering::Relaxed);
    }

    fn map_run_error(&self, error: rusqlite::Error) -> Error {
        self.map_timeout(error)
    }

    fn map_timeout(&self, error: rusqlite::Error) -> Error {
        if let rusqlite::Error::SqliteFailure(failure, _) = &error
            && failure.code == ffi::ErrorCode::OperationInterrupted
        {
            return Error {
                message: format!(
                    "statement exceeded the {}s time limit",
                    self.statement_timeout.as_secs()
                ),
                code: "SQLITE_INTERRUPT".into(),
                extended_code: None,
            };
        }
        map_error(error)
    }
}

/// What a running statement produces: its columns first, then each row.
pub enum Event<'a> {
    Begin(Vec<Col>),
    Row(PendingRow<'a>),
}

/// A row as SQLite holds it. Its size is known before its values are
/// copied, so a sink can refuse it, or reserve memory for it, first.
pub struct PendingRow<'a>(Vec<ValueRef<'a>>);

impl PendingRow<'_> {
    /// The row's JSON size, used for the response budget.
    pub fn encoded_size(&self) -> usize {
        self.0.iter().copied().map(value_size).sum()
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

fn columns(stmt: &rusqlite::Statement<'_>) -> Vec<Col> {
    stmt.columns()
        .map(|column| Col {
            name: Some(column.name().to_owned()),
            decltype: column.decl_type().map(str::to_owned),
        })
        .collect()
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
fn starts_with_insert(sql: &str) -> bool {
    statement_verb(sql).is_some_and(|verb| {
        verb.eq_ignore_ascii_case("insert") || verb.eq_ignore_ascii_case("replace")
    })
}

/// The keyword a statement starts with, skipping a leading `WITH` clause:
/// `WITH [RECURSIVE] name [(columns)] AS [[NOT] MATERIALIZED] (select), …`.
/// `None` when the text does not have that shape.
fn statement_verb(sql: &str) -> Option<&str> {
    let mut tokens = SqlTokens { rest: sql };
    let first = tokens.next()?;
    let SqlToken::Word(first) = first else {
        return None;
    };
    if !first.eq_ignore_ascii_case("with") {
        return Some(first);
    }
    let mut token = tokens.next()?;
    if matches!(token, SqlToken::Word(word) if word.eq_ignore_ascii_case("recursive")) {
        token = tokens.next()?;
    }
    loop {
        // The table name, then its optional column list.
        if !matches!(token, SqlToken::Word(_) | SqlToken::Quoted) {
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
            SqlToken::Word(verb) => return Some(verb),
            _ => return None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SqlToken<'a> {
    Word(&'a str),
    /// A quoted identifier.
    Quoted,
    Open,
    Close,
    Comma,
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
    /// `close` stands for itself.
    fn skip_quoted(&mut self, close: char) {
        let mut chars = self.rest.char_indices().skip(1).peekable();
        while let Some((index, c)) = chars.next() {
            if c == close {
                if close != ']' && chars.peek().is_some_and(|&(_, next)| next == close) {
                    chars.next();
                    continue;
                }
                self.rest = &self.rest[index + c.len_utf8()..];
                return;
            }
        }
        self.rest = "";
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
            '\'' => {
                self.skip_quoted('\'');
                return Some(SqlToken::Other);
            }
            '"' | '`' | '[' => {
                self.skip_quoted(match c {
                    '[' => ']',
                    other => other,
                });
                return Some(SqlToken::Quoted);
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

/// Prepares `sql`, through the connection's statement cache when it is
/// short enough. The statement lives in whichever slot was used.
fn prepare<'c, 's>(
    conn: &'c rusqlite::Connection,
    sql: &str,
    cached: &'s mut Option<rusqlite::CachedStatement<'c>>,
    fresh: &'s mut Option<rusqlite::Statement<'c>>,
) -> Result<&'s mut rusqlite::Statement<'c>, Error> {
    if sql.len() <= MAX_CACHED_SQL_BYTES {
        let stmt = cached.insert(conn.prepare_cached(sql).map_err(map_error)?);
        Ok(&mut **stmt)
    } else {
        Ok(fresh.insert(conn.prepare(sql).map_err(map_error)?))
    }
}

fn bind(stmt: &mut rusqlite::Statement<'_>, args: &Args<'_>) -> Result<(), Error> {
    let expected = stmt.parameter_count();
    match args {
        Args::Positional(values) => {
            if values.len() != expected {
                return Err(Error::new(
                    "ARGS_INVALID",
                    format!(
                        "statement expects {expected} argument(s) but {} were provided",
                        values.len()
                    ),
                ));
            }
            for (index, value) in values.iter().enumerate() {
                stmt.raw_bind_parameter(index + 1, value)
                    .map_err(map_error)?;
            }
        }
        Args::Named(values) => {
            if values.len() != expected {
                return Err(Error::new(
                    "ARGS_INVALID",
                    format!(
                        "statement expects {expected} argument(s) but {} were provided",
                        values.len()
                    ),
                ));
            }
            for index in 1..=expected {
                if stmt
                    .parameter_name(index)
                    .is_none_or(|name| name.starts_with('?'))
                {
                    return Err(Error::new(
                        "ARGS_INVALID",
                        "named arguments cannot bind positional `?` parameters",
                    ));
                }
            }
            // With as many arguments as parameters, each parameter bound
            // once means every parameter is bound.
            let mut bound = vec![false; expected + 1];
            for NamedArg { name, value } in values.iter() {
                // A prefixed name binds exactly that parameter; a bare one
                // binds whichever of `:name`, `@name` and `$name` exists.
                let index = if name.starts_with([':', '@', '$']) {
                    stmt.parameter_index(name).map_err(map_error)?
                } else {
                    let mut found = None;
                    for prefix in [':', '@', '$'] {
                        found = stmt
                            .parameter_index(&format!("{prefix}{name}"))
                            .map_err(map_error)?;
                        if found.is_some() {
                            break;
                        }
                    }
                    found
                };
                let Some(index) = index else {
                    return Err(Error::new(
                        "ARGS_INVALID",
                        format!("statement has no parameter named `{name}`"),
                    ));
                };
                if std::mem::replace(&mut bound[index], true) {
                    return Err(Error::new(
                        "ARGS_INVALID",
                        format!(
                            "argument `{name}` names a parameter that another argument already binds"
                        ),
                    ));
                }
                stmt.raw_bind_parameter(index, value).map_err(map_error)?;
            }
        }
    }
    Ok(())
}

impl ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Value::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Value::Integer(value) => ToSqlOutput::Borrowed(ValueRef::Integer(*value)),
            Value::Float(value) => ToSqlOutput::Borrowed(ValueRef::Real(*value)),
            Value::Text(value) => ToSqlOutput::Borrowed(ValueRef::Text(value.as_bytes())),
            Value::Blob(value) => ToSqlOutput::Borrowed(ValueRef::Blob(value)),
        })
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

/// JSON size of a value, used for the response budget: exact for text and
/// blobs, an upper bound for the fixed-size values and framing.
fn value_size(value: ValueRef<'_>) -> usize {
    24 + match value {
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
    "analysis_limit",
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
#[derive(Default)]
struct TransactionControl {
    forbidden: AtomicBool,
    refused: AtomicBool,
}

/// Error code a streaming sink returns to stop reading rows without failing.
pub(crate) const STOP: &str = "TAKO_STOP";

/// PRAGMAs that write a persistent database setting the app owns.
const PERSISTENT_PRAGMAS: &[&str] = &["application_id", "auto_vacuum", "user_version"];

fn authorize(action: &AuthAction<'_>, dirty: &AtomicBool) -> Authorization {
    match action {
        // VACUUM attaches an empty-named temporary database. Any other
        // attachment, including `VACUUM INTO '<path>'`, could reach server files.
        AuthAction::Attach { filename } => {
            if filename.is_empty() {
                dirty.store(true, Ordering::Relaxed);
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        // A filename computed at run time, such as a parameter, is unknown
        // when the statement is prepared.
        AuthAction::Unknown { code, .. } if *code == ffi::SQLITE_ATTACH => Authorization::Deny,
        // Temporary objects live on the connection, so it must not be reused
        // by another stream.
        AuthAction::CreateTempIndex { .. }
        | AuthAction::CreateTempTable { .. }
        | AuthAction::CreateTempTrigger { .. }
        | AuthAction::CreateTempView { .. } => {
            dirty.store(true, Ordering::Relaxed);
            Authorization::Allow
        }
        AuthAction::Function { function_name } => {
            if function_name.eq_ignore_ascii_case("load_extension")
                || function_name.eq_ignore_ascii_case("fts3_tokenizer")
            {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } => {
            let name = pragma_name.to_ascii_lowercase();
            if INTROSPECTION_PRAGMAS.contains(&name.as_str()) || pragma_value.is_none() {
                Authorization::Allow
            } else if SESSION_PRAGMAS.contains(&name.as_str()) {
                dirty.store(true, Ordering::Relaxed);
                Authorization::Allow
            } else if PERSISTENT_PRAGMAS.contains(&name.as_str()) {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        _ => Authorization::Allow,
    }
}

/// Maps an engine error to a protocol error with SQLite result code names.
pub fn map_error(error: rusqlite::Error) -> Error {
    match error {
        rusqlite::Error::SqliteFailure(failure, message) => {
            let primary = failure.extended_code & 0xff;
            Error {
                message: message.unwrap_or_else(|| failure.to_string()),
                code: primary_code_name(primary).to_owned(),
                extended_code: extended_code_name(failure.extended_code).map(str::to_owned),
            }
        }
        // Prepare errors with a source offset: syntax errors, but also
        // authorization denials and others with their own result code.
        rusqlite::Error::SqlInputError { error, msg, .. } => {
            let primary = error.extended_code & 0xff;
            if primary == ffi::SQLITE_ERROR {
                Error::new("SQL_PARSE_ERROR", msg)
            } else {
                Error {
                    message: msg,
                    code: primary_code_name(primary).to_owned(),
                    extended_code: extended_code_name(error.extended_code).map(str::to_owned),
                }
            }
        }
        rusqlite::Error::MultipleStatement => Error::new(
            "SQL_MANY_STATEMENTS",
            "SQL string contains more than one statement",
        ),
        rusqlite::Error::InvalidParameterCount(given, expected) => Error::new(
            "ARGS_INVALID",
            format!("statement expects {expected} argument(s) but {given} were provided"),
        ),
        rusqlite::Error::InvalidParameterName(name) => Error::new(
            "ARGS_INVALID",
            format!("statement has no parameter named `{name}`"),
        ),
        rusqlite::Error::NulError(_) => {
            Error::new("SQL_INPUT_ERROR", "SQL text contains a NUL byte")
        }
        rusqlite::Error::Utf8Error(_, _) => Error::new("SQL_INPUT_ERROR", "invalid UTF-8"),
        other => Error::new("SQLITE_ERROR", other.to_string()),
    }
}

fn primary_code_name(code: i32) -> &'static str {
    match code {
        ffi::SQLITE_ERROR => "SQLITE_ERROR",
        ffi::SQLITE_INTERNAL => "SQLITE_INTERNAL",
        ffi::SQLITE_PERM => "SQLITE_PERM",
        ffi::SQLITE_ABORT => "SQLITE_ABORT",
        ffi::SQLITE_BUSY => "SQLITE_BUSY",
        ffi::SQLITE_LOCKED => "SQLITE_LOCKED",
        ffi::SQLITE_NOMEM => "SQLITE_NOMEM",
        ffi::SQLITE_READONLY => "SQLITE_READONLY",
        ffi::SQLITE_INTERRUPT => "SQLITE_INTERRUPT",
        ffi::SQLITE_IOERR => "SQLITE_IOERR",
        ffi::SQLITE_CORRUPT => "SQLITE_CORRUPT",
        ffi::SQLITE_NOTFOUND => "SQLITE_NOTFOUND",
        ffi::SQLITE_FULL => "SQLITE_FULL",
        ffi::SQLITE_CANTOPEN => "SQLITE_CANTOPEN",
        ffi::SQLITE_PROTOCOL => "SQLITE_PROTOCOL",
        ffi::SQLITE_SCHEMA => "SQLITE_SCHEMA",
        ffi::SQLITE_TOOBIG => "SQLITE_TOOBIG",
        ffi::SQLITE_CONSTRAINT => "SQLITE_CONSTRAINT",
        ffi::SQLITE_MISMATCH => "SQLITE_MISMATCH",
        ffi::SQLITE_MISUSE => "SQLITE_MISUSE",
        ffi::SQLITE_NOLFS => "SQLITE_NOLFS",
        ffi::SQLITE_AUTH => "SQLITE_AUTH",
        ffi::SQLITE_RANGE => "SQLITE_RANGE",
        ffi::SQLITE_NOTADB => "SQLITE_NOTADB",
        _ => "SQLITE_UNKNOWN",
    }
}

fn extended_code_name(code: i32) -> Option<&'static str> {
    Some(match code {
        ffi::SQLITE_CONSTRAINT_CHECK => "SQLITE_CONSTRAINT_CHECK",
        ffi::SQLITE_CONSTRAINT_COMMITHOOK => "SQLITE_CONSTRAINT_COMMITHOOK",
        ffi::SQLITE_CONSTRAINT_DATATYPE => "SQLITE_CONSTRAINT_DATATYPE",
        ffi::SQLITE_CONSTRAINT_FOREIGNKEY => "SQLITE_CONSTRAINT_FOREIGNKEY",
        ffi::SQLITE_CONSTRAINT_FUNCTION => "SQLITE_CONSTRAINT_FUNCTION",
        ffi::SQLITE_CONSTRAINT_NOTNULL => "SQLITE_CONSTRAINT_NOTNULL",
        ffi::SQLITE_CONSTRAINT_PINNED => "SQLITE_CONSTRAINT_PINNED",
        ffi::SQLITE_CONSTRAINT_PRIMARYKEY => "SQLITE_CONSTRAINT_PRIMARYKEY",
        ffi::SQLITE_CONSTRAINT_ROWID => "SQLITE_CONSTRAINT_ROWID",
        ffi::SQLITE_CONSTRAINT_TRIGGER => "SQLITE_CONSTRAINT_TRIGGER",
        ffi::SQLITE_CONSTRAINT_UNIQUE => "SQLITE_CONSTRAINT_UNIQUE",
        ffi::SQLITE_CONSTRAINT_VTAB => "SQLITE_CONSTRAINT_VTAB",
        ffi::SQLITE_BUSY_SNAPSHOT => "SQLITE_BUSY_SNAPSHOT",
        ffi::SQLITE_BUSY_RECOVERY => "SQLITE_BUSY_RECOVERY",
        ffi::SQLITE_BUSY_TIMEOUT => "SQLITE_BUSY_TIMEOUT",
        ffi::SQLITE_LOCKED_SHAREDCACHE => "SQLITE_LOCKED_SHAREDCACHE",
        ffi::SQLITE_READONLY_DBMOVED => "SQLITE_READONLY_DBMOVED",
        ffi::SQLITE_ABORT_ROLLBACK => "SQLITE_ABORT_ROLLBACK",
        ffi::SQLITE_AUTH_USER => "SQLITE_AUTH_USER",
        _ => return None,
    })
}

#[cfg(test)]
mod tests;

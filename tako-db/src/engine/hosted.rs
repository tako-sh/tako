use super::*;
use crate::mvcc::Owner;
use std::num::NonZeroUsize;
use turso_core::{Dialect, LimboError, Numeric, Statement};
use turso_parser::ast::{Cmd, Stmt};
use turso_sqlite::SqliteDialect;

/// Development and replicated bindings share the native Turso execution path.
pub struct Connection {
    backend: Hosted,
}
pub(super) struct Hosted {
    conn: Arc<turso_core::Connection>,
    owner: Arc<Owner>,
    read_only: bool,
    wrote: bool,
    transaction_wrote: bool,
    dirty: std::cell::Cell<bool>,
    max_value_bytes: usize,
    timeout: Duration,
    buffered: Arc<tokio::sync::Semaphore>,
    _slot: Option<crate::mvcc::WriterSlot>,
}
impl Connection {
    #[cfg(test)]
    pub fn open(path: &Path, c: &EngineConfig) -> Result<Self, Error> {
        Self::development(
            path,
            c,
            false,
            Arc::new(tokio::sync::Semaphore::new(64 << 20)),
        )
    }
    pub(crate) fn development(
        path: &Path,
        config: &EngineConfig,
        read_only: bool,
        buffered: Arc<tokio::sync::Semaphore>,
    ) -> Result<Self, Error> {
        let owner = Owner::local(path, config.busy_timeout).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                Error::new("SQLITE_BUSY", e.to_string())
            } else {
                Error::new("SQLITE_CANTOPEN", e.to_string())
            }
        })?;
        Self::hosted(owner, config, read_only, buffered)
    }
    pub(crate) fn hosted(
        owner: Arc<Owner>,
        config: &EngineConfig,
        read_only: bool,
        buffered: Arc<tokio::sync::Semaphore>,
    ) -> Result<Self, Error> {
        let slot =
            if read_only {
                None
            } else {
                Some(owner.writer_slot(config.busy_timeout).ok_or_else(|| {
                    Error::new("SQLITE_BUSY", "database writer connections are busy")
                })?)
            };
        let conn = owner.connect().map_err(replication_error)?;
        conn.set_busy_timeout(config.busy_timeout);
        conn.set_query_timeout(config.statement_timeout);
        let cache_kib = config
            .cache_bytes
            .div_ceil(1024)
            .clamp(1, i32::MAX as usize);
        conn.execute(format!(
            "PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA cache_size=-{cache_kib}"
        ))
        .map_err(map_native)?;
        conn.set_query_only(read_only);
        Ok(Self {
            backend: Hosted {
                conn,
                owner,
                read_only,
                wrote: false,
                transaction_wrote: false,
                dirty: Default::default(),
                max_value_bytes: config.max_value_bytes,
                timeout: config.statement_timeout,
                _slot: slot,
                buffered,
            },
        })
    }
    pub fn is_autocommit(&self) -> bool {
        self.backend.conn.get_auto_commit()
    }
    pub fn is_dirty(&self) -> bool {
        self.backend.dirty.get()
    }
    pub fn take_wrote(&mut self) -> bool {
        std::mem::take(&mut self.backend.wrote)
    }
    pub fn rollback(&mut self) {
        if !self.is_autocommit() {
            let _ = self.backend.conn.execute("ROLLBACK");
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
        let outcome = self.run(sql, args, want_rows, false, |event| match event {
            // Charged before the names are copied, as rows are.
            Event::Begin(columns) => {
                if !budget.charge(columns.encoded_size()) {
                    return Err(Error::new(
                        "RESPONSE_TOO_LARGE",
                        "the statement's columns exceed the response size limit",
                    ));
                }
                cols = columns.to_cols();
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
        });
        let outcome = outcome?;
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
        let outcome = self.run(sql, args, true, true, sink)?;
        Ok((outcome.affected, outcome.last_insert_rowid))
    }

    fn run(
        &mut self,
        sql: &str,
        args: &Args<'_>,
        copy_rows: bool,
        streaming: bool,
        sink: impl FnMut(Event<'_>) -> Result<(), Error>,
    ) -> Result<Outcome, Error> {
        let c = &mut self.backend;
        turso_core::Value::with_length_limit(c.max_value_bytes, || {
            c.run(sql, args, copy_rows, streaming, sink)
        })
    }
    pub fn sequence(&mut self, sql: &str) -> Result<(), Error> {
        let c = &mut self.backend;
        turso_core::Value::with_length_limit(c.max_value_bytes, || c.sequence(sql, false))
            .map(|_| ())
    }
    pub(crate) fn sequence_in_transaction(&mut self, sql: &str) -> Result<bool, Error> {
        let c = &mut self.backend;
        turso_core::Value::with_length_limit(c.max_value_bytes, || c.sequence(sql, true))
    }
    pub fn describe(&mut self, sql: &str, budget: &mut Budget) -> Result<DescribeResult, Error> {
        let c = &self.backend;
        c.barrier()?;
        let (stmt, explain) = c.prepare(sql)?;
        let params = (1..=stmt.parameters_count())
            .map(|i| DescribeParam {
                name: stmt
                    .parameters()
                    .name(NonZeroUsize::new(i).unwrap())
                    .filter(|s| !s.starts_with('?')),
            })
            .collect::<Vec<_>>();
        let columns = PendingColumns::Turso(&stmt);
        if !budget.charge(
            columns.encoded_size()
                + params
                    .iter()
                    .map(|p| 18 + p.name.as_deref().map_or(4, escaped_len))
                    .sum::<usize>(),
        ) {
            return Err(too_large());
        }
        Ok(DescribeResult {
            params,
            cols: columns.to_cols(),
            is_explain: explain,
            is_readonly: stmt.get_program().is_readonly(),
        })
    }
}
fn too_large() -> Error {
    Error::new(
        "RESPONSE_TOO_LARGE",
        "statement result exceeds the response size limit",
    )
}
fn map_native(e: LimboError) -> Error {
    if matches!(e, LimboError::LexerError(_)) {
        return Error::new("SQL_PARSE_ERROR", e.to_string());
    }
    Error {
        code: primary_code_name(e.sqlite_result_code()).into(),
        extended_code: extended_code_name(e.sqlite_extended_result_code()).map(str::to_owned),
        message: e.to_string(),
    }
}
impl Hosted {
    fn note_write(&mut self, in_transaction: bool, modified: bool) {
        self.wrote |= modified;
        if self.conn.get_auto_commit() {
            let ended = std::mem::take(&mut self.transaction_wrote);
            self.wrote |= in_transaction && ended;
        } else {
            self.transaction_wrote |= modified;
        }
    }
    fn barrier(&self) -> Result<(), Error> {
        self.owner.barrier().map_err(replication_error)
    }
    fn execution_error(&self, error: LimboError) -> Error {
        if matches!(error, LimboError::CompletionError(_)) {
            self.owner.fence();
        }
        map_native(error)
    }
    fn authorize(&self, cmd: &Cmd) -> Result<(), Error> {
        let stmt = match cmd {
            Cmd::Stmt(stmt) | Cmd::Explain(stmt) | Cmd::ExplainQueryPlan { stmt, .. } => stmt,
        };
        let allowed = match stmt {
            Stmt::Pragma { name, body } => {
                let key = name.name.as_str().to_ascii_lowercase();
                // Some pragmas take effect while they are prepared. Reject
                // persistent setters and maintenance before passing them to Turso.
                if self.read_only
                    && ((body.is_some() && PERSISTENT_PRAGMAS.contains(&key.as_str()))
                        || matches!(key.as_str(), "incremental_vacuum" | "optimize"))
                {
                    return Err(Error::new(
                        "SQLITE_READONLY",
                        "attempt to write a read-only replica",
                    ));
                }
                INTROSPECTION_PRAGMAS.contains(&key.as_str())
                    || (SESSION_PRAGMAS.contains(&key.as_str())
                        && (!self.read_only || body.is_none() || key != "query_only"))
                    || PERSISTENT_PRAGMAS.contains(&key.as_str())
                    || (body.is_none()
                        && matches!(
                            key.as_str(),
                            "journal_mode"
                                | "synchronous"
                                | "cache_size"
                                | "busy_timeout"
                                | "schema_version"
                                | "encoding"
                                | "page_size"
                        ))
            }
            Stmt::Select(_)
            | Stmt::Insert { .. }
            | Stmt::Update(_)
            | Stmt::Delete { .. }
            | Stmt::CreateIndex { .. }
            | Stmt::CreateTable { .. }
            | Stmt::CreateTrigger { .. }
            | Stmt::CreateView { .. }
            | Stmt::DropIndex { .. }
            | Stmt::DropTable { .. }
            | Stmt::DropTrigger { .. }
            | Stmt::DropView { .. }
            | Stmt::AlterTable(_)
            | Stmt::Begin { .. }
            | Stmt::Commit { .. }
            | Stmt::Rollback { .. }
            | Stmt::Release { .. }
            | Stmt::Savepoint { .. }
            | Stmt::Reindex { .. }
            | Stmt::Analyze { .. } => true,
            _ => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(Error::new(
                "SQLITE_AUTH",
                "operation is not available on a hosted database",
            ))
        }
    }
    fn compile(&self, cmd: Cmd, sql: &str) -> Result<Statement, Error> {
        self.authorize(&cmd)?;
        if let Cmd::Stmt(stmt) = &cmd {
            let session_state = match stmt {
                Stmt::Pragma {
                    name,
                    body: Some(_),
                } => SESSION_PRAGMAS.contains(&name.name.as_str().to_ascii_lowercase().as_str()),
                Stmt::CreateTable { temporary, .. }
                | Stmt::CreateView { temporary, .. }
                | Stmt::CreateTrigger { temporary, .. } => *temporary,
                _ => false,
            };
            if session_state {
                self.dirty.set(true);
            }
        }
        let stmt = self
            .conn
            .prepare_translated_cmd(cmd, sql)
            .map_err(|error| match &error {
                LimboError::ParseError(msg)
                    if self.read_only
                        && msg == "Cannot execute write statement in query_only mode" =>
                {
                    Error::new("SQLITE_READONLY", "attempt to write a read-only replica")
                }
                _ => map_native(error),
            })?;
        if self.read_only && !stmt.get_program().is_readonly() {
            return Err(Error::new(
                "SQLITE_READONLY",
                "attempt to write a read-only replica",
            ));
        }
        Ok(stmt)
    }
    fn prepare(&self, sql: &str) -> Result<(Statement, bool), Error> {
        let start = Instant::now();
        check_not_empty(sql)?;
        let (cmd, len) = SqliteDialect.parse(sql).map_err(map_native)?;
        let cmd = cmd.ok_or_else(|| Error::new("SQL_NO_STATEMENT", "SQL contains no statement"))?;
        if SqliteDialect
            .parse(&sql[len..])
            .map_err(map_native)?
            .0
            .is_some()
        {
            return Err(Error::new(
                "SQL_MANY_STATEMENTS",
                "expected one SQL statement",
            ));
        }
        let explain = !matches!(&cmd, Cmd::Stmt(_));
        let mut stmt = self.compile(cmd, &sql[..len])?;
        let remaining = self
            .timeout
            .max(Duration::from_millis(1))
            .checked_sub(start.elapsed())
            .filter(|t| !t.is_zero())
            .ok_or_else(|| Error::new("SQLITE_INTERRUPT", "statement exceeded time limit"))?;
        stmt.set_query_timeout_override(Some(Some(remaining)));
        Ok((stmt, explain))
    }
    fn sequence(&mut self, mut sql: &str, managed: bool) -> Result<bool, Error> {
        let before = self.owner.revision().map_err(replication_error)?;
        let started = Instant::now();
        while !sql.is_empty() {
            let (cmd, len) = SqliteDialect.parse(sql).map_err(map_native)?;
            let Some(cmd) = cmd else { break };
            if managed
                && matches!(
                    cmd,
                    Cmd::Stmt(
                        Stmt::Begin { .. }
                            | Stmt::Commit { .. }
                            | Stmt::Rollback {
                                savepoint_name: None,
                                ..
                            }
                    )
                )
            {
                return Ok(false);
            }
            let mut stmt = self.compile(cmd, &sql[..len])?;
            let remaining = self
                .timeout
                .checked_sub(started.elapsed())
                .filter(|t| !t.is_zero())
                .ok_or_else(|| Error::new("SQLITE_INTERRUPT", "statement exceeded time limit"))?;
            stmt.set_query_timeout_override(Some(Some(remaining)));
            self.wrote |= !stmt.get_program().is_readonly();
            stmt.run_ignore_rows()
                .map_err(|e| self.execution_error(e))?;
            sql = &sql[len..];
        }
        self.owner.confirm(before).map_err(replication_error)?;
        Ok(true)
    }
    fn run(
        &mut self,
        sql: &str,
        args: &Args<'_>,
        copy_rows: bool,
        streaming: bool,
        mut sink: impl FnMut(Event<'_>) -> Result<(), Error>,
    ) -> Result<Outcome, Error> {
        let revision = self.owner.revision().map_err(replication_error)?;
        let (mut stmt, _) = self.prepare(sql)?;
        bind_native(&mut stmt, args, self.max_value_bytes)?;
        let readonly = stmt.get_program().is_readonly();
        if streaming && readonly {
            self.barrier()?;
        }
        let defer_rows = streaming && !readonly && self.conn.get_auto_commit();
        let mut deferred = Vec::new();
        let mut deferred_budget = Budget::shared(self.max_value_bytes, self.buffered.clone());
        let before = self.conn.total_changes();
        sink(Event::Begin(PendingColumns::Turso(&stmt)))?;
        let mut count = 0;
        let mut sink_error = None;
        let in_transaction = !self.conn.get_auto_commit();
        let mut discard_rows = false;
        let mut on_row = |row: &turso_core::Row| {
            count += 1;
            if !copy_rows {
                return Ok(());
            }
            let values = row
                .get_values()
                .map(|v| match v {
                    turso_core::Value::Null => ValueRef::Null,
                    turso_core::Value::Numeric(Numeric::Integer(i)) => ValueRef::Integer(*i),
                    turso_core::Value::Numeric(Numeric::Float(f)) => ValueRef::Real((*f).into()),
                    turso_core::Value::Text(t) => ValueRef::Text(t.as_str().as_bytes()),
                    turso_core::Value::Blob(b) => ValueRef::Blob(b.as_ref()),
                })
                .collect::<Vec<_>>();
            if values.iter().any(
                |v| matches!(v,ValueRef::Text(b)|ValueRef::Blob(b) if b.len()>self.max_value_bytes),
            ) {
                sink_error = Some(Error::new(
                    "SQLITE_TOOBIG",
                    "value exceeds the configured size limit",
                ));
                return Err(LimboError::Interrupt);
            }
            let row = PendingRow(values);
            if defer_rows {
                if !deferred_budget.charge(row.encoded_size()) {
                    sink_error = Some(too_large());
                    return Err(LimboError::Interrupt);
                }
                deferred.push(row.to_values());
            } else if let Err(e) = sink(Event::Row(row)) {
                sink_error = Some(e);
                return Err(LimboError::Interrupt);
            }
            Ok(())
        };
        let pager = self.conn.get_pager();
        let result = loop {
            use turso_core::StepResult;
            match stmt.step_with_structured_errors() {
                Ok(StepResult::Row) => {
                    if !discard_rows
                        && let Err(error) = on_row(stmt.row().expect("statement has a row"))
                    {
                        if readonly {
                            break Err(error);
                        }
                        // RETURNING can exceed a response budget after mutations have started.
                        // Complete the statement and confirm the commit before reporting that error.
                        discard_rows = true;
                    }
                }
                Ok(StepResult::Done) => break Ok(()),
                Ok(StepResult::Interrupt) => break Err(LimboError::Interrupt),
                Ok(StepResult::Busy) => break Err(LimboError::Busy),
                Ok(StepResult::IO | StepResult::Yield | StepResult::Sleep { .. }) => {
                    if let Err(error) = pager.io.step() {
                        break Err(error);
                    }
                }
                Err(error) => break Err(error),
            }
        };
        let modified = self.conn.total_changes() != before
            || (!readonly && !statement_verb(sql).is_some_and(changes_only_rows));
        self.note_write(in_transaction, modified);
        if let Err(error) = result
            && !(readonly && sink_error.is_some())
        {
            return Err(self.execution_error(error));
        }
        if !(streaming && readonly) {
            self.owner.confirm(revision).map_err(replication_error)?;
        }
        if let Some(e) = sink_error
            && !(e.code == STOP && readonly)
        {
            return Err(e);
        }
        for row in deferred {
            if let Err(e) = sink(Event::Row(PendingRow(
                row.iter().map(as_value_ref).collect(),
            ))) {
                if e.code == STOP {
                    break;
                }
                return Err(e);
            }
        }
        let affected = if self.conn.total_changes() != before {
            self.conn.changes().max(0) as u64
        } else {
            0
        };
        let last_insert_rowid = stmt.inserted_rowid().filter(|_| starts_with_insert(sql));
        Ok(Outcome {
            rows: count,
            affected,
            last_insert_rowid,
        })
    }
}
fn bind_native(stmt: &mut Statement, args: &Args<'_>, max_value_bytes: usize) -> Result<(), Error> {
    let n = stmt.parameters_count();
    let len = match args {
        Args::Positional(v) => v.len(),
        Args::Named(v) => v.len(),
    };
    if n != len {
        return Err(Error::new(
            "ARGS_INVALID",
            format!("statement expects {n} argument(s) but {len} were provided"),
        ));
    }
    let mut seen = vec![false; n];
    for i in 0..len {
        let (index, v) = match args {
            Args::Positional(v) => (NonZeroUsize::new(i + 1).unwrap(), &v[i]),
            Args::Named(v) => {
                let NamedArg { name, value } = &v[i];
                let pos = if name.starts_with([':', '@', '$']) {
                    stmt.parameter_index(name)
                } else {
                    [':', '@', '$']
                        .into_iter()
                        .find_map(|p| stmt.parameter_index(&format!("{p}{name}")))
                };
                let pos = pos.filter(|_| !name.starts_with('?')).ok_or_else(|| {
                    Error::new("ARGS_INVALID", format!("invalid named argument {name}"))
                })?;
                if std::mem::replace(&mut seen[pos.get() - 1], true) {
                    return Err(Error::new(
                        "ARGS_INVALID",
                        format!("another argument already binds {name}"),
                    ));
                }
                (pos, value)
            }
        };
        if matches!(v, Value::Text(t) if t.len() > max_value_bytes)
            || matches!(v, Value::Blob(b) if b.len() > max_value_bytes)
        {
            return Err(map_native(LimboError::TooBig));
        }
        let val = match v {
            Value::Null => turso_core::Value::Null,
            Value::Float(f) => turso_core::Value::from_f64(*f),
            Value::Integer(i) => turso_core::Value::from_i64(*i),
            Value::Text(s) => turso_core::Value::build_text(s.clone()),
            Value::Blob(b) => turso_core::Value::from_slice(b)
                .map_err(|_| Error::new("SQLITE_NOMEM", "could not bind blob"))?,
        };
        stmt.bind_at(index, val).map_err(map_native)?;
    }
    Ok(())
}

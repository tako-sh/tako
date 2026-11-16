//! Executes protocol requests on a stream. Runs on a blocking thread.

use std::collections::HashMap;

use crate::engine::{self, Args, Budget};
use crate::host::{Database, Stream};
use crate::proto::{
    Batch, BatchCond, BatchResult, CursorEntry, Error, Stmt, StmtResult, StreamRequest,
    StreamResponse, StreamResult, Version,
};

/// Limits applied while executing.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub max_response_bytes: usize,
    pub max_stored_sql: usize,
    /// Total length of the SQL texts a stream may store.
    pub max_stored_sql_bytes: usize,
}

/// A failure of the whole HTTP request (spec section 5.3).
#[derive(Debug)]
pub(crate) struct ProtocolError(pub String);

pub(crate) fn validate_pipeline(
    requests: &[StreamRequest],
    version: Version,
    max_depth: usize,
) -> Result<(), ProtocolError> {
    for (index, request) in requests.iter().enumerate() {
        if matches!(request, StreamRequest::Close) && index + 1 != requests.len() {
            return Err(ProtocolError(
                "a `close` request must be the last request of the pipeline".into(),
            ));
        }
        if version == Version::V2 && request.requires_v3() {
            return Err(ProtocolError(
                "`get_autocommit` and `is_autocommit` require /v3/pipeline".into(),
            ));
        }
        if let StreamRequest::Batch { batch } = request {
            validate_batch(batch, max_depth)?;
        }
    }
    Ok(())
}

pub(crate) fn validate_batch(batch: &Batch, max_depth: usize) -> Result<(), ProtocolError> {
    for (index, step) in batch.steps.iter().enumerate() {
        if let Some(condition) = &step.condition {
            validate_condition(condition, index, 1, max_depth)?;
        }
    }
    Ok(())
}

fn validate_condition(
    condition: &BatchCond,
    step: usize,
    depth: usize,
    max_depth: usize,
) -> Result<(), ProtocolError> {
    if depth > max_depth {
        return Err(ProtocolError(format!(
            "batch conditions are nested deeper than {max_depth} levels"
        )));
    }
    match condition {
        BatchCond::Ok { step: target } | BatchCond::Error { step: target } => {
            if *target as usize >= step {
                return Err(ProtocolError(format!(
                    "condition of step {step} refers to step {target}, which is not an earlier step"
                )));
            }
        }
        BatchCond::Not { cond } => validate_condition(cond, step, depth + 1, max_depth)?,
        BatchCond::And { conds } | BatchCond::Or { conds } => {
            for cond in conds {
                validate_condition(cond, step, depth + 1, max_depth)?;
            }
        }
        BatchCond::IsAutocommit => {}
    }
    Ok(())
}

pub(crate) struct PipelineOutcome {
    pub results: Vec<StreamResult>,
    pub closed: bool,
}

pub(crate) fn run_pipeline(
    stream: &mut Stream,
    database: &Database,
    requests: Vec<StreamRequest>,
    limits: Limits,
) -> PipelineOutcome {
    let mut budget = Budget::new(limits.max_response_bytes);
    let mut results = Vec::with_capacity(requests.len());
    let mut closed = false;
    for request in requests {
        let result = match request {
            StreamRequest::Close => {
                closed = true;
                Ok(StreamResponse::Close)
            }
            StreamRequest::Execute { stmt } => execute(stream, database, &stmt, &mut budget)
                .map(|result| StreamResponse::Execute { result }),
            StreamRequest::Batch { batch } => Ok(StreamResponse::Batch {
                result: run_batch(stream, database, &batch, &mut budget),
            }),
            StreamRequest::Sequence { sql, sql_id } => {
                sequence(stream, database, sql.as_deref(), sql_id)
                    .map(|()| StreamResponse::Sequence)
            }
            StreamRequest::Describe { sql, sql_id } => {
                describe(stream, database, sql.as_deref(), sql_id)
                    .map(|result| StreamResponse::Describe { result })
            }
            StreamRequest::StoreSql { sql_id, sql } => {
                store_sql(&mut stream.stored_sql, sql_id, sql, &limits)
                    .map(|()| StreamResponse::StoreSql)
            }
            StreamRequest::CloseSql { sql_id } => {
                stream.stored_sql.remove(&sql_id);
                Ok(StreamResponse::CloseSql)
            }
            StreamRequest::GetAutocommit => Ok(StreamResponse::GetAutocommit {
                is_autocommit: is_autocommit(stream),
            }),
        };
        let result = match result {
            Ok(response) => StreamResult::Ok { response },
            Err(error) => StreamResult::Error { error },
        };
        // Rows are charged as they are read; everything else in a result
        // (columns, errors, framing) is charged here, so repeating a stored
        // statement cannot grow the response past its limit.
        let result = if budget.charge(metadata_size(&result)) {
            result
        } else {
            StreamResult::Error {
                error: Error::new(
                    "RESPONSE_TOO_LARGE",
                    "the response exceeds its size limit; send fewer requests at a time",
                ),
            }
        };
        results.push(result);
    }
    PipelineOutcome { results, closed }
}

/// The JSON size of a result apart from its rows: an upper bound for the
/// fixed fields and framing, and the exact length of names and messages.
fn metadata_size(result: &StreamResult) -> usize {
    const FRAMING: usize = 64;
    fn text(text: &Option<String>) -> usize {
        text.as_deref().map_or(4, engine::escaped_len) + 2
    }
    fn cols(cols: &[crate::proto::Col]) -> usize {
        cols.iter()
            .map(|col| 32 + text(&col.name) + text(&col.decltype))
            .sum()
    }
    fn error(error: &Error) -> usize {
        FRAMING
            + engine::escaped_len(&error.message)
            + error.code.len()
            + error.extended_code.as_deref().map_or(0, str::len)
    }
    fn stmt(result: &StmtResult) -> usize {
        // affected_row_count, last_insert_rowid, rows_read, rows_written,
        // query_duration_ms, and the keys around the rows.
        192 + cols(&result.cols)
    }
    FRAMING
        + match result {
            StreamResult::Error { error: e } => error(e),
            StreamResult::Ok { response } => match response {
                StreamResponse::Execute { result } => stmt(result),
                StreamResponse::Batch { result } => {
                    result
                        .step_results
                        .iter()
                        .map(|step| step.as_ref().map_or(8, stmt))
                        .sum::<usize>()
                        + result
                            .step_errors
                            .iter()
                            .map(|step| step.as_ref().map_or(8, error))
                            .sum::<usize>()
                }
                StreamResponse::Describe { result } => {
                    64 + cols(&result.cols)
                        + result
                            .params
                            .iter()
                            .map(|param| 16 + text(&param.name))
                            .sum::<usize>()
                }
                _ => 32,
            },
        }
}

/// The cursor client went away; the stream must be closed.
pub(crate) struct Disconnected;

/// Executes a batch for the cursor endpoint. `emit` returns `false` once the
/// client has disconnected.
pub(crate) fn run_cursor(
    stream: &mut Stream,
    database: &Database,
    batch: &Batch,
    max_row_bytes: usize,
    emit: &mut dyn FnMut(CursorEntry) -> bool,
) -> Result<(), Disconnected> {
    let mut outcomes: Vec<Option<bool>> = Vec::with_capacity(batch.steps.len());
    for (index, step) in batch.steps.iter().enumerate() {
        let step_index = index as u32;
        let runs = step
            .condition
            .as_ref()
            .is_none_or(|condition| evaluate(condition, &outcomes, is_autocommit(stream)));
        if !runs {
            outcomes.push(None);
            continue;
        }
        let mut disconnected = false;
        let result = {
            let Stream { conn, stored_sql } = &mut *stream;
            resolve_statement(stored_sql, &step.stmt).and_then(|(sql, args)| {
                let conn = connection(conn, database)?;
                conn.execute_streaming(sql, &args, |event| {
                    let entry = match event {
                        engine::Event::Begin(cols) => CursorEntry::StepBegin {
                            step: step_index,
                            cols,
                        },
                        engine::Event::Row(row) => {
                            // Rows stream one at a time, but each must still
                            // fit in a response.
                            if engine::encoded_size(&row) > max_row_bytes {
                                return Err(Error::new(
                                    "RESPONSE_TOO_LARGE",
                                    "a row exceeds the response size limit",
                                ));
                            }
                            CursorEntry::Row { row }
                        }
                    };
                    if emit(entry) {
                        Ok(())
                    } else {
                        disconnected = true;
                        Err(Error::new("CLIENT_DISCONNECTED", "client disconnected"))
                    }
                })
            })
        };
        // A statement can write before it fails or its client goes away.
        if stream
            .conn
            .as_mut()
            .is_some_and(engine::Connection::take_wrote)
        {
            database.mark_changed();
        }
        if disconnected {
            return Err(Disconnected);
        }
        let delivered = match result {
            Ok((affected_row_count, last_insert_rowid)) => {
                outcomes.push(Some(true));
                emit(CursorEntry::StepEnd {
                    affected_row_count,
                    last_insert_rowid,
                })
            }
            Err(error) => {
                outcomes.push(Some(false));
                emit(CursorEntry::StepError {
                    step: step_index,
                    error,
                })
            }
        };
        if !delivered {
            return Err(Disconnected);
        }
    }
    if emit(CursorEntry::ReplicationIndex {
        replication_index: None,
    }) {
        Ok(())
    } else {
        Err(Disconnected)
    }
}

fn run_batch(
    stream: &mut Stream,
    database: &Database,
    batch: &Batch,
    budget: &mut Budget,
) -> BatchResult {
    let mut outcomes: Vec<Option<bool>> = Vec::with_capacity(batch.steps.len());
    let mut step_results = Vec::with_capacity(batch.steps.len());
    let mut step_errors = Vec::with_capacity(batch.steps.len());
    for step in &batch.steps {
        let runs = step
            .condition
            .as_ref()
            .is_none_or(|condition| evaluate(condition, &outcomes, is_autocommit(stream)));
        if !runs {
            outcomes.push(None);
            step_results.push(None);
            step_errors.push(None);
            continue;
        }
        match execute(stream, database, &step.stmt, budget) {
            Ok(result) => {
                outcomes.push(Some(true));
                step_results.push(Some(result));
                step_errors.push(None);
            }
            Err(error) => {
                outcomes.push(Some(false));
                step_results.push(None);
                step_errors.push(Some(error));
            }
        }
    }
    BatchResult {
        step_results,
        step_errors,
    }
}

/// `outcomes[n]` is `Some(true)` when step `n` succeeded, `Some(false)` when it
/// failed, and `None` when it was skipped; a skipped step is neither `ok` nor
/// `error`. Indices were validated earlier.
fn evaluate(condition: &BatchCond, outcomes: &[Option<bool>], autocommit: bool) -> bool {
    match condition {
        BatchCond::Ok { step } => outcomes.get(*step as usize).copied().flatten() == Some(true),
        BatchCond::Error { step } => outcomes.get(*step as usize).copied().flatten() == Some(false),
        BatchCond::Not { cond } => !evaluate(cond, outcomes, autocommit),
        BatchCond::And { conds } => conds.iter().all(|c| evaluate(c, outcomes, autocommit)),
        BatchCond::Or { conds } => conds.iter().any(|c| evaluate(c, outcomes, autocommit)),
        BatchCond::IsAutocommit => autocommit,
    }
}

fn is_autocommit(stream: &Stream) -> bool {
    stream
        .conn
        .as_ref()
        .is_none_or(engine::Connection::is_autocommit)
}

fn execute(
    stream: &mut Stream,
    database: &Database,
    stmt: &Stmt,
    budget: &mut Budget,
) -> Result<StmtResult, Error> {
    let Stream { conn, stored_sql } = stream;
    let (sql, args) = resolve_statement(stored_sql, stmt)?;
    let conn = connection(conn, database)?;
    let result = conn.execute(sql, &args, stmt.want_rows.unwrap_or(true), budget);
    // A statement can write before it fails, e.g. `INSERT ... RETURNING`
    // whose rows exceed the response budget.
    if conn.take_wrote() {
        database.mark_changed();
    }
    result
}

fn sequence(
    stream: &mut Stream,
    database: &Database,
    sql: Option<&str>,
    sql_id: Option<i32>,
) -> Result<(), Error> {
    let Stream { conn, stored_sql } = stream;
    let sql = resolve_sql(stored_sql, sql, sql_id)?;
    let conn = connection(conn, database)?;
    let result = conn.sequence(sql);
    // A failing statement may follow ones that already wrote.
    if conn.take_wrote() {
        database.mark_changed();
    }
    result
}

fn describe(
    stream: &mut Stream,
    database: &Database,
    sql: Option<&str>,
    sql_id: Option<i32>,
) -> Result<crate::proto::DescribeResult, Error> {
    let Stream { conn, stored_sql } = stream;
    let sql = resolve_sql(stored_sql, sql, sql_id)?;
    connection(conn, database)?.describe(sql)
}

fn store_sql(
    stored: &mut HashMap<i32, String>,
    sql_id: i32,
    sql: String,
    limits: &Limits,
) -> Result<(), Error> {
    if !stored.contains_key(&sql_id) && stored.len() >= limits.max_stored_sql {
        return Err(Error::new(
            "SQL_STORE_LIMIT",
            format!(
                "a stream can store at most {} SQL texts",
                limits.max_stored_sql
            ),
        ));
    }
    let kept: usize = stored
        .iter()
        .filter(|(id, _)| **id != sql_id)
        .map(|(_, text)| text.len())
        .sum();
    if kept + sql.len() > limits.max_stored_sql_bytes {
        return Err(Error::new(
            "SQL_STORE_LIMIT",
            format!(
                "a stream can store at most {} bytes of SQL text",
                limits.max_stored_sql_bytes
            ),
        ));
    }
    stored.insert(sql_id, sql);
    Ok(())
}

fn resolve_statement<'a>(
    stored: &'a HashMap<i32, String>,
    stmt: &'a Stmt,
) -> Result<(&'a str, Args<'a>), Error> {
    let sql = resolve_sql(stored, stmt.sql.as_deref(), stmt.sql_id)?;
    let args = match (stmt.args.is_empty(), stmt.named_args.is_empty()) {
        (false, false) => {
            return Err(Error::new(
                "ARGS_INVALID",
                "a statement cannot have both positional and named arguments",
            ));
        }
        (true, false) => Args::Named(&stmt.named_args),
        _ => Args::Positional(&stmt.args),
    };
    Ok((sql, args))
}

fn resolve_sql<'a>(
    stored: &'a HashMap<i32, String>,
    sql: Option<&'a str>,
    sql_id: Option<i32>,
) -> Result<&'a str, Error> {
    match (sql, sql_id) {
        (Some(sql), None) => Ok(sql),
        (None, Some(id)) => stored.get(&id).map(String::as_str).ok_or_else(|| {
            Error::new(
                "SQL_NOT_FOUND",
                format!("no SQL text is stored with id {id}"),
            )
        }),
        _ => Err(Error::new(
            "SQL_INPUT_ERROR",
            "exactly one of `sql` and `sql_id` must be set",
        )),
    }
}

fn connection<'a>(
    conn: &'a mut Option<engine::Connection>,
    database: &Database,
) -> Result<&'a mut engine::Connection, Error> {
    if conn.is_none() {
        *conn = Some(database.acquire()?);
    }
    Ok(conn.as_mut().expect("connection was just set"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn requests(value: serde_json::Value) -> Vec<StreamRequest> {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn close_must_be_last() {
        let ok =
            requests(json!([{"type": "execute", "stmt": {"sql": "SELECT 1"}}, {"type": "close"}]));
        assert!(validate_pipeline(&ok, Version::V3, 8).is_ok());
        let bad =
            requests(json!([{"type": "close"}, {"type": "execute", "stmt": {"sql": "SELECT 1"}}]));
        assert!(validate_pipeline(&bad, Version::V3, 8).is_err());
    }

    #[test]
    fn version_two_rejects_version_three_requests() {
        let get = requests(json!([{"type": "get_autocommit"}]));
        assert!(validate_pipeline(&get, Version::V2, 8).is_err());
        assert!(validate_pipeline(&get, Version::V3, 8).is_ok());
    }

    #[test]
    fn conditions_must_refer_to_earlier_steps_within_the_depth_limit() {
        let forward = requests(json!([{"type": "batch", "batch": {"steps": [
            {"condition": {"type": "ok", "step": 0}, "stmt": {"sql": "SELECT 1"}}
        ]}}]));
        assert!(validate_pipeline(&forward, Version::V3, 8).is_err());

        let mut nested = json!({"type": "ok", "step": 0});
        for _ in 0..4 {
            nested = json!({"type": "not", "cond": nested});
        }
        let deep = requests(json!([{"type": "batch", "batch": {"steps": [
            {"stmt": {"sql": "SELECT 1"}},
            {"condition": nested, "stmt": {"sql": "SELECT 2"}}
        ]}}]));
        assert!(validate_pipeline(&deep, Version::V3, 5).is_ok());
        assert!(validate_pipeline(&deep, Version::V3, 4).is_err());
    }

    #[test]
    fn skipped_steps_are_neither_ok_nor_error() {
        let outcomes = [Some(true), Some(false), None];
        let ok = |step| BatchCond::Ok { step };
        let err = |step| BatchCond::Error { step };
        assert!(evaluate(&ok(0), &outcomes, true));
        assert!(!evaluate(&ok(1), &outcomes, true));
        assert!(!evaluate(&ok(2), &outcomes, true));
        assert!(!evaluate(&err(0), &outcomes, true));
        assert!(evaluate(&err(1), &outcomes, true));
        assert!(!evaluate(&err(2), &outcomes, true));
        assert!(evaluate(&BatchCond::IsAutocommit, &outcomes, true));
        assert!(!evaluate(
            &BatchCond::And {
                conds: vec![ok(0), ok(1)]
            },
            &outcomes,
            true
        ));
        assert!(evaluate(
            &BatchCond::Or {
                conds: vec![ok(1), ok(0)]
            },
            &outcomes,
            true
        ));
    }

    #[test]
    fn stored_sql_is_bounded_but_replaceable() {
        let limits = Limits {
            max_response_bytes: usize::MAX,
            max_stored_sql: 1,
            max_stored_sql_bytes: usize::MAX,
        };
        let mut stored = HashMap::new();
        store_sql(&mut stored, 1, "SELECT 1".into(), &limits).unwrap();
        store_sql(&mut stored, 1, "SELECT 2".into(), &limits).unwrap();
        assert_eq!(stored[&1], "SELECT 2");
        assert!(store_sql(&mut stored, 2, "SELECT 3".into(), &limits).is_err());
    }

    #[test]
    fn stored_sql_is_bounded_by_total_length() {
        let limits = Limits {
            max_response_bytes: usize::MAX,
            max_stored_sql: 16,
            max_stored_sql_bytes: 16,
        };
        let mut stored = HashMap::new();
        store_sql(&mut stored, 1, "SELECT 1".into(), &limits).unwrap();
        store_sql(&mut stored, 2, "SELECT 2".into(), &limits).unwrap();
        let error = store_sql(&mut stored, 3, "SELECT 3".into(), &limits).unwrap_err();
        assert_eq!(error.code, "SQL_STORE_LIMIT");
        // Replacing a text only counts its new length.
        store_sql(&mut stored, 2, "SELECT 22".into(), &limits).unwrap_err();
        store_sql(&mut stored, 2, "SELECT 9".into(), &limits).unwrap();
        stored.remove(&2);
        store_sql(&mut stored, 3, "SELECT 3".into(), &limits).unwrap();
    }
}

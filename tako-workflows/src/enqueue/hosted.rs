//! Workflow runs in a Tako-hosted SQLite database, shared by every server
//! that runs the app.
//!
//! Each operation is one request holding one transaction ([`Client::batch`]
//! or [`Client::read_batch`]). SQLite's single writer is never held across
//! network round trips, so no Rust runs between the statements of a
//! transaction. Every decision is made in SQL instead: guarded writes,
//! `INSERT ... SELECT ... WHERE`, upserts and `RETURNING`, and later
//! statements that are no-ops when an earlier guard failed. Rust only reads
//! the returned rows to tell the outcomes apart.
//!
//! Leases are written and judged by the database clock; scheduling and
//! history timestamps use this server's clock. Every table is scoped by
//! `app`.

mod schema;
pub(in crate::enqueue) use schema::batch as schema_batch;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde_json::Value;
use tako_core::{
    EnqueueOpts, EnqueueRunResponse, RunPayload, ScheduleSpec, SignalRunStatus,
    WORKFLOW_RUN_MAX_SIGNALS, WORKFLOW_SIGNAL_INBOX_MAX_BYTES, WORKFLOW_SIGNAL_INBOX_MAX_ENTRIES,
    WorkflowRunStatus,
};
use tako_db::client::{Client, Statement};

use super::operator::{
    CancelledRun, OPERATOR_CANCEL_REASON, RunRecord, RunRow, SignalRow, StepRow, SummaryRow,
    WaitRow, parse_status,
};
use super::{
    Reclaimed, RunResult, RunsDbError, ScheduleRow, WORKER_LOST_ERROR, clamp_lease_ms, now_ms,
};

const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// The database clock in Unix milliseconds. Leases are written and compared
/// with it, never with a server's own clock: servers sharing a store can
/// disagree about the time, and a lease judged by a clock ahead of the one
/// that wrote it is reclaimed while its worker still runs.
const DB_NOW_MS: &str = "CAST(unixepoch('subsec') * 1000 AS INTEGER)";

/// Marks the runs one reclaim takes back until the same transaction settles
/// them. The database clock writes no negative lease.
const RECLAIMING: i64 = i64::MIN;

const TERMINAL: &str = "('succeeded', 'cancelled', 'dead')";

/// In statements about a worker's own run: the run (?2) is still claimed by
/// the caller (?3).
const OWNED: &str = "EXISTS (SELECT 1 FROM tako_workflow_runs
    WHERE app = ?1 AND id = ?2 AND worker_id = ?3 AND status = 'running')";

/// In wait statements: the step (?4) of the run (?2) has a saved result.
const SAVED: &str = "EXISTS (SELECT 1 FROM tako_workflow_steps
    WHERE app = ?1 AND run_id = ?2 AND name = ?4)";

/// The run (?2) whose finishing wakes its waiters, once it is finished.
const FINISHED_RUN: &str = "SELECT id, status, result, last_error FROM tako_workflow_runs
    WHERE app = ?1 AND id = ?2 AND status IN ('succeeded', 'cancelled', 'dead')";

/// Runs a reclaim marked (?2) that have no attempts left, as they end:
/// dead with the lost-worker error (?3).
const LOST_RUNS: &str = "SELECT id, 'dead', result, ?3 FROM tako_workflow_runs
    WHERE app = ?1 AND status = 'running' AND lease_until = ?2 AND attempts >= max_attempts";

type Rows = Vec<Vec<Value>>;

pub(super) struct HostedRunsDb {
    client: Client,
    app: String,
    /// This server's clock, in Unix milliseconds, for scheduling and history
    /// timestamps. Leases use [`DB_NOW_MS`] instead. Tests replace it to
    /// stand in for a server whose clock disagrees with the database.
    clock: fn() -> i64,
}

fn invalid() -> RunsDbError {
    RunsDbError::Storage("invalid or missing hosted workflow state".into())
}

fn decode<T: DeserializeOwned>(row: Vec<Value>) -> Result<T, RunsDbError> {
    serde_json::from_value(Value::Array(row)).map_err(|_| invalid())
}

fn decode_all<T: DeserializeOwned>(rows: Rows) -> Result<Vec<T>, RunsDbError> {
    rows.into_iter().map(decode).collect()
}

fn optional<T: DeserializeOwned>(mut rows: Rows) -> Result<Option<T>, RunsDbError> {
    if rows.len() > 1 {
        return Err(invalid());
    }
    rows.pop().map(decode).transpose()
}

fn one<T: DeserializeOwned>(rows: Rows) -> Result<T, RunsDbError> {
    optional(rows)?.ok_or_else(invalid)
}

/// The rows of each statement of a batch.
fn split<const N: usize>(results: Vec<Rows>) -> Result<[Rows; N], RunsDbError> {
    results.try_into().map_err(|_| invalid())
}

fn json_or_null(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// A statement binding as many of `args` as the highest `?N` it names, so
/// the statements of one operation can share one argument list.
fn stmt(sql: impl Into<String>, args: &[Value]) -> Statement {
    let sql = sql.into();
    let count = parameter_count(&sql).min(args.len());
    Statement::new(sql, args[..count].to_vec())
}

fn parameter_count(sql: &str) -> usize {
    sql.split('?')
        .skip(1)
        .filter_map(|rest| {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            rest[..digits].parse::<usize>().ok()
        })
        .max()
        .unwrap_or(0)
}

/// Wakes every run waiting on the outcome of one of `targets`, a query of
/// `(id, status, result, error)` rows: a wait past its deadline gets `null`,
/// the others the target's outcome as [`RunsDb::result`] reports it. ?1 is
/// the app and ?4 the time; `targets` may use ?2 and ?3.
///
/// [`RunsDb::result`]: super::RunsDb::result
fn wake_run_waiters(targets: &str, args: &[Value]) -> [Statement; 3] {
    let prefix = super::run_event_name("");
    let waiters = format!(
        "t JOIN tako_workflow_event_waiters w
         ON w.app = ?1 AND w.event_name = '{prefix}' || t.id"
    );
    [
        stmt(
            format!(
                "WITH t (id, status, result, error) AS ({targets})
                 INSERT INTO tako_workflow_steps (app, run_id, name, result, completed_at)
                 SELECT w.app, w.run_id, w.step_name,
                     CASE WHEN w.expires_at IS NOT NULL AND w.expires_at <= ?4 THEN 'null'
                     ELSE json_object('status', t.status,
                         'result', json(COALESCE(t.result, 'null')), 'error', t.error)
                     END,
                     ?4
                 FROM {waiters} WHERE true
                 ON CONFLICT DO NOTHING"
            ),
            args,
        ),
        stmt(
            format!(
                "WITH t (id, status, result, error) AS ({targets})
                 UPDATE tako_workflow_runs SET run_at = ?4
                 WHERE app = ?1 AND status = 'pending' AND id IN (
                     SELECT w.run_id FROM {waiters}
                     WHERE w.expires_at IS NULL OR w.expires_at > ?4
                 )"
            ),
            args,
        ),
        stmt(
            format!(
                "WITH t (id, status, result, error) AS ({targets})
                 DELETE FROM tako_workflow_event_waiters
                 WHERE app = ?1 AND event_name IN (SELECT '{prefix}' || id FROM t)"
            ),
            args,
        ),
    ]
}

impl HostedRunsDb {
    pub(super) fn open(url: &str, token: &str, app: &str) -> Result<Self, RunsDbError> {
        Self::open_with_clock(url, token, app, now_ms)
    }

    pub(super) fn open_with_clock(
        url: &str,
        token: &str,
        app: &str,
        clock: fn() -> i64,
    ) -> Result<Self, RunsDbError> {
        let db = Self {
            client: Client::new(url, token)?,
            app: app.to_string(),
            clock,
        };
        let mut init = schema::init();
        init.push(Statement::new(
            "SELECT value FROM tako_workflow_meta WHERE name = 'schema_version'",
            vec![],
        ));
        let results = db.client.batch(&init)?;
        let (version,): (i64,) = one(results.into_iter().last().ok_or_else(invalid)?)?;
        if version > schema::VERSION {
            return Err(RunsDbError::UnsupportedBackend(format!(
                "the hosted workflow store has schema version {version}; this server supports up to {}",
                schema::VERSION
            )));
        }
        Ok(db)
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    /// The app, then `rest`: the arguments of every statement start with it.
    fn args<const N: usize>(&self, rest: [Value; N]) -> Vec<Value> {
        let mut args = Vec::with_capacity(N + 1);
        args.push(self.app.as_str().into());
        args.extend(rest);
        args
    }

    fn query(&self, statement: Statement) -> Result<Rows, RunsDbError> {
        self.client
            .read_batch(&[statement])?
            .pop()
            .ok_or_else(invalid)
    }

    #[cfg(test)]
    pub(super) fn test_rows(&self, sql: &str, args: Vec<Value>) -> Rows {
        self.client
            .batch(&[Statement::new(sql, args)])
            .expect("test statement")
            .pop()
            .unwrap()
    }

    pub(super) fn enqueue(
        &self,
        name: &str,
        payload: &Value,
        opts: &EnqueueOpts,
    ) -> Result<EnqueueRunResponse, RunsDbError> {
        let now = self.now();
        let id = opts.id.clone().unwrap_or_else(|| nanoid::nanoid!());
        let args = self.args([
            id.as_str().into(),
            name.into(),
            serde_json::to_string(payload)?.into(),
            opts.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS).into(),
            opts.run_at_ms.unwrap_or(now).into(),
            now.into(),
        ]);
        // A run's name and payload never change, so the stored ones need no
        // lock. They come back only when they differ as text from this
        // request's; JSON equality decides then.
        let [inserted, differing] = split(self.client.batch(&[
            stmt(
                "INSERT INTO tako_workflow_runs
                 (app, id, name, payload, status, attempts, max_attempts, run_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'pending', 0, ?5, ?6, ?7)
                 ON CONFLICT (app, id) DO NOTHING RETURNING 1",
                &args,
            ),
            stmt(
                "SELECT name, payload FROM tako_workflow_runs
                 WHERE app = ?1 AND id = ?2 AND (name IS NOT ?3 OR payload IS NOT ?4)",
                &args,
            ),
        ])?)?;
        let deduplicated = inserted.is_empty();
        if deduplicated
            && let Some((existing_name, existing_payload)) =
                optional::<(String, String)>(differing)?
        {
            super::check_existing_run(&id, name, payload, &existing_name, &existing_payload)?;
        }
        Ok(EnqueueRunResponse { id, deduplicated })
    }

    pub(super) fn claim(
        &self,
        worker_id: &str,
        names: &[String],
        lease_ms: u64,
    ) -> Result<Option<RunPayload>, RunsDbError> {
        if names.is_empty() {
            return Ok(None);
        }
        let args = self.args([
            worker_id.into(),
            clamp_lease_ms(lease_ms).into(),
            self.now().into(),
            serde_json::to_string(names)?.into(),
        ]);
        // Every statement picks the same candidate: nothing before the claim
        // itself changes which run is due first. Expired waits time out and
        // the checkpoints are read in the claim's transaction.
        let candidate = "(SELECT id FROM tako_workflow_runs
            WHERE app = ?1 AND status = 'pending' AND run_at <= ?4
              AND name IN (SELECT value FROM json_each(?5))
            ORDER BY run_at, rowid LIMIT 1)";
        let [_, _, steps, claimed] = split(self.client.batch(&[
            stmt(
                format!(
                    "INSERT INTO tako_workflow_steps (app, run_id, name, result, completed_at)
                     SELECT app, run_id, step_name, 'null', ?4 FROM tako_workflow_event_waiters
                     WHERE app = ?1 AND run_id = {candidate}
                       AND expires_at IS NOT NULL AND expires_at <= ?4
                     ON CONFLICT DO NOTHING"
                ),
                &args,
            ),
            stmt(
                format!(
                    "DELETE FROM tako_workflow_event_waiters
                     WHERE app = ?1 AND run_id = {candidate}
                       AND expires_at IS NOT NULL AND expires_at <= ?4"
                ),
                &args,
            ),
            stmt(
                format!(
                    "SELECT name, result FROM tako_workflow_steps
                     WHERE app = ?1 AND run_id = {candidate}"
                ),
                &args,
            ),
            stmt(
                format!(
                    "UPDATE tako_workflow_runs
                     SET status = 'running', worker_id = ?2, lease_until = {DB_NOW_MS} + ?3,
                         attempts = attempts + 1
                     WHERE app = ?1 AND id = {candidate}
                     RETURNING id, name, payload, status, attempts, max_attempts, run_at"
                ),
                &args,
            ),
        ])?)?;
        let Some((id, name, payload, status, attempts, max_attempts, run_at)) =
            optional::<(String, String, String, String, i64, i64, i64)>(claimed)?
        else {
            return Ok(None);
        };
        let step_state = decode_all::<(String, String)>(steps)?
            .into_iter()
            .map(|(name, result)| (name, json_or_null(&result)))
            .collect();
        Ok(Some(RunPayload {
            id,
            worker_id: worker_id.to_owned(),
            name,
            payload: json_or_null(&payload),
            status,
            attempts: attempts as u32,
            max_attempts: max_attempts as u32,
            run_at_ms: run_at,
            step_state: Value::Object(step_state),
        }))
    }

    pub(super) fn heartbeat(
        &self,
        id: &str,
        worker_id: &str,
        lease_ms: u64,
    ) -> Result<(), RunsDbError> {
        self.update_running(
            &format!("lease_until = {DB_NOW_MS} + ?4"),
            &self.args([id.into(), worker_id.into(), clamp_lease_ms(lease_ms).into()]),
        )
    }

    pub(super) fn save_step(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        result: &Value,
    ) -> Result<(), RunsDbError> {
        let args = self.args([
            run_id.into(),
            worker_id.into(),
            step_name.into(),
            serde_json::to_string(result)?.into(),
            self.now().into(),
        ]);
        // First write wins; a duplicate save after a lost reply is no error.
        let [_, owned] = split(self.client.batch(&[
            stmt(
                format!(
                    "INSERT INTO tako_workflow_steps (app, run_id, name, result, completed_at)
                     SELECT ?1, ?2, ?4, ?5, ?6 WHERE {OWNED}
                     ON CONFLICT DO NOTHING"
                ),
                &args,
            ),
            stmt(format!("SELECT {OWNED}"), &args),
        ])?)?;
        if one::<(i64,)>(owned)?.0 == 0 {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(())
    }

    pub(super) fn result(&self, name: &str, id: &str) -> Result<Option<RunResult>, RunsDbError> {
        let rows = self.query(stmt(
            "SELECT status, result, last_error FROM tako_workflow_runs
             WHERE app = ?1 AND id = ?2 AND name = ?3",
            &self.args([id.into(), name.into()]),
        ))?;
        optional::<(String, Option<String>, Option<String>)>(rows)?
            .map(|(status, result, error)| {
                Ok(RunResult {
                    status,
                    result: result
                        .map(|value| serde_json::from_str(&value))
                        .transpose()?
                        .unwrap_or(Value::Null),
                    error,
                })
            })
            .transpose()
    }

    pub(super) fn complete(
        &self,
        id: &str,
        worker_id: &str,
        result: &Value,
    ) -> Result<(), RunsDbError> {
        self.finish_running(
            "status = 'succeeded', result = ?5, last_error = NULL, worker_id = NULL,
             lease_until = NULL, finished_at = ?4",
            self.args([
                id.into(),
                worker_id.into(),
                self.now().into(),
                serde_json::to_string(result)?.into(),
            ]),
        )
    }

    pub(super) fn cancel(
        &self,
        id: &str,
        worker_id: &str,
        reason: Option<&str>,
    ) -> Result<(), RunsDbError> {
        self.finish_running(
            "status = 'cancelled', last_error = ?5, worker_id = NULL, lease_until = NULL,
             finished_at = ?4",
            self.args([
                id.into(),
                worker_id.into(),
                self.now().into(),
                reason.into(),
            ]),
        )
    }

    pub(super) fn fail(
        &self,
        id: &str,
        worker_id: &str,
        error: &str,
        next_run_at_ms: Option<i64>,
        finalize: bool,
    ) -> Result<(), RunsDbError> {
        if finalize {
            return self.finish_running(
                "status = 'dead', last_error = ?5, worker_id = NULL, lease_until = NULL,
                 finished_at = ?4",
                self.args([id.into(), worker_id.into(), self.now().into(), error.into()]),
            );
        }
        let next = next_run_at_ms.ok_or_else(|| {
            RunsDbError::Storage("fail(finalize=false) requires next_run_at_ms".into())
        })?;
        self.update_running(
            "status = 'pending', last_error = ?4, worker_id = NULL, lease_until = NULL,
             run_at = ?5",
            &self.args([id.into(), worker_id.into(), error.into(), next.into()]),
        )
    }

    pub(super) fn prune_finished_before(
        &self,
        cutoff_ms: i64,
        limit: u32,
    ) -> Result<u64, RunsDbError> {
        let args = self.args([cutoff_ms.into(), limit.into()]);
        // The order is total, so each statement selects the same runs.
        let ids = "SELECT id FROM tako_workflow_runs
            WHERE app = ?1 AND status IN ('succeeded', 'cancelled', 'dead') AND finished_at <= ?2
            ORDER BY finished_at, id LIMIT ?3";
        let mut statements: Vec<_> = [
            "tako_workflow_steps",
            "tako_workflow_event_waiters",
            "tako_workflow_run_inbox",
        ]
        .into_iter()
        .map(|table| {
            stmt(
                format!("DELETE FROM {table} WHERE app = ?1 AND run_id IN ({ids})"),
                &args,
            )
        })
        .collect();
        statements.push(stmt(
            format!("DELETE FROM tako_workflow_runs WHERE app = ?1 AND id IN ({ids})"),
            &args,
        ));
        statements.push(Statement::new("SELECT changes()", vec![]));
        let results = self.client.batch(&statements)?;
        let (deleted,): (i64,) = one(results.into_iter().last().ok_or_else(invalid)?)?;
        Ok(deleted as u64)
    }

    pub(super) fn defer(
        &self,
        id: &str,
        worker_id: &str,
        wake_at_ms: Option<i64>,
    ) -> Result<(), RunsDbError> {
        self.update_running(
            "status = 'pending', worker_id = NULL, lease_until = NULL, run_at = ?4,
             attempts = attempts - 1",
            &self.args([
                id.into(),
                worker_id.into(),
                wake_at_ms.unwrap_or(i64::MAX).into(),
            ]),
        )
    }

    pub(super) fn reclaim_expired(&self) -> Result<Reclaimed, RunsDbError> {
        let args = self.args([
            RECLAIMING.into(),
            WORKER_LOST_ERROR.into(),
            self.now().into(),
        ]);
        // Mark the expired runs first: the marking statement still sees the
        // owner each lost run had, and the following ones find exactly these
        // runs however the database clock moves meanwhile. The lost worker
        // spent the attempt its claim counted; a run with none left ends
        // here, and its waiters are woken with that outcome.
        let mut statements = vec![stmt(
            format!(
                "UPDATE tako_workflow_runs SET lease_until = ?2
                 WHERE app = ?1 AND status = 'running' AND lease_until < {DB_NOW_MS}
                 RETURNING id, worker_id, attempts >= max_attempts"
            ),
            &args,
        )];
        statements.extend(wake_run_waiters(LOST_RUNS, &args));
        statements.push(stmt(
            "UPDATE tako_workflow_runs SET
                 status = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'pending' END,
                 last_error = CASE WHEN attempts >= max_attempts THEN ?3 ELSE last_error END,
                 finished_at = CASE WHEN attempts >= max_attempts THEN ?4 ELSE finished_at END,
                 worker_id = NULL, lease_until = NULL
             WHERE app = ?1 AND status = 'running' AND lease_until = ?2",
            &args,
        ));
        let marked = self
            .client
            .batch(&statements)?
            .into_iter()
            .next()
            .ok_or_else(invalid)?;
        Ok(Reclaimed::from_rows(
            decode_all::<(String, Option<String>, i64)>(marked)?
                .into_iter()
                .map(|(id, worker, exhausted)| (id, worker, exhausted != 0))
                .collect(),
        ))
    }

    pub(super) fn in_flight_by_worker(&self) -> Result<HashMap<String, u32>, RunsDbError> {
        let rows = self.query(stmt(
            "SELECT worker_id, COUNT(*) FROM tako_workflow_runs
             WHERE app = ?1 AND status = 'running' AND worker_id IS NOT NULL
             GROUP BY worker_id",
            &self.args([]),
        ))?;
        Ok(decode_all::<(String, i64)>(rows)?
            .into_iter()
            .map(|(worker, count)| (worker, count as u32))
            .collect())
    }

    pub(super) fn wait_for_event(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        event_name: &str,
        timeout_at_ms: Option<i64>,
    ) -> Result<bool, RunsDbError> {
        let args = self.wait_args(run_id, worker_id, step_name, timeout_at_ms, event_name, []);
        // A signal addressed to the run before the wait is its result, unless
        // it arrived after the deadline: then the wait timed out and the late
        // signal stays in the inbox, unconsumed.
        self.wait(
            vec![
                stmt(
                    format!(
                        "INSERT INTO tako_workflow_steps (app, run_id, name, result, completed_at)
                         SELECT ?1, ?2, ?4,
                             CASE WHEN ?6 IS NOT NULL AND received_at > ?6 THEN 'null'
                             ELSE payload END,
                             ?5
                         FROM tako_workflow_run_inbox
                         WHERE app = ?1 AND run_id = ?2 AND event_name = ?8
                           AND {OWNED} AND NOT {SAVED}
                         ON CONFLICT DO NOTHING"
                    ),
                    &args,
                ),
                // changes() counts the step the statement above saved.
                stmt(
                    "DELETE FROM tako_workflow_run_inbox
                     WHERE app = ?1 AND run_id = ?2 AND event_name = ?8 AND changes() = 1
                       AND (?6 IS NULL OR received_at <= ?6)",
                    &args,
                ),
            ],
            &args,
        )
    }

    /// See `RunsDb::wait_for_run`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn wait_for_run(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        target_name: &str,
        target_id: &str,
        timeout_at_ms: Option<i64>,
    ) -> Result<bool, RunsDbError> {
        let args = self.wait_args(
            run_id,
            worker_id,
            step_name,
            timeout_at_ms,
            &super::run_event_name(target_id),
            [
                target_name.into(),
                target_id.into(),
                serde_json::to_string(&super::missing_outcome())?.into(),
            ],
        );
        // The write lock orders this with the transaction that finishes the
        // target, which wakes its waiters: a wait parked here is either seen
        // by it or sees its outcome. A finished, missing, or other-workflow
        // target satisfies the wait at once; one that finished after the
        // deadline leaves it timed out.
        self.wait(
            vec![stmt(
                format!(
                    "INSERT INTO tako_workflow_steps (app, run_id, name, result, completed_at)
                     SELECT ?1, ?2, ?4, outcome, ?5 FROM (
                         SELECT CASE
                             WHEN t.id IS NOT NULL AND ?6 IS NOT NULL AND t.finished_at > ?6
                                 AND (t.name <> ?9 OR t.status IN {TERMINAL}) THEN 'null'
                             WHEN t.id IS NULL OR t.name <> ?9 THEN ?11
                             WHEN t.status IN {TERMINAL} THEN json_object('status', t.status,
                                 'result', json(COALESCE(t.result, 'null')), 'error', t.last_error)
                         END AS outcome
                         FROM (SELECT 1) LEFT JOIN tako_workflow_runs t
                             ON t.app = ?1 AND t.id = ?10
                     )
                     WHERE outcome IS NOT NULL AND {OWNED} AND NOT {SAVED}
                     ON CONFLICT DO NOTHING"
                ),
                &args,
            )],
            &args,
        )
    }

    /// Arguments of [`Self::wait`], then `rest` from ?9.
    fn wait_args<const N: usize>(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        timeout_at_ms: Option<i64>,
        event_name: &str,
        rest: [Value; N],
    ) -> Vec<Value> {
        let mut args = self.args([
            run_id.into(),
            worker_id.into(),
            step_name.into(),
            self.now().into(),
            timeout_at_ms.into(),
            timeout_at_ms.unwrap_or(i64::MAX).into(),
            event_name.into(),
        ]);
        args.extend(rest);
        args
    }

    /// Release the caller's claim on a run (?2, owned by ?3) that waits on a
    /// step (?4). A step `satisfy` saved, or one saved already, makes the run
    /// due now (?5); otherwise the run parks on the event (?8), due at the
    /// wait's deadline (?7, or never; ?6 is the deadline itself). Either way
    /// it hands back the attempt its claim counted. Returns whether it is due
    /// now.
    fn wait(&self, satisfy: Vec<Statement>, args: &[Value]) -> Result<bool, RunsDbError> {
        let mut statements = satisfy;
        let satisfying = statements.len();
        statements.extend([
            stmt(
                format!(
                    "DELETE FROM tako_workflow_event_waiters
                     WHERE app = ?1 AND run_id = ?2 AND step_name = ?4 AND {OWNED} AND {SAVED}"
                ),
                args,
            ),
            stmt(
                format!(
                    "INSERT INTO tako_workflow_event_waiters
                     (app, run_id, step_name, event_name, expires_at)
                     SELECT ?1, ?2, ?4, ?8, ?6 WHERE {OWNED} AND NOT {SAVED}
                     ON CONFLICT (app, run_id, step_name) DO UPDATE
                     SET event_name = excluded.event_name, expires_at = excluded.expires_at"
                ),
                args,
            ),
            // Last, since it ends the ownership every statement above checks.
            stmt(
                format!(
                    "UPDATE tako_workflow_runs
                     SET status = 'pending', worker_id = NULL, lease_until = NULL,
                         attempts = attempts - 1, run_at = CASE WHEN {SAVED} THEN ?5 ELSE ?7 END
                     WHERE app = ?1 AND id = ?2 AND worker_id = ?3 AND status = 'running'
                     RETURNING 1"
                ),
                args,
            ),
            stmt(format!("SELECT {SAVED}"), args),
        ]);
        let results = self.client.batch(&statements)?;
        let [_, _, released, saved] = split(results.into_iter().skip(satisfying).collect())?;
        if released.is_empty() {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(one::<(i64,)>(saved)?.0 != 0)
    }

    /// Deliver `payload` to one run: wake it when it waits for `event_name`,
    /// otherwise keep it in the run's inbox until it does.
    pub(super) fn signal_run(
        &self,
        run_id: &str,
        event_name: &str,
        payload: &Value,
    ) -> Result<SignalRunStatus, RunsDbError> {
        let payload_json = serde_json::to_string(payload)?;
        let incoming = payload_json.len();
        let args = self.args([
            run_id.into(),
            event_name.into(),
            payload_json.into(),
            incoming.into(),
            self.now().into(),
            WORKFLOW_SIGNAL_INBOX_MAX_ENTRIES.into(),
            WORKFLOW_SIGNAL_INBOX_MAX_BYTES.into(),
        ]);
        // Nothing is written for a finished or missing run.
        let live = "EXISTS (SELECT 1 FROM tako_workflow_runs
            WHERE app = ?1 AND id = ?2 AND status IN ('pending', 'running'))";
        let waiting = "FROM tako_workflow_event_waiters
            WHERE app = ?1 AND run_id = ?2 AND event_name = ?3";
        // The first value for a name wins, whether a step or a buffered signal.
        let held = "(EXISTS (SELECT 1 FROM tako_workflow_steps
                WHERE app = ?1 AND run_id = ?2 AND name = ?3)
            OR EXISTS (SELECT 1 FROM tako_workflow_run_inbox
                WHERE app = ?1 AND run_id = ?2 AND event_name = ?3))";
        let inbox = "FROM tako_workflow_run_inbox WHERE app = ?1 AND run_id = ?2";
        let [_, _, buffered, _, _, delivered, state] = split(self.client.batch(&[
            // A wait past its deadline has timed out, even before its run is
            // claimed: it gets `null`, and this signal finds the name taken.
            stmt(
                format!(
                    "INSERT INTO tako_workflow_steps (app, run_id, name, result, completed_at)
                     SELECT app, run_id, step_name, 'null', ?6 {waiting}
                       AND expires_at IS NOT NULL AND expires_at <= ?6 AND {live}
                     ON CONFLICT DO NOTHING"
                ),
                &args,
            ),
            stmt(
                format!(
                    "DELETE {waiting} AND expires_at IS NOT NULL AND expires_at <= ?6 AND {live}"
                ),
                &args,
            ),
            // Buffered only when no step waits for it, and within the
            // inbox's limits.
            stmt(
                format!(
                    "INSERT INTO tako_workflow_run_inbox
                     (app, run_id, event_name, payload, payload_bytes, received_at)
                     SELECT ?1, ?2, ?3, ?4, ?5, ?6
                     WHERE {live} AND NOT EXISTS (SELECT 1 {waiting}) AND NOT {held}
                       AND (SELECT COUNT(*) < ?7 AND COALESCE(SUM(payload_bytes), 0) + ?5 <= ?8
                            {inbox})
                     RETURNING 1"
                ),
                &args,
            ),
            stmt(
                format!(
                    "INSERT INTO tako_workflow_steps (app, run_id, name, result, completed_at)
                     SELECT app, run_id, step_name, ?4, ?6 {waiting} AND {live}
                     ON CONFLICT DO NOTHING"
                ),
                &args,
            ),
            stmt(
                format!(
                    "UPDATE tako_workflow_runs SET run_at = ?6
                     WHERE app = ?1 AND id = ?2 AND status = 'pending'
                       AND EXISTS (SELECT 1 {waiting})"
                ),
                &args,
            ),
            stmt(format!("DELETE {waiting} AND {live} RETURNING 1"), &args),
            stmt(
                format!(
                    "SELECT (SELECT status FROM tako_workflow_runs WHERE app = ?1 AND id = ?2),
                         {held}, (SELECT COUNT(*) {inbox}),
                         (SELECT COALESCE(SUM(payload_bytes), 0) {inbox})"
                ),
                &args,
            ),
        ])?)?;
        let (status, held, entries, bytes): (Option<String>, i64, i64, i64) = one(state)?;
        let Some(status) = status else {
            return Ok(SignalRunStatus::Missing);
        };
        if parse_status(&status)?.is_terminal() {
            return Ok(SignalRunStatus::Finished);
        }
        if !delivered.is_empty() {
            return Ok(SignalRunStatus::Delivered);
        }
        if !buffered.is_empty() {
            return Ok(SignalRunStatus::Buffered);
        }
        if held != 0 {
            return Ok(SignalRunStatus::Duplicate);
        }
        super::check_inbox_room(run_id, entries, bytes, incoming)?;
        Err(invalid())
    }

    pub(super) fn signal(&self, event_name: &str, payload: &Value) -> Result<u64, RunsDbError> {
        let args = self.args([
            event_name.into(),
            serde_json::to_string(payload)?.into(),
            self.now().into(),
        ]);
        let waiting = "FROM tako_workflow_event_waiters WHERE app = ?1 AND event_name = ?2";
        let expired = "expires_at IS NOT NULL AND expires_at <= ?4";
        let results = self.client.batch(&[
            // Waits past their deadline time out; see `signal_run`.
            stmt(
                format!(
                    "INSERT INTO tako_workflow_steps (app, run_id, name, result, completed_at)
                     SELECT app, run_id, step_name, 'null', ?4 {waiting} AND {expired}
                     ON CONFLICT DO NOTHING"
                ),
                &args,
            ),
            stmt(format!("DELETE {waiting} AND {expired}"), &args),
            stmt(
                format!(
                    "INSERT INTO tako_workflow_steps (app, run_id, name, result, completed_at)
                     SELECT app, run_id, step_name, ?3, ?4 {waiting}
                     ON CONFLICT DO NOTHING"
                ),
                &args,
            ),
            stmt(
                format!(
                    "UPDATE tako_workflow_runs SET run_at = ?4
                     WHERE app = ?1 AND status = 'pending' AND id IN (SELECT run_id {waiting})"
                ),
                &args,
            ),
            stmt(format!("DELETE {waiting}"), &args),
            Statement::new("SELECT changes()", vec![]),
        ])?;
        let (woken,): (i64,) = one(results.into_iter().last().ok_or_else(invalid)?)?;
        Ok(woken as u64)
    }

    pub(super) fn pending_count(&self) -> Result<u64, RunsDbError> {
        let rows = self.query(stmt(
            "SELECT COUNT(*) FROM tako_workflow_runs WHERE app = ?1 AND status = 'pending'",
            &self.args([]),
        ))?;
        Ok(one::<(i64,)>(rows)?.0 as u64)
    }

    pub(super) fn has_runnable_work(&self) -> Result<bool, RunsDbError> {
        let rows = self.query(stmt(
            "SELECT EXISTS (SELECT 1 FROM tako_workflow_runs
                 WHERE app = ?1 AND status = 'pending' AND run_at <= ?2)",
            &self.args([self.now().into()]),
        ))?;
        Ok(one::<(i64,)>(rows)?.0 != 0)
    }

    pub(super) fn list_runs(
        &self,
        status: Option<&str>,
        name: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SummaryRow>, RunsDbError> {
        let rows = self.query(stmt(
            "SELECT id, name, status, attempts, max_attempts, run_at, created_at, finished_at,
                    last_error
             FROM tako_workflow_runs
             WHERE app = ?1 AND (?2 IS NULL OR status = ?2) AND (?3 IS NULL OR name = ?3)
             ORDER BY created_at DESC, id DESC
             LIMIT ?4",
            &self.args([status.into(), name.into(), limit.into()]),
        ))?;
        rows.into_iter()
            .map(|row| Ok(summary(decode(row)?)))
            .collect()
    }

    pub(super) fn get_run(
        &self,
        id: &str,
        step_limit: i64,
    ) -> Result<Option<RunRecord>, RunsDbError> {
        let args = self.args([
            id.into(),
            step_limit.into(),
            WORKFLOW_RUN_MAX_SIGNALS.into(),
        ]);
        // One snapshot, so the steps belong to the run state shown with them.
        let [run, steps, waits, signals] = split(self.client.read_batch(&[
            stmt(
                "SELECT id, name, status, attempts, max_attempts, run_at, created_at, finished_at,
                        last_error, payload, result, lease_until, worker_id,
                        (SELECT COUNT(*) FROM tako_workflow_steps WHERE app = ?1 AND run_id = ?2),
                        (SELECT COUNT(*) FROM tako_workflow_run_inbox
                         WHERE app = ?1 AND run_id = ?2)
                 FROM tako_workflow_runs WHERE app = ?1 AND id = ?2",
                &args,
            ),
            stmt(
                "SELECT name, result, completed_at FROM tako_workflow_steps
                 WHERE app = ?1 AND run_id = ?2
                 ORDER BY completed_at DESC, name DESC LIMIT ?3",
                &args,
            ),
            stmt(
                "SELECT step_name, event_name, expires_at FROM tako_workflow_event_waiters
                 WHERE app = ?1 AND run_id = ?2 ORDER BY step_name LIMIT ?3",
                &args,
            ),
            stmt(
                "SELECT event_name, received_at, payload_bytes FROM tako_workflow_run_inbox
                 WHERE app = ?1 AND run_id = ?2 ORDER BY received_at, event_name LIMIT ?4",
                &args,
            ),
        ])?)?;
        let Some(row) = optional::<RunDetailRow>(run)? else {
            return Ok(None);
        };
        let RunDetailRow(
            id,
            name,
            status,
            attempts,
            max_attempts,
            run_at,
            created_at,
            finished_at,
            last_error,
            payload,
            result,
            lease_until,
            worker_id,
            steps_total,
            signals_total,
        ) = row;
        Ok(Some(RunRecord {
            run: RunRow {
                summary: summary((
                    id,
                    name,
                    status,
                    attempts,
                    max_attempts,
                    run_at,
                    created_at,
                    finished_at,
                    last_error,
                )),
                payload,
                result,
                lease_until,
                worker_id,
            },
            steps: decode_all::<(String, String, i64)>(steps)?
                .into_iter()
                .map(|(name, result, completed_at)| StepRow {
                    name,
                    result,
                    completed_at,
                })
                .collect(),
            steps_total: steps_total as u64,
            waits: decode_all::<(String, String, Option<i64>)>(waits)?
                .into_iter()
                .map(|(step_name, event_name, expires_at)| WaitRow {
                    step_name,
                    event_name,
                    expires_at,
                })
                .collect(),
            signals: decode_all::<(String, i64, i64)>(signals)?
                .into_iter()
                .map(|(event_name, received_at, bytes)| SignalRow {
                    event_name,
                    received_at,
                    bytes,
                })
                .collect(),
            signals_total: signals_total as u64,
        }))
    }

    pub(super) fn retry_run(&self, id: &str) -> Result<WorkflowRunStatus, RunsDbError> {
        let args = self.args([id.into(), self.now().into()]);
        let retryable = "EXISTS (SELECT 1 FROM tako_workflow_runs
            WHERE app = ?1 AND id = ?2 AND status IN ('dead', 'cancelled'))";
        let [previous, _, _] = split(self.client.batch(&[
            stmt(
                "SELECT status FROM tako_workflow_runs WHERE app = ?1 AND id = ?2",
                &args,
            ),
            // Leftover waits are dropped; the handler parks again if needed.
            stmt(
                format!(
                    "DELETE FROM tako_workflow_event_waiters
                     WHERE app = ?1 AND run_id = ?2 AND {retryable}"
                ),
                &args,
            ),
            stmt(
                "UPDATE tako_workflow_runs
                 SET status = 'pending', attempts = 0, run_at = ?3, lease_until = NULL,
                     worker_id = NULL, finished_at = NULL, result = NULL
                 WHERE app = ?1 AND id = ?2 AND status IN ('dead', 'cancelled')",
                &args,
            ),
        ])?)?;
        let (status,): (String,) =
            optional(previous)?.ok_or_else(|| RunsDbError::RunNotFound { id: id.to_string() })?;
        let status = parse_status(&status)?;
        if !matches!(
            status,
            WorkflowRunStatus::Dead | WorkflowRunStatus::Cancelled
        ) {
            return Err(RunsDbError::RunNotRetryable {
                id: id.to_string(),
                status,
            });
        }
        Ok(status)
    }

    pub(super) fn cancel_run(&self, id: &str) -> Result<CancelledRun, RunsDbError> {
        let args = self.args([id.into(), OPERATOR_CANCEL_REASON.into(), self.now().into()]);
        let cancellable = "EXISTS (SELECT 1 FROM tako_workflow_runs
            WHERE app = ?1 AND id = ?2 AND status IN ('pending', 'running'))";
        let mut statements = vec![
            stmt(
                "SELECT status, worker_id FROM tako_workflow_runs WHERE app = ?1 AND id = ?2",
                &args,
            ),
            // The run's own waits go with it, so a later signal neither wakes
            // it nor writes its step.
            stmt(
                format!(
                    "DELETE FROM tako_workflow_event_waiters
                     WHERE app = ?1 AND run_id = ?2 AND {cancellable}"
                ),
                &args,
            ),
            stmt(
                "UPDATE tako_workflow_runs
                 SET status = 'cancelled', last_error = ?3, worker_id = NULL, lease_until = NULL,
                     finished_at = ?4
                 WHERE app = ?1 AND id = ?2 AND status IN ('pending', 'running')",
                &args,
            ),
        ];
        statements.extend(wake_run_waiters(FINISHED_RUN, &args));
        let previous = self
            .client
            .batch(&statements)?
            .into_iter()
            .next()
            .ok_or_else(invalid)?;
        let (status, worker_id): (String, Option<String>) =
            optional(previous)?.ok_or_else(|| RunsDbError::RunNotFound { id: id.to_string() })?;
        let status = parse_status(&status)?;
        if !matches!(
            status,
            WorkflowRunStatus::Pending | WorkflowRunStatus::Running
        ) {
            return Err(RunsDbError::RunAlreadyFinished {
                id: id.to_string(),
                status,
            });
        }
        Ok(CancelledRun {
            previous_status: status,
            worker_id: worker_id.filter(|_| status == WorkflowRunStatus::Running),
        })
    }

    /// Copy rows read from a per-server store of an earlier release. Rows
    /// already present are kept, so a retried or repeated import is
    /// harmless, and several servers importing their own stores into one
    /// database do not overwrite each other. Runs go last: a run becomes
    /// claimable only once its saved steps and waits are in.
    pub(super) fn import_legacy(&self, rows: &super::legacy::Rows) -> Result<(), RunsDbError> {
        const CHUNK_BYTES: usize = 1024 * 1024;
        let tables: [(&[Value], &str); 5] = [
            (
                &rows.steps,
                "INSERT INTO tako_workflow_steps (app, run_id, name, result, completed_at)
                 SELECT ?1, json_extract(value, '$.run_id'), json_extract(value, '$.name'),
                     json_extract(value, '$.result'), json_extract(value, '$.completed_at')
                 FROM json_each(?2) WHERE true
                 ON CONFLICT (app, run_id, name) DO NOTHING",
            ),
            (
                &rows.waiters,
                "INSERT INTO tako_workflow_event_waiters (app, run_id, step_name, event_name, expires_at)
                 SELECT ?1, json_extract(value, '$.run_id'), json_extract(value, '$.step_name'),
                     json_extract(value, '$.event_name'), json_extract(value, '$.expires_at')
                 FROM json_each(?2) WHERE true
                 ON CONFLICT (app, run_id, step_name) DO NOTHING",
            ),
            (
                &rows.inbox,
                "INSERT INTO tako_workflow_run_inbox
                     (app, run_id, event_name, payload, payload_bytes, received_at)
                 SELECT ?1, json_extract(value, '$.run_id'), json_extract(value, '$.event_name'),
                     json_extract(value, '$.payload'), json_extract(value, '$.payload_bytes'),
                     json_extract(value, '$.received_at')
                 FROM json_each(?2) WHERE true
                 ON CONFLICT (app, run_id, event_name) DO NOTHING",
            ),
            (
                &rows.schedules,
                "INSERT INTO tako_workflow_schedules (app, name, cron, last_run_at, max_attempts)
                 SELECT ?1, json_extract(value, '$.name'), json_extract(value, '$.cron'),
                     json_extract(value, '$.last_run_at'), json_extract(value, '$.max_attempts')
                 FROM json_each(?2) WHERE true
                 ON CONFLICT (app, name) DO NOTHING",
            ),
            (
                &rows.runs,
                "INSERT INTO tako_workflow_runs
                     (app, id, name, payload, status, attempts, max_attempts, run_at,
                      lease_until, worker_id, last_error, created_at, finished_at, result)
                 SELECT ?1, json_extract(value, '$.id'), json_extract(value, '$.name'),
                     json_extract(value, '$.payload'), json_extract(value, '$.status'),
                     json_extract(value, '$.attempts'), json_extract(value, '$.max_attempts'),
                     json_extract(value, '$.run_at'), json_extract(value, '$.lease_until'),
                     json_extract(value, '$.worker_id'), json_extract(value, '$.last_error'),
                     json_extract(value, '$.created_at'), json_extract(value, '$.finished_at'),
                     json_extract(value, '$.result')
                 FROM json_each(?2) WHERE true
                 ON CONFLICT (app, id) DO NOTHING",
            ),
        ];
        for (rows, sql) in tables {
            for chunk in super::legacy::chunks(rows, CHUNK_BYTES) {
                self.client
                    .batch(&[stmt(sql, &self.args([chunk.into()]))])?;
            }
        }
        Ok(())
    }

    /// A revision above every one handed out before, for a deployment's
    /// schedule registration.
    pub(super) fn next_schedule_revision(&self) -> Result<i64, RunsDbError> {
        let [revision] = split(self.client.batch(&[Statement::new(
            // Never below a registered revision, even in a database copied
            // in from elsewhere.
            "UPDATE tako_workflow_meta
             SET value = MAX(value,
                 COALESCE((SELECT MAX(revision) FROM tako_workflow_schedule_releases), 0)) + 1
             WHERE name = 'schedule_revision'
             RETURNING value",
            vec![],
        )])?)?;
        Ok(one::<(i64,)>(revision)?.0)
    }

    pub(super) fn replace_schedules(
        &self,
        schedules: &[ScheduleSpec],
        build: Option<&str>,
        revision: Option<i64>,
    ) -> Result<(), RunsDbError> {
        let specs: Vec<_> = schedules
            .iter()
            .map(|s| serde_json::json!({"name": s.name, "cron": s.cron, "max_attempts": s.max_attempts}))
            .collect();
        let args = self.args([
            build.unwrap_or_default().into(),
            revision.unwrap_or(0).into(),
            serde_json::to_string(&specs)?.into(),
            self.now().into(),
        ]);
        // Whole-set replacements serialize across servers. An older release
        // or a pre-upgrade recovery (revision 0) cannot replace a newer
        // deployment's set; the statements after the first then change
        // nothing.
        let active = "EXISTS (SELECT 1 FROM tako_workflow_schedule_releases
            WHERE app = ?1 AND build = ?2 AND revision = ?3)";
        self.client.batch(&[
            stmt(
                "INSERT INTO tako_workflow_schedule_releases AS active (app, build, revision)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT (app) DO UPDATE SET build = excluded.build, revision = excluded.revision
                 WHERE active.revision < excluded.revision
                    OR (active.revision = excluded.revision AND active.build = excluded.build)",
                &args,
            ),
            stmt(
                format!(
                    "DELETE FROM tako_workflow_schedules WHERE app = ?1 AND {active}
                       AND name NOT IN (SELECT json_extract(value, '$.name') FROM json_each(?4))"
                ),
                &args,
            ),
            // New schedules start now; existing ones keep their last run, so
            // a re-registration does not repeat processed boundaries.
            stmt(
                format!(
                    "INSERT INTO tako_workflow_schedules (app, name, cron, last_run_at, max_attempts)
                     SELECT ?1, json_extract(value, '$.name'), json_extract(value, '$.cron'), ?5,
                         json_extract(value, '$.max_attempts')
                     FROM json_each(?4) WHERE {active}
                     ON CONFLICT (app, name)
                     DO UPDATE SET cron = excluded.cron, max_attempts = excluded.max_attempts"
                ),
                &args,
            ),
        ])?;
        Ok(())
    }

    pub(super) fn list_schedules(&self) -> Result<Vec<ScheduleRow>, RunsDbError> {
        let rows = self.query(stmt(
            "SELECT name, cron, last_run_at, max_attempts FROM tako_workflow_schedules
             WHERE app = ?1 ORDER BY name",
            &self.args([]),
        ))?;
        Ok(
            decode_all::<(String, String, Option<i64>, Option<i64>)>(rows)?
                .into_iter()
                .map(|(name, cron, last_run_at, max_attempts)| ScheduleRow {
                    name,
                    cron,
                    last_run_at,
                    max_attempts: max_attempts
                        .map(|budget| budget.clamp(0, u32::MAX as i64) as u32),
                })
                .collect(),
        )
    }

    pub(super) fn set_schedule_last_run_at(&self, name: &str, ts: i64) -> Result<(), RunsDbError> {
        self.client.batch(&[stmt(
            "UPDATE tako_workflow_schedules SET last_run_at = ?3 WHERE app = ?1 AND name = ?2",
            &self.args([name.into(), ts.into()]),
        )])?;
        Ok(())
    }

    /// Like `update_running`, for a write that finishes the run (?2, owned
    /// by ?3, at ?4): in one transaction with waking the runs waiting on its
    /// outcome.
    fn finish_running(&self, set: &str, args: Vec<Value>) -> Result<(), RunsDbError> {
        let mut statements = vec![stmt(
            format!(
                "UPDATE tako_workflow_runs SET {set}
                 WHERE app = ?1 AND id = ?2 AND worker_id = ?3 AND status = 'running'
                 RETURNING 1"
            ),
            &args,
        )];
        // No waiter outlives its target's finishing, so waking the waiters of
        // a run that was already finished, when the caller no longer owned
        // it, finds none.
        statements.extend(wake_run_waiters(FINISHED_RUN, &args));
        let finished = self
            .client
            .batch(&statements)?
            .into_iter()
            .next()
            .ok_or_else(invalid)?;
        if finished.is_empty() {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(())
    }

    /// Updates a run (?2) only while the caller (?3) still owns it.
    fn update_running(&self, set: &str, args: &[Value]) -> Result<(), RunsDbError> {
        let [updated] = split(self.client.batch(&[stmt(
            format!(
                "UPDATE tako_workflow_runs SET {set}
                 WHERE app = ?1 AND id = ?2 AND worker_id = ?3 AND status = 'running'
                 RETURNING 1"
            ),
            args,
        )])?)?;
        if updated.is_empty() {
            return Err(RunsDbError::StaleWorker);
        }
        Ok(())
    }
}

type SummaryColumns = (
    String,
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<String>,
);

fn summary(
    (id, name, status, attempts, max_attempts, run_at, created_at, finished_at, last_error): SummaryColumns,
) -> SummaryRow {
    SummaryRow {
        id,
        name,
        status,
        attempts,
        max_attempts,
        run_at,
        created_at,
        finished_at,
        last_error,
    }
}

/// A `runs` row with its step and signal counts, as `get_run` reads it.
#[derive(serde::Deserialize)]
struct RunDetailRow(
    String,
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<String>,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    i64,
    i64,
);

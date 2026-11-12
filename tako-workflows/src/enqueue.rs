//! Run enqueue + per-step persistence + lifecycle transitions.
//!
//! All run state lives in two tables:
//!   - `runs` - one row per run; tracks status, attempts, lease.
//!   - `steps` - append-only memoization of completed step results.
//!
//! Operations are synchronous; call from `tokio::task::spawn_blocking` when
//! invoking from async contexts.

pub(crate) mod operator;
mod sqlite;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tako_core::{
    EnqueueOpts, EnqueueRunResponse, RunPayload, ScheduleSpec, WORKFLOW_RUN_MAX_STEPS,
    WorkflowRunDetail, WorkflowRunStatus, WorkflowRunSummary, WorkflowStoreKind,
};

pub use operator::{CancelledRun, OPERATOR_CANCEL_REASON};

use super::postgres_store::PostgresRunsDb;
use sqlite::SqliteRunsDb;

pub const POSTGRES_WORKFLOWS_SCHEMA: &str = "tako_workflows";
/// Finished history and run IDs remain reserved for at least six calendar months.
pub const DEFAULT_RETENTION_MS: i64 = 184 * 24 * 60 * 60 * 1000;

pub use tako_core::RunResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowStoreConfig {
    Sqlite {
        path: PathBuf,
    },
    Postgres {
        url: String,
        schema: String,
        app_id: String,
    },
}

impl WorkflowStoreConfig {
    pub fn sqlite(path: impl Into<PathBuf>) -> Self {
        Self::Sqlite { path: path.into() }
    }

    pub fn postgres(url: impl Into<String>, app_id: impl Into<String>) -> Self {
        Self::Postgres {
            url: url.into(),
            schema: POSTGRES_WORKFLOWS_SCHEMA.to_string(),
            app_id: app_id.into(),
        }
    }
}

/// Cap for client-supplied `lease_ms`. One week is far longer than any
/// legitimate workflow step; anything larger is a misuse or hostile input
/// that would wrap `i64` arithmetic below if passed through directly.
const MAX_LEASE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Saturating cast of client-supplied lease length (in ms) to the signed
/// millisecond value we store in sqlite. Prevents `u64::MAX as i64 -> -1`
/// from producing a negative `lease_until` that would be reclaimed
/// instantly.
pub(crate) fn clamp_lease_ms(lease_ms: u64) -> i64 {
    lease_ms.min(MAX_LEASE_MS) as i64
}

#[derive(thiserror::Error, Debug)]
pub enum RunsDbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("postgres error: {0}")]
    Postgres(#[from] postgres::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Storage(String),
    #[error("{0}")]
    UnsupportedBackend(String),
    #[error("workflow run ID '{id}' already belongs to a different workflow or payload")]
    IdConflict { id: String },
    #[error("workflow run ID must contain 1–255 bytes and no NUL characters")]
    InvalidId,
    /// The run is no longer owned by the caller (lease expired and was
    /// reclaimed by another worker, or the run already terminated).
    #[error("stale worker: run is no longer owned by this worker")]
    StaleWorker,
    #[error("Workflow run not found: {id}")]
    RunNotFound { id: String },
    #[error("Workflow run {id} is {status}; only dead or cancelled runs can be retried")]
    RunNotRetryable {
        id: String,
        status: WorkflowRunStatus,
    },
    #[error(
        "Workflow run {id} already finished as {status}; only pending or running runs can be cancelled"
    )]
    RunAlreadyFinished {
        id: String,
        status: WorkflowRunStatus,
    },
}

impl RunsDbError {
    /// The internal-socket code a worker or enqueue command failing with this
    /// error replies with. Operator-only failures (`RunNotFound`, ...) and
    /// storage errors an SDK cannot act on have none.
    pub fn socket_code(&self) -> Option<tako_core::ErrorCode> {
        use tako_core::ErrorCode;
        match self {
            Self::StaleWorker => Some(ErrorCode::RunNotOwned),
            Self::IdConflict { .. } => Some(ErrorCode::RunIdConflict),
            Self::InvalidId => Some(ErrorCode::InvalidRequest),
            Self::Sqlite(error) => matches!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            )
            .then_some(ErrorCode::Unavailable),
            Self::Postgres(error) => postgres_unavailable(error).then_some(ErrorCode::Unavailable),
            Self::Json(_)
            | Self::Storage(_)
            | Self::UnsupportedBackend(_)
            | Self::RunNotFound { .. }
            | Self::RunNotRetryable { .. }
            | Self::RunAlreadyFinished { .. } => None,
        }
    }

    /// Context in front of the message, keeping the code.
    pub fn into_socket_error(self, operation: &str) -> tako_core::SocketError {
        tako_core::SocketError {
            code: self.socket_code(),
            message: format!("{operation} failed: {self}"),
        }
    }
}

/// A lost connection or a server refusing connections, not a rejected query.
fn postgres_unavailable(error: &postgres::Error) -> bool {
    use postgres::error::SqlState;
    if error.is_closed() {
        return true;
    }
    error.code().is_some_and(|code| {
        code.code().starts_with("08")
            || [
                SqlState::ADMIN_SHUTDOWN,
                SqlState::CRASH_SHUTDOWN,
                SqlState::CANNOT_CONNECT_NOW,
                SqlState::TOO_MANY_CONNECTIONS,
            ]
            .contains(code)
    })
}

pub struct RunsDb {
    backend: RunsDbBackend,
    pub(crate) monitoring: crate::monitoring::StoreMonitoring,
}

enum RunsDbBackend {
    Sqlite(SqliteRunsDb),
    Postgres(Box<PostgresRunsDb>),
}

impl RunsDb {
    pub fn open_config(config: WorkflowStoreConfig) -> Result<Self, RunsDbError> {
        match config {
            WorkflowStoreConfig::Sqlite { path } => Self::open_sqlite(&path),
            WorkflowStoreConfig::Postgres {
                url,
                schema,
                app_id,
            } => Ok(Self {
                monitoring: crate::monitoring::StoreMonitoring::postgres(&url, &schema, &app_id)?,
                backend: RunsDbBackend::Postgres(Box::new(PostgresRunsDb::open(
                    &url, &schema, &app_id,
                )?)),
            }),
        }
    }

    pub fn open(path: &Path) -> Result<Self, RunsDbError> {
        Self::open_sqlite(path)
    }

    pub fn open_sqlite(path: &Path) -> Result<Self, RunsDbError> {
        Ok(Self {
            backend: RunsDbBackend::Sqlite(SqliteRunsDb::open(path)?),
            monitoring: Default::default(),
        })
    }

    pub fn open_postgres(url: &str, app_id: &str) -> Result<Self, RunsDbError> {
        Self::open_config(WorkflowStoreConfig::postgres(url, app_id))
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, RunsDbError> {
        Ok(Self {
            backend: RunsDbBackend::Sqlite(SqliteRunsDb::open_in_memory()?),
            monitoring: Default::default(),
        })
    }

    /// Insert a run, or reuse a retained run with the same supplied ID and inputs.
    /// Conflicting workflow names or payloads return `IdConflict`.
    pub fn enqueue(
        &self,
        name: &str,
        payload: &serde_json::Value,
        opts: &EnqueueOpts,
    ) -> Result<EnqueueRunResponse, RunsDbError> {
        if let Some(id) = &opts.id
            && (id.is_empty() || id.len() > 255 || id.contains('\0'))
        {
            return Err(RunsDbError::InvalidId);
        }
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.enqueue(name, payload, opts),
            RunsDbBackend::Postgres(db) => db.enqueue(name, payload, opts),
        }
    }

    /// Test-only raw SQL escape hatches against the sqlite backend.
    #[cfg(test)]
    pub(crate) fn raw_execute(&self, sql: &str, params: impl rusqlite::Params) {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.raw_execute(sql, params),
            RunsDbBackend::Postgres(_) => {
                panic!("sqlite connection requested for postgres workflow store")
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn raw_query_values(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Vec<rusqlite::types::Value> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.raw_query_values(sql, params),
            RunsDbBackend::Postgres(_) => {
                panic!("sqlite connection requested for postgres workflow store")
            }
        }
    }

    /// Atomically claim the oldest eligible run for one of `names`. Bumps
    /// `attempts`. Returns `None` when nothing is due. Loads any persisted
    /// step results into `step_state`.
    pub fn claim(
        &self,
        worker_id: &str,
        names: &[String],
        lease_ms: u64,
    ) -> Result<Option<RunPayload>, RunsDbError> {
        let started = self
            .monitoring
            .claims
            .get()
            .map(|_| std::time::Instant::now());
        let result = match &self.backend {
            RunsDbBackend::Sqlite(db) => db.claim(worker_id, names, lease_ms),
            RunsDbBackend::Postgres(db) => db.claim(worker_id, names, lease_ms),
        };
        if let Some(started) = started {
            let outcome = match &result {
                Ok(Some(_)) => 0,
                Ok(None) => 1,
                Err(_) => 2,
            };
            self.monitoring
                .claims
                .get()
                .unwrap()
                .observe(outcome, started.elapsed().as_secs_f64());
        }
        result
    }

    /// All of these lifecycle writes are guarded by `worker_id = ?1 AND
    /// status = 'running'`. If a worker's lease expired and another worker
    /// already reclaimed the run, the UPDATE affects 0 rows and we return
    /// `StaleWorker` so the SDK can log/raise rather than silently
    /// marking the run in a state the new worker didn't intend.
    pub fn heartbeat(&self, id: &str, worker_id: &str, lease_ms: u64) -> Result<(), RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.heartbeat(id, worker_id, lease_ms),
            RunsDbBackend::Postgres(db) => db.heartbeat(id, worker_id, lease_ms),
        }
    }

    /// Persist a single completed step result. Guarded by the run's
    /// current `worker_id` so a stale worker that lost its lease can't
    /// write step results into a different worker's in-flight run.
    /// First-write-wins on `(run_id, name)` - a duplicate save after a
    /// failed RPC is silently deduped.
    pub fn save_step(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        result: &serde_json::Value,
    ) -> Result<(), RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.save_step(run_id, worker_id, step_name, result),
            RunsDbBackend::Postgres(db) => db.save_step(run_id, worker_id, step_name, result),
        }
    }

    pub fn result(&self, name: &str, id: &str) -> Result<Option<RunResult>, RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.result(name, id),
            RunsDbBackend::Postgres(db) => db.result(name, id),
        }
    }

    pub fn complete(
        &self,
        id: &str,
        worker_id: &str,
        result: &serde_json::Value,
    ) -> Result<(), RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.complete(id, worker_id, result),
            RunsDbBackend::Postgres(db) => db.complete(id, worker_id, result),
        }
    }

    /// Delete at most `limit` terminal runs completed before `cutoff_ms`.
    /// Returns the number of runs deleted, so housekeeping can retry soon
    /// when a backlog remains.
    pub fn prune_finished_before(&self, cutoff_ms: i64, limit: u32) -> Result<u64, RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.prune_finished_before(cutoff_ms, limit),
            RunsDbBackend::Postgres(db) => db.prune_finished_before(cutoff_ms, limit),
        }
    }

    pub fn cancel(
        &self,
        id: &str,
        worker_id: &str,
        reason: Option<&str>,
    ) -> Result<(), RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.cancel(id, worker_id, reason),
            RunsDbBackend::Postgres(db) => db.cancel(id, worker_id, reason),
        }
    }

    pub fn fail(
        &self,
        id: &str,
        worker_id: &str,
        error: &str,
        next_run_at_ms: Option<i64>,
        finalize: bool,
    ) -> Result<(), RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.fail(id, worker_id, error, next_run_at_ms, finalize),
            RunsDbBackend::Postgres(db) => db.fail(id, worker_id, error, next_run_at_ms, finalize),
        }
    }

    /// Reschedule a run for later without bumping attempts (for durable
    /// `ctx.sleep` and `ctx.waitFor` parking). When `wake_at_ms` is None
    /// the run is parked indefinitely (waiting for an event).
    pub fn defer(
        &self,
        id: &str,
        worker_id: &str,
        wake_at_ms: Option<i64>,
    ) -> Result<(), RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.defer(id, worker_id, wake_at_ms),
            RunsDbBackend::Postgres(db) => db.defer(id, worker_id, wake_at_ms),
        }
    }

    /// Take back every run whose lease expired and return how many there
    /// were. A run with attempts left goes back to `pending`. A run that was
    /// on its last attempt becomes `dead` with [`WORKER_LOST_ERROR`]: its
    /// worker died before the SDK could finalize it, and requeueing would
    /// let a run that kills its worker be reclaimed forever. Deferring and
    /// waiting hand their attempt back, so they never count toward this.
    pub fn reclaim_expired(&self) -> Result<u64, RunsDbError> {
        Ok(self.reclaim()?.count)
    }

    /// Atomically reclaim expired leases and return the list of
    /// `worker_id`s whose runs were reclaimed, one entry per reclaimed
    /// row (so callers can decrement per-worker in-flight counters). The
    /// list covers requeued and dead runs alike; see [`Self::reclaim_expired`].
    pub fn reclaim_expired_with_workers(&self) -> Result<Vec<String>, RunsDbError> {
        Ok(self.reclaim()?.workers)
    }

    fn reclaim(&self) -> Result<Reclaimed, RunsDbError> {
        let reclaimed = match &self.backend {
            RunsDbBackend::Sqlite(db) => db.reclaim_expired(),
            RunsDbBackend::Postgres(db) => db.reclaim_expired(),
        }?;
        // No worker is left to report these runs, so the server does.
        for run_id in &reclaimed.dead {
            tracing::warn!(%run_id, "Workflow run lost its worker on its last attempt; marked dead");
        }
        Ok(reclaimed)
    }

    /// Snapshot of `worker_id -> in-flight count` over currently-running
    /// rows. Used to rehydrate [`InFlightLimiter`] on startup so cached
    /// counts match reality before the socket starts serving claims.
    pub fn in_flight_by_worker(
        &self,
    ) -> Result<std::collections::HashMap<String, u32>, RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.in_flight_by_worker(),
            RunsDbBackend::Postgres(db) => db.in_flight_by_worker(),
        }
    }

    /// Park a run waiting for a named event. Stores the waiter and defers
    /// the run. Wake happens via `signal` (or via run_at if a timeout was
    /// set and it elapses).
    pub fn wait_for_event(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        event_name: &str,
        timeout_at_ms: Option<i64>,
    ) -> Result<(), RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => {
                db.wait_for_event(run_id, worker_id, step_name, event_name, timeout_at_ms)
            }
            RunsDbBackend::Postgres(db) => {
                db.wait_for_event(run_id, worker_id, step_name, event_name, timeout_at_ms)
            }
        }
    }

    /// Deliver an event payload. Wakes every parked waiter with matching
    /// `event_name`: the payload is stored as the waiter's step result,
    /// the waiter row is removed, and the run is set to pending. Returns
    /// the number of runs woken.
    pub fn signal(
        &self,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> Result<u64, RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.signal(event_name, payload),
            RunsDbBackend::Postgres(db) => db.signal(event_name, payload),
        }
    }

    pub fn pending_count(&self) -> Result<u64, RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.pending_count(),
            RunsDbBackend::Postgres(db) => db.pending_count(),
        }
    }

    /// Returns true when at least one pending run is due for workers to
    /// claim now. Future `run_at` rows stay durable without waking a
    /// scale-to-zero worker until the dispatcher scan sees them become due.
    pub fn has_runnable_work(&self) -> Result<bool, RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.has_runnable_work(),
            RunsDbBackend::Postgres(db) => db.has_runnable_work(),
        }
    }

    pub fn store_kind(&self) -> WorkflowStoreKind {
        match &self.backend {
            RunsDbBackend::Sqlite(_) => WorkflowStoreKind::Sqlite,
            RunsDbBackend::Postgres(_) => WorkflowStoreKind::Postgres,
        }
    }

    /// Newest runs first, at most `limit` and the reply budget, and whether
    /// more match.
    pub fn list_runs(
        &self,
        status: Option<WorkflowRunStatus>,
        name: Option<&str>,
        limit: u32,
    ) -> Result<(Vec<WorkflowRunSummary>, bool), RunsDbError> {
        let status = status.map(WorkflowRunStatus::as_str);
        let fetch = limit.saturating_add(1);
        let rows = match &self.backend {
            RunsDbBackend::Sqlite(db) => db.list_runs(status, name, fetch),
            RunsDbBackend::Postgres(db) => db.list_runs(status, name, fetch),
        }?;
        operator::summary_page(rows, limit as usize)
    }

    /// Every field of a run with its most recent steps, capped for a
    /// management reply.
    pub fn get_run(&self, id: &str) -> Result<Option<WorkflowRunDetail>, RunsDbError> {
        let limit = WORKFLOW_RUN_MAX_STEPS as i64;
        let record = match &self.backend {
            RunsDbBackend::Sqlite(db) => db.get_run(id, limit),
            RunsDbBackend::Postgres(db) => db.get_run(id, limit),
        }?;
        record.map(operator::RunRecord::into_detail).transpose()
    }

    /// Return a `dead` or `cancelled` run to `pending`, due now, with its
    /// attempts reset. Saved steps stay, so it resumes after the last one.
    /// Leftover event waits are dropped; the handler parks again if needed.
    /// Returns the previous status.
    pub fn retry_run(&self, id: &str) -> Result<WorkflowRunStatus, RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.retry_run(id),
            RunsDbBackend::Postgres(db) => db.retry_run(id),
        }
    }

    /// Cancel a `pending` or `running` run for an operator. A running
    /// attempt loses ownership, so its later writes fail with `StaleWorker`
    /// and reclaim, which only takes back running rows, leaves it alone.
    pub fn cancel_run(&self, id: &str) -> Result<CancelledRun, RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.cancel_run(id),
            RunsDbBackend::Postgres(db) => db.cancel_run(id),
        }
    }

    pub(crate) fn replace_schedules(&self, schedules: &[ScheduleSpec]) -> Result<(), RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.replace_schedules(schedules),
            RunsDbBackend::Postgres(db) => db.replace_schedules(schedules),
        }
    }

    pub(crate) fn list_schedules(&self) -> Result<Vec<ScheduleRow>, RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.list_schedules(),
            RunsDbBackend::Postgres(db) => db.list_schedules(),
        }
    }

    pub(crate) fn set_schedule_last_run_at(&self, name: &str, ts: i64) -> Result<(), RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.set_schedule_last_run_at(name, ts),
            RunsDbBackend::Postgres(db) => db.set_schedule_last_run_at(name, ts),
        }
    }
}

/// Stored as the error of a run that reclaim ends; see [`RunsDb::reclaim_expired`].
pub const WORKER_LOST_ERROR: &str = "worker lost; retry budget exhausted";

/// Expired leases taken back by one reclaim pass.
#[derive(Debug, Default)]
pub(crate) struct Reclaimed {
    /// Every reclaimed run, requeued or dead.
    pub(crate) count: u64,
    /// Claim owner of each reclaimed run that still recorded one.
    pub(crate) workers: Vec<String>,
    /// IDs of the reclaimed runs that had no attempts left.
    pub(crate) dead: Vec<String>,
}

impl Reclaimed {
    /// Build from `(run id, claim owner, budget exhausted)` rows.
    pub(crate) fn from_rows(rows: Vec<(String, Option<String>, bool)>) -> Self {
        let mut reclaimed = Self {
            count: rows.len() as u64,
            ..Self::default()
        };
        for (id, worker, exhausted) in rows {
            reclaimed.workers.extend(worker);
            if exhausted {
                reclaimed.dead.push(id);
            }
        }
        reclaimed
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ScheduleRow {
    pub(crate) name: String,
    pub(crate) cron: String,
    pub(crate) last_run_at: Option<i64>,
    /// The registering workflow's budget; `None` uses the server default.
    pub(crate) max_attempts: Option<u32>,
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub(super) fn check_existing_run(
    id: &str,
    name: &str,
    payload: &serde_json::Value,
    existing_name: &str,
    existing_payload: &str,
) -> Result<(), RunsDbError> {
    if existing_name != name
        || serde_json::from_str::<serde_json::Value>(existing_payload)? != *payload
    {
        return Err(RunsDbError::IdConflict { id: id.into() });
    }
    Ok(())
}

#[cfg(test)]
mod tests;

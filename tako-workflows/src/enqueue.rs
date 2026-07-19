//! Run enqueue + per-step persistence + lifecycle transitions.
//!
//! All run state lives in two tables:
//!   - `runs` - one row per run; tracks status, attempts, lease.
//!   - `steps` - append-only memoization of completed step results.
//!
//! Operations are synchronous; call from `tokio::task::spawn_blocking` when
//! invoking from async contexts.

mod sqlite;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tako_core::{EnqueueOpts, EnqueueRunResponse, RunPayload, ScheduleSpec};

use super::postgres_store::PostgresRunsDb;
use sqlite::SqliteRunsDb;

pub const POSTGRES_WORKFLOWS_SCHEMA: &str = "tako_workflows";

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
    Sqlite(#[from] turso::Error),
    #[error("postgres error: {0}")]
    Postgres(#[from] postgres::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Storage(String),
    #[error("{0}")]
    UnsupportedBackend(String),
    /// The run is no longer owned by the caller (lease expired and was
    /// reclaimed by another worker, or the run already terminated).
    #[error("stale worker: run is no longer owned by this worker")]
    StaleWorker,
}

pub struct RunsDb {
    backend: RunsDbBackend,
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
        })
    }

    pub fn open_postgres(url: &str, app_id: &str) -> Result<Self, RunsDbError> {
        Self::open_config(WorkflowStoreConfig::postgres(url, app_id))
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, RunsDbError> {
        Ok(Self {
            backend: RunsDbBackend::Sqlite(SqliteRunsDb::open_in_memory()?),
        })
    }

    /// Insert a new run, or return the id of an existing non-terminal run
    /// with the same `unique_key` if one exists.
    pub fn enqueue(
        &self,
        name: &str,
        payload: &serde_json::Value,
        opts: &EnqueueOpts,
    ) -> Result<EnqueueRunResponse, RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.enqueue(name, payload, opts),
            RunsDbBackend::Postgres(db) => db.enqueue(name, payload, opts),
        }
    }

    /// Test-only raw SQL escape hatches against the sqlite backend.
    #[cfg(test)]
    pub(crate) fn raw_execute(&self, sql: &str, params: impl turso::IntoParams) {
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
        params: impl turso::IntoParams,
    ) -> Vec<turso::Value> {
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
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.claim(worker_id, names, lease_ms),
            RunsDbBackend::Postgres(db) => db.claim(worker_id, names, lease_ms),
        }
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

    pub fn complete(&self, id: &str, worker_id: &str) -> Result<(), RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.complete(id, worker_id),
            RunsDbBackend::Postgres(db) => db.complete(id, worker_id),
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

    pub fn reclaim_expired(&self) -> Result<u64, RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.reclaim_expired(),
            RunsDbBackend::Postgres(db) => db.reclaim_expired(),
        }
    }

    /// Atomically reclaim expired leases and return the list of
    /// `worker_id`s whose runs were reclaimed, one entry per reclaimed
    /// row (so callers can decrement per-worker in-flight counters).
    pub fn reclaim_expired_with_workers(&self) -> Result<Vec<String>, RunsDbError> {
        match &self.backend {
            RunsDbBackend::Sqlite(db) => db.reclaim_expired_with_workers(),
            RunsDbBackend::Postgres(db) => db.reclaim_expired_with_workers(),
        }
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

#[derive(Debug, Clone)]
pub(crate) struct ScheduleRow {
    pub(crate) name: String,
    pub(crate) cron: String,
    pub(crate) last_run_at: Option<i64>,
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests;

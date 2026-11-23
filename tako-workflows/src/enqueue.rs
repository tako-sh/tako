//! Run enqueue + per-step persistence + lifecycle transitions.
//!
//! Every app's runs live in its Tako-hosted workflow database (see
//! [`WorkflowStoreConfig`]), shared by every server that runs the app:
//!   - runs: one row per run; tracks status, attempts, lease.
//!   - steps: append-only memoization of completed step results.
//!   - event waits, run inboxes and cron schedules.
//!
//! Operations are synchronous and block on the database; call them from
//! `tokio::task::spawn_blocking` (or `crate::blocking::run`) when invoking
//! from async contexts.

mod hosted;
pub(crate) mod legacy;
pub(crate) mod operator;
mod replacement;

use std::time::{SystemTime, UNIX_EPOCH};

use tako_core::{
    EnqueueOpts, EnqueueRunResponse, RunPayload, ScheduleSpec, SignalRunStatus,
    WORKFLOW_EVENT_NAME_MAX_BYTES, WORKFLOW_RUN_MAX_STEPS, WORKFLOW_SIGNAL_INBOX_MAX_BYTES,
    WORKFLOW_SIGNAL_INBOX_MAX_ENTRIES, WorkflowRunDetail, WorkflowRunStatus, WorkflowRunSummary,
};

pub use operator::{CancelledRun, OPERATOR_CANCEL_REASON};

use hosted::HostedRunsDb;
#[cfg(test)]
pub(crate) use hosted::testing::HostedFixture;

pub use replacement::{keep_live_schedules, schema_sql};

/// Finished history and run IDs remain reserved for at least six calendar months.
pub const DEFAULT_RETENTION_MS: i64 = 184 * 24 * 60 * 60 * 1000;

pub use tako_core::RunResult;

/// An app's workflow store: a Tako-hosted database, reached through this
/// server's local endpoint (`http://127.0.0.1:<port>`) with the token of the
/// app's `@workflows` binding. Every table is scoped by `app_id`, so several
/// apps can share one database.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkflowStoreConfig {
    pub url: String,
    pub token: String,
    pub app_id: String,
}

/// Leaves out the endpoint and the token, which is a credential.
impl std::fmt::Debug for WorkflowStoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowStoreConfig")
            .field("url", &"<redacted>")
            .field("token", &"<redacted>")
            .field("app_id", &self.app_id)
            .finish()
    }
}

impl WorkflowStoreConfig {
    pub fn hosted(
        url: impl Into<String>,
        token: impl Into<String>,
        app_id: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
            app_id: app_id.into(),
        }
    }
}

/// Cap for client-supplied `lease_ms`. One week is far longer than any
/// legitimate workflow step; anything larger is a misuse or hostile input
/// that would wrap `i64` arithmetic below if passed through directly.
const MAX_LEASE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Saturating cast of client-supplied lease length (in ms) to the signed
/// millisecond value the store keeps. Prevents `u64::MAX as i64 -> -1`
/// from producing a negative `lease_until` that would be reclaimed
/// instantly.
pub(crate) fn clamp_lease_ms(lease_ms: u64) -> i64 {
    lease_ms.min(MAX_LEASE_MS) as i64
}

#[derive(thiserror::Error, Debug)]
pub enum RunsDbError {
    /// Carries SQLite's error name, never the statement's message, which
    /// can contain app data.
    #[error("workflow store error: {0}")]
    Hosted(#[from] tako_db::client::ClientError),
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
    #[error(
        "workflow run {id} already holds {WORKFLOW_SIGNAL_INBOX_MAX_ENTRIES} buffered signals or {WORKFLOW_SIGNAL_INBOX_MAX_BYTES} bytes of them; the signal was not stored"
    )]
    SignalInboxFull { id: String },
    #[error("{0}")]
    InvalidEventName(&'static str),
}

impl RunsDbError {
    /// Whether the workflow database did not answer, so the same call can
    /// succeed later.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Hosted(error) if error.is_unavailable())
    }

    /// The internal-socket code a worker or enqueue command failing with this
    /// error replies with. Operator-only failures (`RunNotFound`, ...) and
    /// storage errors an SDK cannot act on have none.
    pub fn socket_code(&self) -> Option<tako_core::ErrorCode> {
        use tako_core::ErrorCode;
        match self {
            Self::StaleWorker => Some(ErrorCode::RunNotOwned),
            Self::IdConflict { .. } => Some(ErrorCode::RunIdConflict),
            Self::InvalidId | Self::InvalidEventName(_) => Some(ErrorCode::InvalidRequest),
            Self::SignalInboxFull { .. } => Some(ErrorCode::SignalInboxFull),
            Self::Hosted(error) => error.is_unavailable().then_some(ErrorCode::Unavailable),
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

pub struct RunsDb {
    store: HostedRunsDb,
    pub(crate) monitoring: crate::monitoring::StoreMonitoring,
    schedules: ScheduleRegistration,
}

#[derive(Default)]
struct ScheduleRegistration {
    build: Option<String>,
    revision: Option<i64>,
    done: parking_lot::Mutex<bool>,
}

impl RunsDb {
    /// Import `app`'s runs from the per-server SQLite store an earlier
    /// release kept at `path`, then rename that file. A store that cannot
    /// be read is left in place, and the import is tried again on the next
    /// deploy or restart.
    pub(crate) fn import_legacy_store(&self, path: &std::path::Path) {
        let imported = legacy::read(path)
            .map_err(|error| error.to_string())
            .and_then(|rows| {
                self.store
                    .import_legacy(&rows)
                    .map(|()| rows.len())
                    .map_err(|error| error.to_string())
            })
            .and_then(|count| {
                legacy::retire(path)
                    .map(|()| count)
                    .map_err(|error| error.to_string())
            });
        match imported {
            Ok(rows) => tracing::info!(
                path = %path.display(),
                rows,
                "Imported workflow runs from this server's earlier store"
            ),
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "Could not import workflow runs from this server's earlier store; retrying on the next start"
            ),
        }
    }

    /// Opens the store, creating its tables when they are missing.
    pub fn open(config: WorkflowStoreConfig) -> Result<Self, RunsDbError> {
        Ok(Self::with_store(HostedRunsDb::open(
            &config.url,
            &config.token,
            &config.app_id,
        )?))
    }

    /// A handle on a server whose clock (for scheduling and history, not
    /// leases) is `clock`.
    #[cfg(test)]
    pub(crate) fn open_with_clock(
        config: WorkflowStoreConfig,
        clock: fn() -> i64,
    ) -> Result<Self, RunsDbError> {
        Ok(Self::with_store(HostedRunsDb::open_with_clock(
            &config.url,
            &config.token,
            &config.app_id,
            clock,
        )?))
    }

    fn with_store(store: HostedRunsDb) -> Self {
        Self {
            store,
            monitoring: Default::default(),
            schedules: Default::default(),
        }
    }

    /// Insert a run, or reuse a retained run with the same supplied ID and inputs.
    /// Conflicting workflow names or payloads return `IdConflict`.
    pub fn enqueue(
        &self,
        name: &str,
        payload: &serde_json::Value,
        opts: &EnqueueOpts,
    ) -> Result<EnqueueRunResponse, RunsDbError> {
        if let Some(id) = &opts.id {
            validate_run_id(id)?;
        }
        self.store.enqueue(name, payload, opts)
    }

    /// Test-only SQL. `{runs}`, `{steps}`, `{event_waiters}`, `{run_inbox}`
    /// and `{schedules}` name the store's tables; arguments are JSON
    /// scalars. A statement reaches every app in the database.
    #[cfg(test)]
    pub(crate) fn test_execute(&self, sql: &str, args: Vec<serde_json::Value>) {
        self.test_rows(sql, args);
    }

    /// Like [`Self::test_execute`], for a query returning one row.
    #[cfg(test)]
    pub(crate) fn test_query_row(
        &self,
        sql: &str,
        args: Vec<serde_json::Value>,
    ) -> Vec<serde_json::Value> {
        let mut rows = self.test_rows(sql, args);
        assert_eq!(rows.len(), 1, "{sql}");
        rows.pop().unwrap()
    }

    #[cfg(test)]
    fn test_rows(&self, sql: &str, args: Vec<serde_json::Value>) -> Vec<Vec<serde_json::Value>> {
        let sql = ["runs", "steps", "event_waiters", "run_inbox", "schedules"]
            .into_iter()
            .fold(sql.to_string(), |sql, table| {
                sql.replace(&format!("{{{table}}}"), &format!("tako_workflow_{table}"))
            });
        self.store.test_rows(&sql, args)
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
        let result = self.store.claim(worker_id, names, lease_ms);
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

    /// All of these lifecycle writes are guarded by the run's current
    /// owner and `status = 'running'`. If a worker's lease expired and
    /// another worker already reclaimed the run, the write changes nothing
    /// and returns `StaleWorker` so the SDK can log/raise rather than
    /// silently marking the run in a state the new worker didn't intend.
    pub fn heartbeat(&self, id: &str, worker_id: &str, lease_ms: u64) -> Result<(), RunsDbError> {
        self.store.heartbeat(id, worker_id, lease_ms)
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
        self.store.save_step(run_id, worker_id, step_name, result)
    }

    pub fn result(&self, name: &str, id: &str) -> Result<Option<RunResult>, RunsDbError> {
        self.store.result(name, id)
    }

    pub fn complete(
        &self,
        id: &str,
        worker_id: &str,
        result: &serde_json::Value,
    ) -> Result<(), RunsDbError> {
        self.store.complete(id, worker_id, result)
    }

    /// Delete at most `limit` terminal runs completed before `cutoff_ms`.
    /// Returns the number of runs deleted, so housekeeping can retry soon
    /// when a backlog remains.
    pub fn prune_finished_before(&self, cutoff_ms: i64, limit: u32) -> Result<u64, RunsDbError> {
        self.store.prune_finished_before(cutoff_ms, limit)
    }

    pub fn cancel(
        &self,
        id: &str,
        worker_id: &str,
        reason: Option<&str>,
    ) -> Result<(), RunsDbError> {
        self.store.cancel(id, worker_id, reason)
    }

    pub fn fail(
        &self,
        id: &str,
        worker_id: &str,
        error: &str,
        next_run_at_ms: Option<i64>,
        finalize: bool,
    ) -> Result<(), RunsDbError> {
        self.store
            .fail(id, worker_id, error, next_run_at_ms, finalize)
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
        self.store.defer(id, worker_id, wake_at_ms)
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
        let reclaimed = self.store.reclaim_expired()?;
        // No worker is left to report these runs, so the server does.
        for run_id in &reclaimed.dead {
            tracing::warn!(%run_id, "Workflow run lost its worker on its last attempt; marked dead");
        }
        Ok(reclaimed)
    }

    /// Snapshot of `worker_id -> in-flight count` over currently-running
    /// rows. Used to rehydrate [`InFlightLimiter`] on startup so cached
    /// counts match reality before the socket starts serving claims.
    ///
    /// [`InFlightLimiter`]: crate::InFlightLimiter
    pub fn in_flight_by_worker(
        &self,
    ) -> Result<std::collections::HashMap<String, u32>, RunsDbError> {
        self.store.in_flight_by_worker()
    }

    /// Park a run waiting for a named event. Stores the waiter and defers
    /// the run. Wake happens via `signal` (or via run_at if a timeout was
    /// set and it elapses).
    ///
    /// When the run's inbox already holds a signal for `event_name`, it is
    /// consumed as the step result instead, the run is due again at once,
    /// and this returns `true`. Either way the caller no longer owns the run.
    pub fn wait_for_event(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        event_name: &str,
        timeout_at_ms: Option<i64>,
    ) -> Result<bool, RunsDbError> {
        self.store
            .wait_for_event(run_id, worker_id, step_name, event_name, timeout_at_ms)
    }

    /// Park a run until `target_id`, a run of `target_name` in this store,
    /// finishes; its outcome (as [`Self::result`] reports it) becomes the
    /// step result. The transaction that finishes the target wakes every
    /// run waiting on it. A target that already finished, is missing, or
    /// belongs to another workflow satisfies the wait at once, and this
    /// returns `true`; the run is then due immediately. An expired wait
    /// saves `null`.
    pub fn wait_for_run(
        &self,
        run_id: &str,
        worker_id: &str,
        step_name: &str,
        target_name: &str,
        target_id: &str,
        timeout_at_ms: Option<i64>,
    ) -> Result<bool, RunsDbError> {
        validate_run_id(target_id)?;
        self.store.wait_for_run(
            run_id,
            worker_id,
            step_name,
            target_name,
            target_id,
            timeout_at_ms,
        )
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
        if event_name.starts_with(RESERVED_EVENT_PREFIX) {
            return Err(RunsDbError::InvalidEventName(RESERVED_EVENT_NAME));
        }
        self.store.signal(event_name, payload)
    }

    /// Deliver an event payload to one run. A run waiting for `event_name`
    /// is woken with it, like [`Self::signal`]. A run that has not reached
    /// that wait yet keeps the payload in its inbox, and the wait consumes
    /// it. The first value for a name wins; later ones are reported as
    /// [`SignalRunStatus::Duplicate`] and not stored. An inbox over its
    /// limits rejects the signal with [`RunsDbError::SignalInboxFull`]
    /// rather than dropping anything.
    pub fn signal_run(
        &self,
        run_id: &str,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> Result<SignalRunStatus, RunsDbError> {
        validate_run_id(run_id)?;
        validate_event_name(event_name)?;
        self.store.signal_run(run_id, event_name, payload)
    }

    pub fn pending_count(&self) -> Result<u64, RunsDbError> {
        self.store.pending_count()
    }

    /// Returns true when at least one pending run is due for workers to
    /// claim now. Future `run_at` rows stay durable without waking a
    /// scale-to-zero worker until the dispatcher scan sees them become due.
    pub fn has_runnable_work(&self) -> Result<bool, RunsDbError> {
        self.store.has_runnable_work()
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
        let rows = self
            .store
            .list_runs(status, name, limit.saturating_add(1))?;
        operator::summary_page(rows, limit as usize)
    }

    /// Every field of a run with its most recent steps, capped for a
    /// management reply.
    pub fn get_run(&self, id: &str) -> Result<Option<WorkflowRunDetail>, RunsDbError> {
        self.store
            .get_run(id, WORKFLOW_RUN_MAX_STEPS as i64)?
            .map(operator::RunRecord::into_detail)
            .transpose()
    }

    /// Return a `dead` or `cancelled` run to `pending`, due now, with its
    /// attempts reset. Saved steps stay, so it resumes after the last one.
    /// Leftover event waits are dropped; the handler parks again if needed.
    /// Returns the previous status.
    pub fn retry_run(&self, id: &str) -> Result<WorkflowRunStatus, RunsDbError> {
        self.store.retry_run(id)
    }

    /// Cancel a `pending` or `running` run for an operator. A running
    /// attempt loses ownership, so its later writes fail with `StaleWorker`
    /// and reclaim, which only takes back running rows, leaves it alone.
    pub fn cancel_run(&self, id: &str) -> Result<CancelledRun, RunsDbError> {
        self.store.cancel_run(id)
    }

    /// Servers share the store, so an older release restarting must not
    /// replace a newer release's schedules. A deploy takes a revision above
    /// every earlier one; a recovery keeps the revision its release had.
    pub(crate) fn prepare_schedule_registration(
        &mut self,
        build: Option<String>,
        activation: crate::manager::ScheduleActivation,
    ) -> Result<(), RunsDbError> {
        use crate::manager::ScheduleActivation;
        self.schedules.revision = match activation {
            ScheduleActivation::Deploy => Some(self.store.next_schedule_revision()?),
            ScheduleActivation::Recover(revision) => revision,
        };
        self.schedules.build = build.filter(|build| !build.is_empty());
        Ok(())
    }

    pub(crate) fn schedule_revision(&self) -> Option<i64> {
        self.schedules.revision
    }

    pub(crate) fn needs_schedule_registration(&self) -> bool {
        // Registration may be committing to the store on a blocking thread.
        self.schedules.done.try_lock().is_some_and(|done| !*done)
    }

    pub(crate) fn register_worker_schedules(
        &self,
        build: Option<&str>,
        schedules: &[ScheduleSpec],
    ) -> Result<usize, RunsDbError> {
        if build != self.schedules.build.as_deref() {
            // An old worker can reach the successor's socket while it drains.
            return Ok(0);
        }
        let mut done = self.schedules.done.lock();
        if !*done {
            super::cron::register_schedules(self, schedules)?;
            *done = true;
        }
        Ok(schedules.len())
    }

    pub(crate) fn replace_schedules(&self, schedules: &[ScheduleSpec]) -> Result<(), RunsDbError> {
        self.store.replace_schedules(
            schedules,
            self.schedules.build.as_deref(),
            self.schedules.revision,
        )
    }

    pub(crate) fn list_schedules(&self) -> Result<Vec<ScheduleRow>, RunsDbError> {
        self.store.list_schedules()
    }

    pub(crate) fn set_schedule_last_run_at(&self, name: &str, ts: i64) -> Result<(), RunsDbError> {
        self.store.set_schedule_last_run_at(name, ts)
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

/// Event names starting with this are reserved for Tako's own waits.
pub(crate) const RESERVED_EVENT_PREFIX: &str = "__";

/// The event a run waiting on another run's outcome parks on. The reserved
/// prefix keeps app signals from reaching it.
pub(crate) fn run_event_name(id: &str) -> String {
    format!("__run:{id}")
}

/// The outcome a wait sees for a run that is not retained, or that belongs
/// to another workflow. It matches what the SDKs report for a missing run.
pub(crate) fn missing_outcome() -> RunResult {
    RunResult {
        status: "missing".into(),
        result: serde_json::Value::Null,
        error: Some("Workflow run not found or no longer retained".into()),
    }
}
const RESERVED_EVENT_NAME: &str = "event names starting with `__` are reserved for Tako";

fn validate_run_id(id: &str) -> Result<(), RunsDbError> {
    if id.is_empty() || id.len() > 255 || id.contains('\0') {
        return Err(RunsDbError::InvalidId);
    }
    Ok(())
}

/// Rules for event names a signal addresses to one run. Broadcast signals
/// keep accepting any name a wait could use, except the reserved prefix.
fn validate_event_name(name: &str) -> Result<(), RunsDbError> {
    if name.is_empty()
        || name.len() > WORKFLOW_EVENT_NAME_MAX_BYTES
        || name.chars().any(char::is_control)
    {
        return Err(RunsDbError::InvalidEventName(
            "event names must contain 1–255 bytes and no control characters",
        ));
    }
    if name.starts_with(RESERVED_EVENT_PREFIX) {
        return Err(RunsDbError::InvalidEventName(RESERVED_EVENT_NAME));
    }
    Ok(())
}

/// Rejects a signal that would take the run's inbox past either limit,
/// given the inbox's size when the signal was refused.
pub(super) fn check_inbox_room(
    run_id: &str,
    entries: i64,
    bytes: i64,
    incoming_bytes: usize,
) -> Result<(), RunsDbError> {
    let entries = entries.max(0) as usize;
    let bytes = (bytes.max(0) as usize).saturating_add(incoming_bytes);
    if entries >= WORKFLOW_SIGNAL_INBOX_MAX_ENTRIES || bytes > WORKFLOW_SIGNAL_INBOX_MAX_BYTES {
        return Err(RunsDbError::SignalInboxFull {
            id: run_id.to_string(),
        });
    }
    Ok(())
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

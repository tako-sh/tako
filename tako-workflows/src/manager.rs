//! Per-server workflow lifecycle manager.
//!
//! Holds one entry per deployed app: `RunsDb`, cron ticker, worker
//! supervisor. A single shared internal socket (owned by the manager)
//! routes commands to the right app via the `app` field on each command.
//! Workflow RPCs and server-side channel `publish()` calls both land on this socket,
//! which is why it's named `internal.sock` rather than `workflows.sock`.
//!
//! Single integration surface for `operations.rs` to call from
//! deploy / stop / delete handlers, plus shutdown_all on server shutdown.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::RwLock;
use tako_core::{
    GetWorkflowRunResponse, ListWorkflowRunsResponse, WORKFLOW_RUNS_DEFAULT_LIMIT,
    WORKFLOW_RUNS_MAX_LIMIT, WorkflowRunStatus, WorkflowRunTransition, WorkflowStoreKind,
};

use super::cron::{self, CronTickerHandle};
use super::dispatcher::WorkDispatcher;
use super::enqueue::{RunsDb, RunsDbError, WorkflowStoreConfig};
use super::enqueue_socket::{
    AppHandlers, AppLookup, ChannelPublishFn, EnqueueSocketHandle, NetworkCallFn, OnEnqueue,
    PeerAuthFn, spawn as spawn_internal_socket,
};
use super::in_flight::InFlightLimiter;
use super::supervisor::{WorkerSpec, WorkerSupervisor};

const WORKFLOWS_DB_FILENAME: &str = "workflows.sqlite";

/// The single Tako internal socket. Named after its env var
/// (`TAKO_INTERNAL_SOCKET`) and shared by all server-side SDK RPCs
/// (workflow enqueue/worker loop, channel publish). Single source of truth
/// for the path — consumers (`WorkflowManager`, `AppManager`) must not
/// hardcode `"internal.sock"` themselves.
pub fn internal_socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("internal.sock")
}

/// Per-app workflow resources. Dropping the entry shuts down dispatch,
/// cron, and the worker supervisor.
pub struct AppWorkflow {
    handlers: AppHandlers,
    supervisor: Arc<WorkerSupervisor>,
    dispatcher: Option<WorkDispatcher>,
    cron: Option<CronTickerHandle>,
    accepting_new_work: Arc<AtomicBool>,
}

impl AppWorkflow {
    pub fn supervisor(&self) -> Arc<WorkerSupervisor> {
        self.supervisor.clone()
    }

    fn handlers(&self) -> AppHandlers {
        self.handlers.clone()
    }

    async fn shutdown(mut self, drain_timeout: Duration) {
        self.accepting_new_work.store(false, Ordering::SeqCst);
        if let Some(cron) = self.cron.take() {
            cron.shutdown().await;
        }
        if let Some(dispatcher) = self.dispatcher.take() {
            dispatcher.shutdown().await;
        }
        self.supervisor.shutdown(drain_timeout).await;
    }

    async fn drain_existing_work(mut self) {
        self.accepting_new_work.store(false, Ordering::SeqCst);
        if let Some(cron) = self.cron.take() {
            cron.shutdown().await;
        }
        if let Some(dispatcher) = self.dispatcher.take() {
            dispatcher.shutdown().await;
        }
        self.supervisor.shutdown_gracefully().await;
    }
}

/// A release's validated workflow backend and supervisor. Dispatch starts at cutover.
pub struct PreparedWorkflow {
    db: Arc<RunsDb>,
    limiter: Arc<InFlightLimiter>,
    supervisor: Arc<WorkerSupervisor>,
    retention_ms: Option<i64>,
}

pub struct WorkflowManager {
    data_dir: PathBuf,
    apps: Arc<RwLock<HashMap<String, AppWorkflow>>>,
    socket: parking_lot::Mutex<Option<EnqueueSocketHandle>>,
    postgres_url: parking_lot::Mutex<Option<PostgresUrlResolver>>,
    /// Server-side channel `publish` relay used by SDK channel handles.
    /// Snapshotted at `start_socket` time and passed into the socket's
    /// accept loop.
    channel_publish: parking_lot::Mutex<Option<ChannelPublishFn>>,
    peer_auth: parking_lot::Mutex<Option<PeerAuthFn>>,
    network_call: Arc<RwLock<Option<NetworkCallFn>>>,
    /// Serializes `ensure` calls so two concurrent deploys of the same app
    /// can't each start their own supervisor + cron ticker and then have
    /// one silently overwrite the other (leaking the loser's children).
    ensure_gate: tokio::sync::Mutex<()>,
    retired: parking_lot::Mutex<Vec<RetiredRuntime>>,
    store_gate: Arc<parking_lot::Mutex<StoreGate>>,
}

/// A runtime a deploy replaced. Its workers finish their runs in the
/// background; `task` ends, and releases its store, once they have exited.
struct RetiredRuntime {
    app: String,
    supervisor: Arc<WorkerSupervisor>,
    db: Weak<RunsDb>,
    task: tokio::task::JoinHandle<()>,
}

/// Stores that must stay closed, and the stores operator commands opened.
#[derive(Default)]
struct StoreGate {
    closed: HashSet<String>,
    operator_stores: HashMap<String, usize>,
}

/// While a handle on a closed store remains after its runtimes stop, such as
/// one an in-flight socket call or operator command holds, [`WorkflowManager::close_store`]
/// waits up to this long for it.
const STORE_RELEASE_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(10)
};

/// Keeps an app's workflow store closed until dropped: no runtime of the app
/// runs and operator commands are refused. [`WorkflowManager::ensure`] still
/// opens the store, so a restart can bring workflows back while it is held.
#[must_use = "the store reopens for operator commands when this is dropped"]
pub struct ClosedStore {
    gate: Arc<parking_lot::Mutex<StoreGate>>,
    app: String,
}

impl Drop for ClosedStore {
    fn drop(&mut self) {
        self.gate.lock().closed.remove(&self.app);
    }
}

/// Counts an operator command's handle on an app's store.
struct OperatorLease {
    gate: Arc<parking_lot::Mutex<StoreGate>>,
    app: String,
}

impl Drop for OperatorLease {
    fn drop(&mut self) {
        let mut gate = self.gate.lock();
        if let Some(count) = gate.operator_stores.get_mut(&self.app) {
            *count -= 1;
            if *count == 0 {
                gate.operator_stores.remove(&self.app);
            }
        }
    }
}

type PostgresUrlResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[derive(Debug, Default, Clone, Copy)]
pub struct DrainStatus {
    pub workers: usize,
    pub background_tasks: usize,
}

impl DrainStatus {
    pub fn is_complete(self) -> bool {
        self.workers == 0 && self.background_tasks == 0
    }
}

#[derive(thiserror::Error, Debug)]
pub enum WorkflowManagerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("db error: {0}")]
    Db(#[from] RunsDbError),
    #[error("supervisor error: {0}")]
    Supervisor(#[from] super::supervisor::SupervisorError),
    #[error("{0}")]
    InvalidRequest(String),
    #[error("workflow storage task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// The store an operator command reads or changes, and the app's active
/// runtime when this server runs one.
struct OperatorStore {
    db: Arc<RunsDb>,
    handlers: Option<AppHandlers>,
    _lease: OperatorLease,
}

impl WorkflowManager {
    pub fn begin_server_drain(&self) {
        for entry in self.apps.write().values_mut() {
            entry.accepting_new_work.store(false, Ordering::SeqCst);
            if let Some(cron) = entry.cron.as_mut() {
                cron.request_shutdown();
            }
            if let Some(dispatcher) = entry.dispatcher.as_mut() {
                dispatcher.request_shutdown();
            }
            entry.supervisor.begin_drain();
        }
        for retired in self.retired.lock().iter() {
            retired.supervisor.begin_drain();
        }
    }

    pub fn server_drain_status(&self) -> DrainStatus {
        let mut status = DrainStatus::default();
        for entry in self.apps.read().values() {
            status.workers += entry.supervisor.remaining_children();
            status.background_tasks +=
                usize::from(entry.cron.as_ref().is_some_and(|c| !c.is_finished()));
            status.background_tasks +=
                usize::from(entry.dispatcher.as_ref().is_some_and(|d| !d.is_finished()));
        }
        self.retired.lock().retain(|retired| {
            let workers = retired.supervisor.remaining_children();
            status.workers += workers;
            status.background_tasks += usize::from(!retired.task.is_finished());
            workers > 0 || !retired.task.is_finished()
        });
        status
    }

    /// Returns kill requests and failures. Process exits remain observable separately.
    pub fn force_server_drain(&self) -> (usize, usize) {
        let mut result = (0, 0);
        let apps = self.apps.read();
        let retired = self.retired.lock();
        for supervisor in apps
            .values()
            .map(|entry| &entry.supervisor)
            .chain(retired.iter().map(|retired| &retired.supervisor))
        {
            let (signalled, errors) = supervisor.force_stop();
            result.0 += signalled;
            result.1 += errors;
        }
        result
    }

    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            apps: Arc::new(RwLock::new(HashMap::new())),
            socket: parking_lot::Mutex::new(None),
            postgres_url: parking_lot::Mutex::new(None),
            channel_publish: parking_lot::Mutex::new(None),
            peer_auth: parking_lot::Mutex::new(None),
            network_call: Arc::new(RwLock::new(None)),
            ensure_gate: tokio::sync::Mutex::new(()),
            retired: parking_lot::Mutex::new(Vec::new()),
            store_gate: Arc::default(),
        }
    }

    /// Install the channel-publish relay used by server-side channel
    /// `publish()` calls arriving on the internal socket. Must be called before
    /// `start_socket` — the publisher is snapshotted at that point.
    pub fn set_channel_publisher(&self, publisher: ChannelPublishFn) {
        *self.channel_publish.lock() = Some(publisher);
    }

    pub fn set_network_router(&self, router: NetworkCallFn) {
        *self.network_call.write() = Some(router);
    }

    pub fn set_peer_auth(&self, auth: PeerAuthFn) {
        *self.peer_auth.lock() = Some(auth);
    }

    pub fn set_postgres_url_resolver(&self, resolver: PostgresUrlResolver) {
        *self.postgres_url.lock() = Some(resolver);
    }

    pub fn app_dir(&self, app: &str) -> PathBuf {
        self.data_dir.join("apps").join(app)
    }

    pub fn app_tako_dir(&self, app: &str) -> PathBuf {
        self.app_dir(app).join("data").join("tako")
    }

    pub fn workflows_db_path(&self, app: &str) -> PathBuf {
        self.app_tako_dir(app).join(WORKFLOWS_DB_FILENAME)
    }

    pub fn workflow_store_config(&self, app: &str) -> WorkflowStoreConfig {
        if let Some(url) = self
            .postgres_url
            .lock()
            .as_ref()
            .and_then(|resolver| resolver(app))
        {
            return WorkflowStoreConfig::postgres(url, app.to_string());
        }
        WorkflowStoreConfig::sqlite(self.workflows_db_path(app))
    }

    /// Server-wide socket path. SDKs connect here.
    pub fn socket_path(&self) -> PathBuf {
        internal_socket_path(&self.data_dir)
    }

    /// Bring up the shared internal socket. Idempotent — calling twice is a
    /// no-op. Should be called once at server startup before any `ensure`.
    pub fn start_socket(&self) -> Result<(), WorkflowManagerError> {
        let mut guard = self.socket.lock();
        if guard.is_some() {
            return Ok(());
        }
        let apps = self.apps.clone();
        let lookup: AppLookup = Arc::new(move |app: &str| {
            let apps = apps.read();
            apps.get(app).map(AppWorkflow::handlers)
        });
        let publisher = self.channel_publish.lock().clone();
        let peer_auth = self.peer_auth.lock().clone();
        let network_call = self.network_call.clone();
        let route: NetworkCallFn = Arc::new(move |caller, target, invocation| {
            let route = network_call.read().clone();
            match route {
                Some(route) => route(caller, target, invocation),
                None => tako_core::Response::coded_error(
                    tako_core::ErrorCode::Unavailable,
                    "network primitives are unavailable",
                ),
            }
        });
        *guard = Some(spawn_internal_socket(
            self.socket_path(),
            lookup,
            publisher,
            peer_auth,
            Some(route),
        )?);
        Ok(())
    }

    /// Configure or reconfigure workflows for an app. Called on deploy.
    /// The replacement runtime is built before swapping it into the lookup map,
    /// so a failed replacement keeps the existing runtime active.
    pub async fn ensure(
        &self,
        app: &str,
        retention_ms: Option<i64>,
        spec_fn: impl FnOnce(PathBuf) -> WorkerSpec,
    ) -> Result<(), WorkflowManagerError> {
        // Serialize concurrent ensures of the same app (rare, but a deploy
        // race can otherwise start two supervisors + two cron tickers and
        // silently drop the first when the second inserts). Held across
        // the DB open + supervisor.start().await so replacements for the
        // same app cannot interleave.
        let _gate = self.ensure_gate.lock().await;

        let store_config = self.workflow_store_config(app);
        let db_path = match &store_config {
            WorkflowStoreConfig::Sqlite { path } => path.clone(),
            WorkflowStoreConfig::Postgres { .. } => self.workflows_db_path(app),
        };
        let prepared = self
            .prepare(store_config, retention_ms, spec_fn(db_path))
            .await?;
        if let Err(error) = prepared.supervisor.start().await {
            prepared.supervisor.shutdown(Duration::ZERO).await;
            return Err(error.into());
        }
        self.install(app, prepared);
        Ok(())
    }

    /// Prepare a candidate against its own backend without replacing the active runtime.
    pub async fn prepare(
        &self,
        store_config: WorkflowStoreConfig,
        retention_ms: Option<i64>,
        spec: WorkerSpec,
    ) -> Result<PreparedWorkflow, WorkflowManagerError> {
        // The synchronous Postgres client cannot connect on a runtime thread.
        let (db, in_flight) = crate::blocking::run(move || {
            RunsDb::open_config(store_config).map(|db| {
                let in_flight = db.in_flight_by_worker();
                (db, in_flight)
            })
        })
        .await??;
        let db = Arc::new(db);

        let limiter = Arc::new(InFlightLimiter::new(spec.concurrency));
        // Rehydrate: any rows still `status='running'` from a previous
        // process count against the live budget until their leases expire
        // and `reclaim_expired_with_workers` drops them.
        if let Ok(snapshot) = in_flight {
            limiter.rehydrate(snapshot);
        }
        let supervisor = Arc::new(WorkerSupervisor::new(spec));
        Ok(PreparedWorkflow {
            db,
            limiter,
            supervisor,
            retention_ms,
        })
    }

    pub fn install(&self, app: &str, prepared: PreparedWorkflow) {
        let PreparedWorkflow {
            db,
            limiter,
            supervisor,
            retention_ms,
        } = prepared;
        let sup_for_dispatcher = supervisor.clone();
        let db_for_dispatcher = db.clone();
        let dispatcher = WorkDispatcher::spawn(
            Arc::new(move || {
                // wake() errors surface via health_check on the next
                // enqueue; the log_sink inside the supervisor carries
                // any human-readable detail.
                let _ = sup_for_dispatcher.wake();
            }),
            Arc::new(move || match db_for_dispatcher.has_runnable_work() {
                Ok(has_work) => has_work,
                Err(e) => {
                    tracing::warn!(error = %e, "workflow runnable-work check failed");
                    false
                }
            }),
        );
        let dispatch_signal = dispatcher.signaler();
        let on_enqueue: OnEnqueue = Arc::new(move || {
            dispatch_signal.signal();
        });
        let cron_handle = cron::spawn_with_limiter(
            db.clone(),
            limiter.clone(),
            on_enqueue.clone(),
            retention_ms,
        );

        let sup_for_health = supervisor.clone();
        let sup_for_claim = supervisor.clone();
        let accepting_new_work = Arc::new(AtomicBool::new(true));
        let handlers = AppHandlers {
            db,
            limiter,
            accepting_new_work: accepting_new_work.clone(),
            on_enqueue,
            health_check: Arc::new(move || sup_for_health.check_startup_health()),
            on_claimed: Arc::new(move || {
                sup_for_claim.notify_claimed();
            }),
        };

        let entry = AppWorkflow {
            handlers,
            supervisor,
            dispatcher: Some(dispatcher),
            cron: Some(cron_handle),
            accepting_new_work,
        };

        let mut apps = self.apps.write();
        let old_entry = apps.insert(app.to_string(), entry);
        drop(apps);

        if let Some(mut old_entry) = old_entry {
            old_entry.accepting_new_work.store(false, Ordering::SeqCst);
            if let Some(cron) = old_entry.cron.as_mut() {
                cron.request_shutdown();
            }
            if let Some(dispatcher) = old_entry.dispatcher.as_mut() {
                dispatcher.request_shutdown();
            }
            let supervisor = old_entry.supervisor.clone();
            let db = Arc::downgrade(&old_entry.handlers.db);
            let task = tokio::spawn(old_entry.drain_existing_work());
            let mut retired = self.retired.lock();
            retired.retain(|retired| {
                !retired.task.is_finished() || retired.supervisor.remaining_children() > 0
            });
            retired.push(RetiredRuntime {
                app: app.to_string(),
                supervisor,
                db,
                task,
            });
        }
    }

    /// Stop the worker but keep the DB around (app is paused).
    pub async fn stop(&self, app: &str, drain_timeout: Duration) {
        let entry = self.apps.write().remove(app);
        if let Some(entry) = entry {
            entry.shutdown(drain_timeout).await;
        }
    }

    /// Stop every workflow runtime of `app`, including runtimes a deploy
    /// replaced whose workers are still finishing runs, and wait until this
    /// process holds no handle on the app's store. Workers still running
    /// after `drain_timeout` are killed. Operator commands are refused until
    /// the returned guard is dropped. Fails when a handle outlives the stop,
    /// so callers never replace store files underneath an open connection.
    pub async fn close_store(
        &self,
        app: &str,
        drain_timeout: Duration,
    ) -> Result<ClosedStore, WorkflowManagerError> {
        self.store_gate.lock().closed.insert(app.to_string());
        let closed = ClosedStore {
            gate: self.store_gate.clone(),
            app: app.to_string(),
        };
        let mut handles: Vec<Weak<RunsDb>> = self
            .apps
            .read()
            .get(app)
            .map(|entry| Arc::downgrade(&entry.handlers.db))
            .into_iter()
            .collect();
        self.stop(app, drain_timeout).await;
        let retired: Vec<RetiredRuntime> = {
            let mut all = self.retired.lock();
            let (ours, others) = std::mem::take(&mut *all)
                .into_iter()
                .partition(|retired| retired.app == app);
            *all = others;
            ours
        };
        for retired in retired {
            handles.push(retired.db);
            retired.supervisor.shutdown(drain_timeout).await;
            // The drain task ends once its workers are gone, releasing the store.
            let _ = retired.task.await;
        }
        let deadline = tokio::time::Instant::now() + STORE_RELEASE_TIMEOUT;
        loop {
            let open = handles
                .iter()
                .filter(|handle| handle.strong_count() > 0)
                .count()
                + self
                    .store_gate
                    .lock()
                    .operator_stores
                    .get(app)
                    .copied()
                    .unwrap_or(0);
            if open == 0 {
                return Ok(closed);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(WorkflowManagerError::InvalidRequest(format!(
                    "The workflow store of {app} is still open after its workers stopped"
                )));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Retire the active workflow runtime without killing in-flight runs.
    /// New enqueues/claims stop immediately; running workers receive SIGTERM,
    /// drain, and can still write lifecycle updates through the retained
    /// handlers until they exit.
    pub fn retire(&self, app: &str) {
        let mut active = self.apps.write();
        let Some(mut entry) = active.remove(app) else {
            return;
        };

        entry.accepting_new_work.store(false, Ordering::SeqCst);
        if let Some(cron) = entry.cron.as_mut() {
            cron.request_shutdown();
        }
        if let Some(dispatcher) = entry.dispatcher.as_mut() {
            dispatcher.request_shutdown();
        }

        let handlers = entry.handlers.clone();
        let supervisor = entry.supervisor.clone();
        let accepting_new_work = entry.accepting_new_work.clone();
        let draining_entry = AppWorkflow {
            handlers,
            supervisor,
            dispatcher: None,
            cron: None,
            accepting_new_work,
        };

        active.insert(app.to_string(), draining_entry);
        drop(active);

        let apps = self.apps.clone();
        let app = app.to_string();
        tokio::spawn(async move {
            let supervisor = entry.supervisor.clone();
            entry.drain_existing_work().await;
            let mut apps = apps.write();
            if apps
                .get(&app)
                .is_some_and(|current| Arc::ptr_eq(&current.supervisor, &supervisor))
            {
                apps.remove(&app);
            }
        });
    }

    /// Stop the worker and remove per-app data files entirely.
    pub async fn delete(&self, app: &str, drain_timeout: Duration) {
        self.stop(app, drain_timeout).await;
        let path = self.workflows_db_path(app);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_file_name(format!("{WORKFLOWS_DB_FILENAME}-wal")));
        let _ = std::fs::remove_file(path.with_file_name(format!("{WORKFLOWS_DB_FILENAME}-shm")));
    }

    /// Use the active runtime's store, or open the app's store for this call
    /// so stopped apps stay inspectable. `None` when this server has no
    /// SQLite store for the app; opening one would create an empty file.
    fn operator_store(&self, app: &str) -> Result<Option<OperatorStore>, WorkflowManagerError> {
        let lease = {
            let mut gate = self.store_gate.lock();
            if gate.closed.contains(app) {
                return Err(WorkflowManagerError::InvalidRequest(format!(
                    "The workflow store of {app} is closed while a backup is restored; try again when the restore finishes"
                )));
            }
            *gate.operator_stores.entry(app.to_string()).or_default() += 1;
            OperatorLease {
                gate: self.store_gate.clone(),
                app: app.to_string(),
            }
        };
        if let Some(handlers) = self.apps.read().get(app).map(AppWorkflow::handlers) {
            return Ok(Some(OperatorStore {
                db: handlers.db.clone(),
                handlers: Some(handlers),
                _lease: lease,
            }));
        }
        let config = self.workflow_store_config(app);
        if let WorkflowStoreConfig::Sqlite { path } = &config
            && !path.exists()
        {
            return Ok(None);
        }
        Ok(Some(OperatorStore {
            db: Arc::new(RunsDb::open_config(config)?),
            handlers: None,
            _lease: lease,
        }))
    }

    fn store_kind(&self, app: &str) -> WorkflowStoreKind {
        match self.workflow_store_config(app) {
            WorkflowStoreConfig::Sqlite { .. } => WorkflowStoreKind::Sqlite,
            WorkflowStoreConfig::Postgres { .. } => WorkflowStoreKind::Postgres,
        }
    }

    /// Newest runs first. `limit` defaults to 50 and must be 1–200.
    pub async fn list_runs(
        self: &Arc<Self>,
        app: &str,
        status: Option<WorkflowRunStatus>,
        name: Option<String>,
        limit: Option<u32>,
    ) -> Result<ListWorkflowRunsResponse, WorkflowManagerError> {
        let limit = limit.unwrap_or(WORKFLOW_RUNS_DEFAULT_LIMIT);
        if !(1..=WORKFLOW_RUNS_MAX_LIMIT).contains(&limit) {
            return Err(WorkflowManagerError::InvalidRequest(format!(
                "Workflow run list limit must be between 1 and {WORKFLOW_RUNS_MAX_LIMIT}"
            )));
        }
        let manager = self.clone();
        let app = app.to_string();
        crate::blocking::run(move || {
            let Some(store) = manager.operator_store(&app)? else {
                return Ok(ListWorkflowRunsResponse {
                    store: manager.store_kind(&app),
                    app,
                    runs: Vec::new(),
                    has_more: false,
                });
            };
            let (runs, has_more) = store.db.list_runs(status, name.as_deref(), limit)?;
            Ok(ListWorkflowRunsResponse {
                store: store.db.store_kind(),
                app,
                runs,
                has_more,
            })
        })
        .await?
    }

    pub async fn get_run(
        self: &Arc<Self>,
        app: &str,
        id: &str,
    ) -> Result<GetWorkflowRunResponse, WorkflowManagerError> {
        let manager = self.clone();
        let (app, id) = (app.to_string(), id.to_string());
        crate::blocking::run(move || {
            let Some(store) = manager.operator_store(&app)? else {
                return Ok(GetWorkflowRunResponse {
                    store: manager.store_kind(&app),
                    app,
                    run: None,
                });
            };
            Ok(GetWorkflowRunResponse {
                store: store.db.store_kind(),
                run: store.db.get_run(&id)?,
                app,
            })
        })
        .await?
    }

    /// See [`RunsDb::retry_run`]. Wakes this server's workers when it runs the app.
    pub async fn retry_run(
        self: &Arc<Self>,
        app: &str,
        id: &str,
    ) -> Result<WorkflowRunTransition, WorkflowManagerError> {
        let manager = self.clone();
        let (app, id) = (app.to_string(), id.to_string());
        crate::blocking::run(move || {
            let store = manager
                .operator_store(&app)?
                .ok_or_else(|| RunsDbError::RunNotFound { id: id.clone() })?;
            let previous_status = store.db.retry_run(&id)?;
            if let Some(handlers) = &store.handlers {
                (handlers.on_enqueue)();
            }
            Ok(WorkflowRunTransition {
                store: store.db.store_kind(),
                app,
                id,
                previous_status,
                status: WorkflowRunStatus::Pending,
            })
        })
        .await?
    }

    /// See [`RunsDb::cancel_run`]. Frees the fenced attempt's concurrency
    /// slot when this server's runtime holds it.
    pub async fn cancel_run(
        self: &Arc<Self>,
        app: &str,
        id: &str,
    ) -> Result<WorkflowRunTransition, WorkflowManagerError> {
        let manager = self.clone();
        let (app, id) = (app.to_string(), id.to_string());
        crate::blocking::run(move || {
            let store = manager
                .operator_store(&app)?
                .ok_or_else(|| RunsDbError::RunNotFound { id: id.clone() })?;
            let cancelled = store.db.cancel_run(&id)?;
            if let (Some(handlers), Some(worker_id)) = (&store.handlers, &cancelled.worker_id) {
                handlers.limiter.release_claim(worker_id);
            }
            Ok(WorkflowRunTransition {
                store: store.db.store_kind(),
                app,
                id,
                previous_status: cancelled.previous_status,
                status: WorkflowRunStatus::Cancelled,
            })
        })
        .await?
    }

    pub fn supervisor_for(&self, app: &str) -> Option<Arc<WorkerSupervisor>> {
        self.apps.read().get(app).map(|e| e.supervisor())
    }

    pub fn has(&self, app: &str) -> bool {
        self.apps.read().contains_key(app)
    }

    pub(crate) fn monitored_stores(&self) -> Vec<(String, Arc<RunsDb>)> {
        self.apps
            .read()
            .iter()
            .map(|(app, entry)| (app.clone(), entry.handlers.db.clone()))
            .collect()
    }

    /// Shut down every app, then the socket. Called on server shutdown.
    pub async fn shutdown_all(&self, drain_timeout: Duration) {
        let apps: Vec<(String, AppWorkflow)> = {
            let mut guard = self.apps.write();
            guard.drain().collect()
        };
        for (_, entry) in apps {
            entry.shutdown(drain_timeout).await;
        }
        let socket = self.socket.lock().take();
        if let Some(handle) = socket {
            handle.shutdown().await;
        }
    }
}

/// Build a Bun worker spec. The shared socket path comes from the manager
/// (`socket_path()`); we pass it via env var so the SDK's `WorkflowsClient`
/// can connect.
#[allow(clippy::too_many_arguments)]
pub fn worker_spec_for_bun(
    app: &str,
    workers: u32,
    concurrency: u32,
    idle_timeout_ms: u64,
    internal_socket: &Path,
    bun_path: &Path,
    worker_entry: &Path,
    app_cwd: &Path,
    mut env: std::collections::HashMap<String, String>,
    secrets: std::collections::HashMap<String, String>,
    storages: std::collections::HashMap<String, tako_core::StorageBinding>,
    isolation: Option<tako_spawn::ProcessIsolation>,
) -> WorkerSpec {
    env.entry("NODE_ENV".to_string())
        .or_insert_with(|| "production".to_string());

    worker_spec_for_command(
        app,
        workers,
        concurrency,
        idle_timeout_ms,
        internal_socket,
        vec![bun_path.into(), worker_entry.into()],
        app_cwd,
        env,
        secrets,
        storages,
        isolation,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn worker_spec_for_command(
    app: &str,
    workers: u32,
    concurrency: u32,
    idle_timeout_ms: u64,
    internal_socket: &Path,
    command: Vec<std::ffi::OsString>,
    app_cwd: &Path,
    mut env: std::collections::HashMap<String, String>,
    secrets: std::collections::HashMap<String, String>,
    storages: std::collections::HashMap<String, tako_core::StorageBinding>,
    isolation: Option<tako_spawn::ProcessIsolation>,
) -> WorkerSpec {
    env.insert(
        tako_core::instance_env::TAKO_APP_NAME_ENV.into(),
        app.to_string(),
    );
    env.insert(
        tako_core::instance_env::TAKO_INTERNAL_SOCKET_ENV.into(),
        internal_socket.to_string_lossy().to_string(),
    );

    WorkerSpec {
        app: app.to_string(),
        workers,
        concurrency,
        idle_timeout_ms,
        command,
        cwd: app_cwd.to_path_buf(),
        env,
        secrets,
        storages,
        log_sink: None,
        isolation,
        lanes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::POSTGRES_WORKFLOWS_SCHEMA;
    use crate::enqueue_socket::{HealthCheck, OnClaimed};
    use std::collections::HashMap as StdHashMap;

    fn dummy_spec(cwd: PathBuf, _db: PathBuf) -> WorkerSpec {
        WorkerSpec {
            app: "t".into(),
            workers: 0,
            concurrency: 1,
            idle_timeout_ms: 0,
            command: vec!["sleep".into(), "10".into()],
            cwd,
            env: StdHashMap::new(),
            secrets: StdHashMap::new(),
            storages: StdHashMap::new(),
            log_sink: None,
            isolation: None,
            lanes: Vec::new(),
        }
    }

    fn invalid_start_spec(cwd: PathBuf, _db: PathBuf) -> WorkerSpec {
        WorkerSpec {
            app: "t".into(),
            workers: 1,
            concurrency: 1,
            idle_timeout_ms: 0,
            command: Vec::new(),
            cwd,
            env: StdHashMap::new(),
            secrets: StdHashMap::new(),
            storages: StdHashMap::new(),
            log_sink: None,
            isolation: None,
            lanes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn server_drain_stops_claims_and_backgrounds_and_can_resume_without_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let m = WorkflowManager::new(tmp.path());
        for name in ["a", "b"] {
            m.ensure(name, None, |db| dummy_spec(tmp.path().into(), db))
                .await
                .unwrap();
        }
        let handlers = m.apps.read()["a"].handlers();
        m.begin_server_drain();
        assert!(!handlers.accepting_new_work.load(Ordering::SeqCst));
        m.supervisor_for("a").unwrap().wake().unwrap();
        assert!(!m.supervisor_for("a").unwrap().is_running());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !m.server_drain_status().is_complete() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(m.force_server_drain(), (0, 0));
        assert!(m.has("a"));
        m.ensure("a", None, |db| dummy_spec(tmp.path().into(), db))
            .await
            .unwrap();
        assert!(
            m.apps.read()["a"]
                .handlers()
                .accepting_new_work
                .load(Ordering::SeqCst)
        );
        m.begin_server_drain();
    }

    #[tokio::test]
    async fn server_drain_includes_retired_release_workers() {
        let tmp = tempfile::tempdir().unwrap();
        let m = WorkflowManager::new(tmp.path());
        m.ensure("a", None, |db| {
            let mut spec = dummy_spec(tmp.path().into(), db);
            spec.command = vec![
                "/bin/sh".into(),
                "-c".into(),
                "trap '' TERM; touch ready; exec sleep 60".into(),
            ];
            spec
        })
        .await
        .unwrap();
        let old = m.supervisor_for("a").unwrap();
        old.wake().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !tmp.path().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        m.ensure("a", None, |db| dummy_spec(tmp.path().into(), db))
            .await
            .unwrap();
        m.begin_server_drain();
        assert_eq!(m.server_drain_status().workers, 1);
        assert_eq!(m.force_server_drain(), (1, 0));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !m.server_drain_status().is_complete() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(!old.is_running());
    }

    /// A deploy that replaces a runtime leaves its workers finishing runs
    /// against the store. Stopping the app leaves them alone; closing the
    /// store for a restore must not.
    #[tokio::test]
    async fn close_store_stops_retired_workers_and_releases_every_store_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let m = Arc::new(WorkflowManager::new(tmp.path()));
        let stubborn = |db| {
            let mut spec = dummy_spec(tmp.path().into(), db);
            spec.command = vec![
                "/bin/sh".into(),
                "-c".into(),
                "trap '' TERM; touch ready; exec sleep 60".into(),
            ];
            spec
        };
        let start_retired = || async {
            m.ensure("a", None, stubborn).await.unwrap();
            let old = m.supervisor_for("a").unwrap();
            old.wake().unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while !tmp.path().join("ready").exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            std::fs::remove_file(tmp.path().join("ready")).unwrap();
            m.ensure("a", None, |db| dummy_spec(tmp.path().into(), db))
                .await
                .unwrap();
            let store = Arc::downgrade(&m.retired.lock().last().unwrap().db.upgrade().unwrap());
            (old, store)
        };

        let (old, _) = start_retired().await;
        m.stop("a", Duration::from_millis(100)).await;
        assert!(old.is_running(), "stop leaves a retired worker running");
        old.force_stop();
        m.retired.lock().clear();

        let (old, retired_store) = start_retired().await;
        let current_store = Arc::downgrade(&m.apps.read()["a"].handlers.db);
        let closed = tokio::time::timeout(
            Duration::from_secs(5),
            m.close_store("a", Duration::from_millis(100)),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!old.is_running());
        assert!(!m.has("a"));
        assert!(m.retired.lock().is_empty());
        assert_eq!(retired_store.strong_count(), 0);
        assert_eq!(current_store.strong_count(), 0);
        let error = m.list_runs("a", None, None, None).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("is closed while a backup is restored")
        );

        // A restart may bring the runtime back while the store is closed.
        m.ensure("a", None, |db| dummy_spec(tmp.path().into(), db))
            .await
            .unwrap();
        drop(closed);
        assert!(m.list_runs("a", None, None, None).await.is_ok());
        m.delete("a", Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn close_store_waits_for_operator_commands_and_fails_on_a_lingering_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let m = WorkflowManager::new(tmp.path());
        m.ensure("a", None, |db| dummy_spec(tmp.path().into(), db))
            .await
            .unwrap();
        let lingering = m.apps.read()["a"].handlers.db.clone();
        let lease = m.operator_store("a").unwrap().unwrap();
        drop(lingering);
        let close = m.close_store("a", Duration::from_millis(100));
        tokio::pin!(close);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut close)
                .await
                .is_err(),
            "an operator command still holds the store"
        );
        drop(lease);
        let closed = close.await.unwrap();
        drop(closed);

        m.ensure("a", None, |db| dummy_spec(tmp.path().into(), db))
            .await
            .unwrap();
        let _lingering = m.apps.read()["a"].handlers.db.clone();
        let error = m
            .close_store("a", Duration::from_millis(100))
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("is still open"), "{error}");
        // The failed close reopens the store for operator commands.
        assert!(m.operator_store("a").is_ok());
    }

    #[tokio::test]
    async fn operator_commands_use_the_active_runtime_or_the_stored_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let m = Arc::new(WorkflowManager::new(tmp.path()));

        // No store on this server: nothing to show, and no file is created.
        let list = m.list_runs("a", None, None, None).await.unwrap();
        assert_eq!(
            (list.store, list.runs.len()),
            (WorkflowStoreKind::Sqlite, 0)
        );
        assert!(m.get_run("a", "r1").await.unwrap().run.is_none());
        for error in [
            m.retry_run("a", "r1").await.unwrap_err(),
            m.cancel_run("a", "r1").await.unwrap_err(),
        ] {
            assert_eq!(error.to_string(), "db error: Workflow run not found: r1");
        }
        assert!(!m.workflows_db_path("a").exists());
        for limit in [0, WORKFLOW_RUNS_MAX_LIMIT + 1] {
            assert_eq!(
                m.list_runs("a", None, None, Some(limit))
                    .await
                    .unwrap_err()
                    .to_string(),
                "Workflow run list limit must be between 1 and 200"
            );
        }

        m.ensure("a", None, |db| dummy_spec(tmp.path().into(), db))
            .await
            .unwrap();
        let handlers = m.apps.read()["a"].handlers();
        for id in ["r1", "r2"] {
            let options = tako_core::EnqueueOpts {
                id: Some(id.into()),
                ..Default::default()
            };
            handlers
                .db
                .enqueue("work", &serde_json::json!({}), &options)
                .unwrap();
        }
        let claimed = handlers
            .limiter
            .claim(&handlers.db, "worker", &["work".into()], 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(handlers.limiter.count("worker"), 1);

        // Cancelling the running attempt frees its concurrency slot.
        let cancelled = m.cancel_run("a", &claimed.id).await.unwrap();
        assert_eq!(cancelled.previous_status, WorkflowRunStatus::Running);
        assert_eq!(cancelled.status, WorkflowRunStatus::Cancelled);
        assert_eq!(handlers.limiter.count("worker"), 0);
        let listed = m
            .list_runs("a", Some(WorkflowRunStatus::Cancelled), None, Some(1))
            .await
            .unwrap();
        assert_eq!(listed.runs[0].id, claimed.id);
        assert!(!listed.has_more);

        // A stopped app's runs stay inspectable and repairable.
        m.stop("a", Duration::from_secs(1)).await;
        let retried = m.retry_run("a", &claimed.id).await.unwrap();
        assert_eq!(retried.previous_status, WorkflowRunStatus::Cancelled);
        let shown = m.get_run("a", &claimed.id).await.unwrap().run.unwrap();
        assert_eq!(
            (shown.status, shown.attempts),
            (WorkflowRunStatus::Pending, 0)
        );
        assert_eq!(
            m.list_runs("a", None, None, None).await.unwrap().runs.len(),
            2
        );
    }

    /// Two servers share one Postgres store. The server that does not run
    /// the attempt can still cancel it, and the attempt is fenced.
    #[tokio::test]
    async fn operator_commands_reach_runs_of_other_servers_in_a_shared_postgres_store() {
        let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
            return;
        };
        let app = format!("operator-{}", nanoid::nanoid!(8, &['a', 'b', 'c', 'd']));
        let servers: Vec<_> = (0..2)
            .map(|_| {
                let tmp = tempfile::tempdir().unwrap();
                let m = Arc::new(WorkflowManager::new(tmp.path()));
                let url = url.clone();
                m.set_postgres_url_resolver(Arc::new(move |_| Some(url.clone())));
                (tmp, m)
            })
            .collect();
        let (running, operator) = (&servers[0].1, &servers[1].1);
        running
            .ensure(&app, None, |db| dummy_spec(servers[0].0.path().into(), db))
            .await
            .unwrap();
        let handlers = running.apps.read()[&app].handlers();
        // The synchronous Postgres client must stay off runtime threads.
        let blocking = |f: Box<dyn FnOnce(&RunsDb) -> String + Send>| {
            let db = handlers.db.clone();
            async move { tokio::task::spawn_blocking(move || f(&db)).await.unwrap() }
        };
        let id = blocking(Box::new(|db| {
            db.enqueue("work", &serde_json::json!({}), &Default::default())
                .unwrap()
                .id
        }))
        .await;
        let owner = blocking(Box::new(|db| {
            db.claim("worker", &["work".into()], 60_000)
                .unwrap()
                .unwrap()
                .worker_id
        }))
        .await;

        let list = operator.list_runs(&app, None, None, None).await.unwrap();
        assert_eq!(list.store, WorkflowStoreKind::Postgres);
        assert_eq!(list.runs[0].id, id);
        let cancelled = operator.cancel_run(&app, &id).await.unwrap();
        assert_eq!(cancelled.previous_status, WorkflowRunStatus::Running);
        let (late_id, late_owner) = (id.clone(), owner.clone());
        let late = blocking(Box::new(move |db| {
            db.save_step(&late_id, &late_owner, "late", &serde_json::json!(1))
                .unwrap_err()
                .to_string()
        }))
        .await;
        assert_eq!(late, RunsDbError::StaleWorker.to_string());
        assert_eq!(
            running
                .get_run(&app, &id)
                .await
                .unwrap()
                .run
                .unwrap()
                .status,
            WorkflowRunStatus::Cancelled
        );
        running.stop(&app, Duration::from_secs(1)).await;
        // The last store handle is released in async code.
        drop(handlers);

        tokio::task::spawn_blocking(move || {
            let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
            for table in ["steps", "event_waiters", "runs"] {
                client
                    .execute(
                        &format!("DELETE FROM {POSTGRES_WORKFLOWS_SCHEMA}.{table} WHERE app_id=$1"),
                        &[&app],
                    )
                    .unwrap();
            }
        })
        .await
        .unwrap();
    }

    /// The synchronous Postgres client panics when it closes on a runtime
    /// thread. Each lifecycle path below releases the store's last handle
    /// in async code.
    #[tokio::test]
    async fn postgres_store_survives_every_lifecycle_path_that_releases_it() {
        let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let m = WorkflowManager::new(tmp.path());
        m.set_postgres_url_resolver(Arc::new(move |_| Some(url.clone())));
        let app = format!("lifecycle-{}", nanoid::nanoid!(8, &['a', 'b', 'c', 'd']));
        let cwd = tmp.path().to_path_buf();
        let start = || m.ensure(&app, None, |db| dummy_spec(cwd.clone(), db));

        // A replacement that fails to start is the only holder of its store.
        m.ensure(&app, None, |db| invalid_start_spec(cwd.clone(), db))
            .await
            .unwrap_err();

        start().await.unwrap();
        m.stop(&app, Duration::from_secs(1)).await;

        start().await.unwrap();
        start().await.unwrap();
        let replaced = m.retired.lock().pop().unwrap();
        replaced.task.await.unwrap();

        m.retire(&app);
        tokio::time::timeout(Duration::from_secs(2), async {
            while m.has(&app) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        start().await.unwrap();
        m.shutdown_all(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn ensure_creates_db_and_supervisor() {
        let tmp = tempfile::tempdir().unwrap();
        let m = WorkflowManager::new(tmp.path());
        let cwd = tmp.path().to_path_buf();

        m.ensure("a", Some(crate::DEFAULT_RETENTION_MS), |db| {
            dummy_spec(cwd.clone(), db)
        })
        .await
        .unwrap();
        assert!(m.has("a"));
        assert_eq!(
            m.workflows_db_path("a"),
            tmp.path()
                .join("apps")
                .join("a")
                .join("data")
                .join("tako")
                .join("workflows.sqlite")
        );
        assert!(m.workflows_db_path("a").exists());

        m.delete("a", Duration::from_secs(1)).await;
    }

    #[test]
    fn workflow_store_config_uses_local_sqlite_path() {
        let tmp = tempfile::tempdir().unwrap();
        let m = WorkflowManager::new(tmp.path());

        assert_eq!(
            m.workflow_store_config("a"),
            WorkflowStoreConfig::Sqlite {
                path: tmp
                    .path()
                    .join("apps")
                    .join("a")
                    .join("data")
                    .join("tako")
                    .join("workflows.sqlite"),
            },
        );
    }

    #[test]
    fn workflow_store_config_uses_postgres_when_resolver_returns_url() {
        let tmp = tempfile::tempdir().unwrap();
        let m = WorkflowManager::new(tmp.path());
        m.set_postgres_url_resolver(Arc::new(|_| Some("postgres://db".to_string())));

        assert_eq!(
            m.workflow_store_config("a/production"),
            WorkflowStoreConfig::Postgres {
                url: "postgres://db".to_string(),
                schema: POSTGRES_WORKFLOWS_SCHEMA.to_string(),
                app_id: "a/production".to_string(),
            },
        );
    }

    #[tokio::test]
    async fn ensure_replaces_existing_runtime_without_deleting_db() {
        let tmp = tempfile::tempdir().unwrap();
        let m = WorkflowManager::new(tmp.path());
        let cwd = tmp.path().to_path_buf();

        m.ensure("a", Some(crate::DEFAULT_RETENTION_MS), |db| {
            dummy_spec(cwd.clone(), db)
        })
        .await
        .unwrap();
        let first = m.supervisor_for("a").unwrap();
        m.ensure("a", Some(crate::DEFAULT_RETENTION_MS), |db| {
            dummy_spec(cwd.clone(), db)
        })
        .await
        .unwrap();
        let second = m.supervisor_for("a").unwrap();
        assert!(m.has("a"));
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(m.workflows_db_path("a").exists());
        m.delete("a", Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn ensure_keeps_existing_runtime_when_replacement_fails_to_start() {
        let tmp = tempfile::tempdir().unwrap();
        let m = WorkflowManager::new(tmp.path());
        let cwd = tmp.path().to_path_buf();

        m.ensure("a", Some(crate::DEFAULT_RETENTION_MS), |db| {
            dummy_spec(cwd.clone(), db)
        })
        .await
        .unwrap();
        let first = m.supervisor_for("a").unwrap();
        let err = m
            .ensure("a", Some(crate::DEFAULT_RETENTION_MS), |db| {
                invalid_start_spec(cwd.clone(), db)
            })
            .await
            .unwrap_err();

        assert!(matches!(err, WorkflowManagerError::Supervisor(_)));
        let current = m.supervisor_for("a").unwrap();
        assert!(Arc::ptr_eq(&first, &current));
        assert!(m.has("a"));
        m.delete("a", Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn stop_keeps_db_file_delete_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let m = WorkflowManager::new(tmp.path());
        let cwd = tmp.path().to_path_buf();

        m.ensure("a", Some(crate::DEFAULT_RETENTION_MS), |db| {
            dummy_spec(cwd.clone(), db)
        })
        .await
        .unwrap();
        m.stop("a", Duration::from_secs(1)).await;
        assert!(!m.has("a"));
        assert!(m.workflows_db_path("a").exists());

        m.ensure("a", Some(crate::DEFAULT_RETENTION_MS), |db| {
            dummy_spec(cwd.clone(), db)
        })
        .await
        .unwrap();
        m.delete("a", Duration::from_secs(1)).await;
        assert!(!m.workflows_db_path("a").exists());
    }

    #[tokio::test]
    async fn shutdown_all_clears_every_app_and_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let m = WorkflowManager::new(tmp.path());
        m.start_socket().unwrap();
        let cwd = tmp.path().to_path_buf();

        for name in ["a", "b", "c"] {
            m.ensure(name, Some(crate::DEFAULT_RETENTION_MS), |db| {
                dummy_spec(cwd.clone(), db)
            })
            .await
            .unwrap();
        }
        m.shutdown_all(Duration::from_secs(1)).await;
        for name in ["a", "b", "c"] {
            assert!(!m.has(name));
        }
        assert!(m.socket.lock().is_none());
    }

    #[test]
    fn app_workflow_reuses_prebuilt_handlers() {
        let db = Arc::new(RunsDb::open_in_memory().unwrap());
        let limiter = Arc::new(InFlightLimiter::new(1));
        let accepting_new_work = Arc::new(AtomicBool::new(true));
        let supervisor = Arc::new(WorkerSupervisor::new(dummy_spec(
            std::env::temp_dir(),
            std::env::temp_dir().join("workflows.sqlite"),
        )));
        let on_enqueue: OnEnqueue = Arc::new(|| {});
        let health_check: HealthCheck = Arc::new(|| Ok(()));
        let on_claimed: OnClaimed = Arc::new(|| {});

        let workflow = AppWorkflow {
            handlers: AppHandlers {
                db,
                limiter,
                accepting_new_work: accepting_new_work.clone(),
                on_enqueue,
                health_check,
                on_claimed,
            },
            supervisor,
            dispatcher: None,
            cron: None,
            accepting_new_work,
        };

        let first = workflow.handlers();
        let second = workflow.handlers();

        assert!(Arc::ptr_eq(&first.db, &second.db));
        assert!(Arc::ptr_eq(&first.limiter, &second.limiter));
        assert!(Arc::ptr_eq(&first.on_enqueue, &second.on_enqueue));
        assert!(Arc::ptr_eq(&first.health_check, &second.health_check));
        assert!(Arc::ptr_eq(&first.on_claimed, &second.on_claimed));
    }
}

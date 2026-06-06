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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::RwLock;

use super::cron::{self, CronTickerHandle};
use super::dispatcher::WorkDispatcher;
use super::enqueue::{RunsDb, RunsDbError, WorkflowStoreConfig};
use super::enqueue_socket::{
    AppHandlers, AppLookup, ChannelPublishFn, EnqueueSocketHandle, OnEnqueue,
    spawn as spawn_internal_socket,
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

pub struct WorkflowManager {
    data_dir: PathBuf,
    apps: Arc<RwLock<HashMap<String, AppWorkflow>>>,
    socket: parking_lot::Mutex<Option<EnqueueSocketHandle>>,
    postgres_url: parking_lot::Mutex<Option<PostgresUrlResolver>>,
    /// Server-side channel `publish` relay used by SDK channel handles.
    /// Snapshotted at `start_socket` time and passed into the socket's
    /// accept loop.
    channel_publish: parking_lot::Mutex<Option<ChannelPublishFn>>,
    /// Serializes `ensure` calls so two concurrent deploys of the same app
    /// can't each start their own supervisor + cron ticker and then have
    /// one silently overwrite the other (leaking the loser's children).
    ensure_gate: tokio::sync::Mutex<()>,
}

type PostgresUrlResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[derive(thiserror::Error, Debug)]
pub enum WorkflowManagerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("db error: {0}")]
    Db(#[from] RunsDbError),
    #[error("supervisor error: {0}")]
    Supervisor(#[from] super::supervisor::SupervisorError),
}

impl WorkflowManager {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            apps: Arc::new(RwLock::new(HashMap::new())),
            socket: parking_lot::Mutex::new(None),
            postgres_url: parking_lot::Mutex::new(None),
            channel_publish: parking_lot::Mutex::new(None),
            ensure_gate: tokio::sync::Mutex::new(()),
        }
    }

    /// Install the channel-publish relay used by server-side channel
    /// `publish()` calls arriving on the internal socket. Must be called before
    /// `start_socket` — the publisher is snapshotted at that point.
    pub fn set_channel_publisher(&self, publisher: ChannelPublishFn) {
        *self.channel_publish.lock() = Some(publisher);
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
        *guard = Some(spawn_internal_socket(
            self.socket_path(),
            lookup,
            publisher,
        )?);
        Ok(())
    }

    /// Configure or reconfigure workflows for an app. Called on deploy.
    /// The replacement runtime is built before swapping it into the lookup map,
    /// so a failed replacement keeps the existing runtime active.
    pub async fn ensure(
        &self,
        app: &str,
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
        let db = Arc::new(RunsDb::open_config(store_config)?);

        let spec = spec_fn(db_path);
        let limiter = Arc::new(InFlightLimiter::new(spec.concurrency));
        // Rehydrate: any rows still `status='running'` from a previous
        // process count against the live budget until their leases expire
        // and `reclaim_expired_with_workers` drops them.
        if let Ok(snapshot) = db.in_flight_by_worker() {
            limiter.rehydrate(snapshot);
        }
        let supervisor = Arc::new(WorkerSupervisor::new(spec));
        supervisor.start().await?;

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
        let cron_handle = cron::spawn_with_limiter(db.clone(), limiter.clone(), on_enqueue.clone());

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

        if let Some(old_entry) = old_entry {
            tokio::spawn(old_entry.drain_existing_work());
        }
        Ok(())
    }

    /// Stop the worker but keep the DB around (app is paused).
    pub async fn stop(&self, app: &str, drain_timeout: Duration) {
        let entry = self.apps.write().remove(app);
        if let Some(entry) = entry {
            entry.shutdown(drain_timeout).await;
        }
    }

    /// Retire the active workflow runtime without killing in-flight runs.
    /// New enqueues/claims stop immediately; running workers receive SIGTERM,
    /// drain, and can still write lifecycle updates through the retained
    /// handlers until they exit.
    pub async fn retire(&self, app: &str) {
        let Some(mut entry) = self.apps.write().remove(app) else {
            return;
        };

        entry.accepting_new_work.store(false, Ordering::SeqCst);
        if let Some(cron) = entry.cron.take() {
            cron.shutdown().await;
        }
        if let Some(dispatcher) = entry.dispatcher.take() {
            dispatcher.shutdown().await;
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

        self.apps.write().insert(app.to_string(), draining_entry);

        let apps = self.apps.clone();
        let app = app.to_string();
        tokio::spawn(async move {
            entry.supervisor.shutdown_gracefully().await;
            let mut apps = apps.write();
            if apps
                .get(&app)
                .is_some_and(|current| Arc::ptr_eq(&current.supervisor, &entry.supervisor))
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

    pub fn supervisor_for(&self, app: &str) -> Option<Arc<WorkerSupervisor>> {
        self.apps.read().get(app).map(|e| e.supervisor())
    }

    pub fn has(&self, app: &str) -> bool {
        self.apps.read().contains_key(app)
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
    env.insert(
        tako_core::instance_env::TAKO_APP_NAME_ENV.into(),
        app.to_string(),
    );
    env.insert(
        tako_core::instance_env::TAKO_INTERNAL_SOCKET_ENV.into(),
        internal_socket.to_string_lossy().to_string(),
    );
    env.entry("NODE_ENV".to_string())
        .or_insert_with(|| "production".to_string());

    WorkerSpec {
        app: app.to_string(),
        workers,
        concurrency,
        idle_timeout_ms,
        command: vec![bun_path.into(), worker_entry.into()],
        cwd: app_cwd.to_path_buf(),
        env,
        secrets,
        storages,
        log_sink: None,
        isolation,
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
        }
    }

    #[tokio::test]
    async fn ensure_creates_db_and_supervisor() {
        let tmp = tempfile::tempdir().unwrap();
        let m = WorkflowManager::new(tmp.path());
        let cwd = tmp.path().to_path_buf();

        m.ensure("a", |db| dummy_spec(cwd.clone(), db))
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

        m.ensure("a", |db| dummy_spec(cwd.clone(), db))
            .await
            .unwrap();
        let first = m.supervisor_for("a").unwrap();
        m.ensure("a", |db| dummy_spec(cwd.clone(), db))
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

        m.ensure("a", |db| dummy_spec(cwd.clone(), db))
            .await
            .unwrap();
        let first = m.supervisor_for("a").unwrap();
        let err = m
            .ensure("a", |db| invalid_start_spec(cwd.clone(), db))
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

        m.ensure("a", |db| dummy_spec(cwd.clone(), db))
            .await
            .unwrap();
        m.stop("a", Duration::from_secs(1)).await;
        assert!(!m.has("a"));
        assert!(m.workflows_db_path("a").exists());

        m.ensure("a", |db| dummy_spec(cwd.clone(), db))
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
            m.ensure(name, |db| dummy_spec(cwd.clone(), db))
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

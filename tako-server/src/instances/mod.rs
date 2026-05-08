//! Instance lifecycle management
//!
//! Manages app instances - spawning, health checking, and cleanup.

mod health;
pub mod logger;
mod network;
mod rolling;
mod spawner;

pub use health::*;
pub use logger::{AppLogHandle, LogStream, log_pipe, spawn_app_logger};
pub use network::*;
pub use rolling::*;
pub use spawner::*;

use crate::socket::{AppState, InstanceState, InstanceStatus};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Child;
use tokio::sync::mpsc;

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub const INTERNAL_STATUS_HOST: &str = "tako.internal";
pub const INTERNAL_TOKEN_HEADER: &str = "X-Tako-Internal-Token";

/// Generate a short random instance ID
fn generate_instance_id() -> String {
    nanoid::nanoid!(8)
}

fn generate_internal_token() -> String {
    nanoid::nanoid!(32)
}

/// Configuration for an app
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// App name
    pub name: String,
    /// Deployment environment
    pub environment: String,
    /// Current version
    pub version: String,
    /// Derived path to the active app directory
    pub path: PathBuf,
    /// Runtime command derived from app.json
    pub command: Vec<String>,
    /// Non-secret environment variables (read from app.json in release dir)
    pub env_vars: HashMap<String, String>,
    /// Secret environment variables (loaded from encrypted server state)
    pub secrets: HashMap<String, String>,
    /// Minimum instances (0 = on-demand)
    pub min_instances: u32,
    /// Maximum instances
    pub max_instances: u32,
    /// Health check path
    pub health_check_path: String,
    /// Health check host header
    pub health_check_host: String,
    /// Startup timeout
    pub startup_timeout: Duration,
    /// Idle timeout (for on-demand scaling)
    pub idle_timeout: Duration,
}

impl AppConfig {
    pub fn deployment_id(&self) -> String {
        if self.environment.is_empty() {
            return self.name.clone();
        }
        tako_core::deployment_app_id(&self.name, &self.environment)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            environment: String::new(),
            version: String::new(),
            path: PathBuf::new(),
            command: vec![],
            env_vars: HashMap::new(),
            secrets: HashMap::new(),
            min_instances: 1,
            max_instances: 4,
            health_check_path: "/status".to_string(),
            health_check_host: INTERNAL_STATUS_HOST.to_string(),
            startup_timeout: Duration::from_secs(30),
            idle_timeout: crate::defaults::DEFAULT_IDLE_TIMEOUT,
        }
    }
}

/// A running instance of an app
pub struct Instance {
    /// Unique instance ID
    pub id: String,
    /// Build version this instance was launched from
    build_version: String,
    /// Shared secret for internal status and secret-delivery requests.
    internal_token: String,
    /// Upstream endpoint and runtime cleanup metadata.
    upstream: RwLock<Option<PreparedInstanceNetwork>>,
    /// Process handle
    process: RwLock<Option<Child>>,
    /// Process ID
    pid: AtomicU32,
    /// Current state
    state: RwLock<InstanceState>,
    /// When the instance started
    started_at: RwLock<Option<Instant>>,
    /// Total requests handled
    requests_total: AtomicU64,

    /// In-flight requests (best-effort; used to avoid killing while serving)
    in_flight: AtomicU64,
    /// Last request completion time as millis since UNIX_EPOCH (for idle timeout)
    last_request_ms: AtomicU64,
    /// Last health-check heartbeat time as millis since UNIX_EPOCH
    last_heartbeat_ms: AtomicU64,

    /// Log handle for forwarding stdout/stderr to the app log writer.
    log_handle: AppLogHandle,
}

impl Instance {
    pub fn new(id: String, build_version: String, log_handle: AppLogHandle) -> Self {
        Self {
            id,
            build_version,
            internal_token: generate_internal_token(),
            upstream: RwLock::new(None),
            process: RwLock::new(None),
            pid: AtomicU32::new(0),
            state: RwLock::new(InstanceState::Starting),
            started_at: RwLock::new(None),
            requests_total: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            last_request_ms: AtomicU64::new(now_unix_millis()),
            last_heartbeat_ms: AtomicU64::new(now_unix_millis()),
            log_handle,
        }
    }

    pub fn state(&self) -> InstanceState {
        *self.state.read()
    }

    pub fn set_state(&self, state: InstanceState) {
        *self.state.write() = state;
    }

    pub fn pid(&self) -> Option<u32> {
        let pid = self.pid.load(Ordering::Relaxed);
        if pid > 0 { Some(pid) } else { None }
    }

    pub fn build_version(&self) -> &str {
        &self.build_version
    }

    #[cfg(test)]
    pub fn port(&self) -> Option<u16> {
        self.endpoint().map(|endpoint| endpoint.port())
    }

    pub fn endpoint(&self) -> Option<SocketAddr> {
        self.upstream
            .read()
            .as_ref()
            .map(|upstream| upstream.endpoint().addr())
    }

    pub fn internal_token(&self) -> &str {
        &self.internal_token
    }

    pub fn set_pid(&self, pid: u32) {
        self.pid.store(pid, Ordering::Relaxed);
    }

    pub fn set_port(&self, port: u16) {
        *self.upstream.write() = Some(PreparedInstanceNetwork::host_loopback(port));
    }

    pub fn set_process(&self, child: Child) {
        if let Some(pid) = child.id() {
            self.set_pid(pid);
        }
        *self.process.write() = Some(child);
        *self.started_at.write() = Some(Instant::now());
    }

    pub fn take_process(&self) -> Option<Child> {
        self.process.write().take()
    }

    pub fn request_started(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    pub fn request_finished(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.last_request_ms
            .store(now_unix_millis(), Ordering::Relaxed);
    }

    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    pub fn requests_total(&self) -> u64 {
        self.requests_total.load(Ordering::Relaxed)
    }

    pub fn uptime(&self) -> Duration {
        self.started_at
            .read()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }

    pub fn idle_time(&self) -> Duration {
        let last_ms = self.last_request_ms.load(Ordering::Relaxed);
        let now_ms = now_unix_millis();
        Duration::from_millis(now_ms.saturating_sub(last_ms))
    }

    /// Record a heartbeat
    pub fn record_heartbeat(&self) {
        self.last_heartbeat_ms
            .store(now_unix_millis(), Ordering::Relaxed);
    }

    pub fn status(&self) -> InstanceStatus {
        InstanceStatus {
            id: self.id.clone(),
            state: self.state(),
            pid: self.pid(),
            uptime_secs: self.uptime().as_secs(),
            requests_total: self.requests_total(),
        }
    }

    /// Check if process is still running
    pub async fn is_alive(&self) -> bool {
        let mut process = self.process.write();
        if let Some(ref mut child) = *process {
            match child.try_wait() {
                Ok(Some(_)) => false, // Process exited
                Ok(None) => true,     // Still running
                Err(_) => false,      // Error checking
            }
        } else {
            false
        }
    }

    /// Start forwarding stdout/stderr to the app logger.
    /// Called after the instance becomes healthy.
    pub fn drain_pipes(&self) {
        let mut process = self.process.write();
        if let Some(ref mut child) = *process {
            if let Some(stdout) = child.stdout.take() {
                let lh = self.log_handle.clone();
                let id = self.id.clone();
                tokio::spawn(log_pipe(stdout, lh, id, LogStream::Stdout));
            }
            if let Some(stderr) = child.stderr.take() {
                let lh = self.log_handle.clone();
                let id = self.id.clone();
                tokio::spawn(log_pipe(stderr, lh, id, LogStream::Stderr));
            }
        }
    }

    /// Kill the process
    pub async fn kill(&self) -> Result<(), std::io::Error> {
        if let Some(mut child) = self.take_process() {
            child.kill().await?;
        }
        self.cleanup_upstream();
        self.set_state(InstanceState::Stopped);
        Ok(())
    }

    pub fn cleanup_upstream(&self) {
        if let Some(upstream) = self.upstream.write().take() {
            upstream.cleanup();
        }
    }
}

/// Manages all instances of an app
pub struct App {
    /// App configuration
    pub config: RwLock<AppConfig>,
    /// Running instances
    instances: DashMap<String, Arc<Instance>>,
    /// Current app state
    state: RwLock<AppState>,

    /// Most recent error message (if any)
    last_error: RwLock<Option<String>>,
    /// Channel to notify about instance changes
    instance_tx: mpsc::Sender<InstanceEvent>,
    /// Shared log handle for all instances of this app
    log_handle: AppLogHandle,
}

/// Events for instance lifecycle
#[derive(Debug)]
pub enum InstanceEvent {
    Started { app: String, instance_id: String },
    Ready { app: String, instance_id: String },
}

impl App {
    pub fn new(
        config: AppConfig,
        instance_tx: mpsc::Sender<InstanceEvent>,
        log_handle: AppLogHandle,
    ) -> Self {
        Self {
            config: RwLock::new(config),
            instances: DashMap::new(),
            state: RwLock::new(AppState::Stopped),
            last_error: RwLock::new(None),
            instance_tx,
            log_handle,
        }
    }

    pub fn name(&self) -> String {
        self.config.read().deployment_id()
    }

    pub fn version(&self) -> String {
        self.config.read().version.clone()
    }

    pub fn state(&self) -> AppState {
        *self.state.read()
    }

    pub fn set_state(&self, state: AppState) {
        *self.state.write() = state;
    }

    pub fn set_last_error(&self, message: impl Into<String>) {
        *self.last_error.write() = Some(message.into());
    }

    pub fn clear_last_error(&self) {
        *self.last_error.write() = None;
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.read().clone()
    }

    /// Get all healthy instances
    pub fn get_healthy_instances(&self) -> Vec<Arc<Instance>> {
        self.instances
            .iter()
            .filter(|entry| entry.value().state() == InstanceState::Healthy)
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Get instance by ID
    pub fn get_instance(&self, id: &str) -> Option<Arc<Instance>> {
        self.instances.get(id).map(|entry| entry.value().clone())
    }

    /// Get all instances
    pub fn get_instances(&self) -> Vec<Arc<Instance>> {
        self.instances
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Allocate a new instance (doesn't start it yet)
    pub fn allocate_instance(&self) -> Arc<Instance> {
        let id = generate_instance_id();
        let config = self.config.read();
        let instance = Arc::new(Instance::new(
            id.clone(),
            config.version.clone(),
            self.log_handle.clone(),
        ));
        self.instances.insert(id, instance.clone());
        instance
    }

    /// Remove an instance
    pub fn remove_instance(&self, id: &str) -> Option<Arc<Instance>> {
        self.instances.remove(id).map(|(_, v)| v)
    }

    /// Update configuration (for reloads/deploys)
    pub fn update_config(&self, config: AppConfig) {
        *self.config.write() = config;
    }
}

/// Manages all apps
pub struct AppManager {
    /// All registered apps
    apps: DashMap<String, Arc<App>>,
    /// Instance spawner
    spawner: Arc<Spawner>,
    /// Event channel sender
    event_tx: mpsc::Sender<InstanceEvent>,
    /// Event channel receiver (for the manager loop)
    event_rx: RwLock<Option<mpsc::Receiver<InstanceEvent>>>,
    /// Server data directory (for app log paths)
    data_dir: PathBuf,
}

impl AppManager {
    pub fn new(data_dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        let internal_socket = tako_workflows::internal_socket_path(&data_dir);
        Self {
            apps: DashMap::new(),
            spawner: Arc::new(Spawner::new().with_internal_socket(internal_socket)),
            event_tx: tx,
            event_rx: RwLock::new(Some(rx)),
            data_dir,
        }
    }

    /// Take the event receiver (can only be called once)
    pub fn take_event_receiver(&self) -> Option<mpsc::Receiver<InstanceEvent>> {
        self.event_rx.write().take()
    }

    /// Register a new app
    pub fn register_app(&self, config: AppConfig) -> Arc<App> {
        let name = config.deployment_id();
        let log_dir = self.data_dir.join("apps").join(&name).join("logs");
        let log_handle = spawn_app_logger(&name, log_dir);
        let app = Arc::new(App::new(config, self.event_tx.clone(), log_handle));
        self.apps.insert(name, app.clone());
        app
    }

    /// Get an app by name
    pub fn get_app(&self, name: &str) -> Option<Arc<App>> {
        self.apps.get(name).map(|entry| entry.value().clone())
    }

    pub fn data_dir(&self) -> &PathBuf {
        &self.data_dir
    }

    /// Remove an app
    pub fn remove_app(&self, name: &str) -> Option<Arc<App>> {
        self.apps.remove(name).map(|(_, v)| v)
    }

    /// List all app names
    pub fn list_apps(&self) -> Vec<String> {
        self.apps.iter().map(|entry| entry.key().clone()).collect()
    }

    /// Start an app (spawn minimum instances)
    pub async fn start_app(&self, name: &str) -> Result<(), InstanceError> {
        let app = self
            .get_app(name)
            .ok_or_else(|| InstanceError::AppNotFound(name.to_string()))?;

        let min_instances = app.config.read().min_instances;
        app.set_state(AppState::Running);

        for _ in 0..min_instances {
            let instance = app.allocate_instance();
            self.spawner.spawn(&app, instance).await?;
        }

        Ok(())
    }

    /// Stop an app (kill all instances)
    pub async fn stop_app(&self, name: &str) -> Result<(), InstanceError> {
        let app = self
            .get_app(name)
            .ok_or_else(|| InstanceError::AppNotFound(name.to_string()))?;

        app.set_state(AppState::Stopped);

        // Kill all instances
        let instances = app.get_instances();
        for instance in instances {
            instance.kill().await.map_err(InstanceError::StopError)?;
            app.remove_instance(&instance.id);
        }

        Ok(())
    }

    /// Stop every app instance owned by this server process.
    pub async fn shutdown_all(&self) {
        let apps: Vec<(String, Arc<App>)> = self
            .apps
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();

        for (name, app) in apps {
            app.set_state(AppState::Stopped);
            for instance in app.get_instances() {
                if let Err(error) = instance.kill().await {
                    tracing::warn!(
                        app = %name,
                        instance = %instance.id,
                        "Failed to stop instance during server shutdown: {error}"
                    );
                }
                app.remove_instance(&instance.id);
            }
        }
    }

    /// Get spawner for external use
    pub fn spawner(&self) -> Arc<Spawner> {
        self.spawner.clone()
    }
}

/// Errors that can occur during instance management
#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    #[error("App not found: {0}")]
    AppNotFound(String),

    #[error("Failed to spawn instance: {0}")]
    SpawnError(std::io::Error),

    #[error("Failed to stop instance: {0}")]
    StopError(std::io::Error),

    #[error("Instance startup timeout")]
    StartupTimeout,

    #[error("Instance startup timeout: {0}")]
    StartupTimeoutWithDetail(String),

    #[error("Health check failed: {0}")]
    HealthCheckFailed(String),
}

#[cfg(test)]
mod tests {
    use super::logger::noop_log_handle;
    use super::*;

    #[test]
    fn test_instance_state_transitions() {
        let instance = Instance::new("test-1".to_string(), "v1".to_string(), noop_log_handle());
        assert_eq!(instance.state(), InstanceState::Starting);

        instance.set_state(InstanceState::Ready);
        assert_eq!(instance.state(), InstanceState::Ready);

        instance.set_state(InstanceState::Healthy);
        assert_eq!(instance.state(), InstanceState::Healthy);
    }

    #[test]
    fn stop_error_display_names_stop_failure() {
        let error = InstanceError::StopError(std::io::Error::from_raw_os_error(1));

        assert!(error.to_string().starts_with("Failed to stop instance:"));
    }

    #[test]
    fn test_instance_request_tracking() {
        let instance = Instance::new("test-1".to_string(), "v1".to_string(), noop_log_handle());
        assert_eq!(instance.requests_total(), 0);

        instance.request_started();
        instance.request_finished();
        instance.request_started();
        instance.request_finished();
        instance.request_started();
        instance.request_finished();

        assert_eq!(instance.requests_total(), 3);
    }

    #[test]
    fn test_app_allocate_instances() {
        let (tx, _rx) = mpsc::channel(16);
        let config = AppConfig {
            name: "test-app".to_string(),
            version: "v1".to_string(),
            ..Default::default()
        };
        let app = App::new(config, tx, noop_log_handle());

        let i1 = app.allocate_instance();
        assert!(!i1.id.is_empty());

        let i2 = app.allocate_instance();
        assert_ne!(i1.id, i2.id);

        let i3 = app.allocate_instance();
        assert_ne!(i2.id, i3.id);
        assert!(i3.internal_token().len() >= 16);
    }

    #[test]
    fn test_allocate_instance_tracks_build_version() {
        let (tx, _rx) = mpsc::channel(16);
        let config = AppConfig {
            name: "test-app".to_string(),
            version: "v1".to_string(),
            ..Default::default()
        };
        let app = App::new(config, tx, noop_log_handle());

        let v1_instance = app.allocate_instance();
        assert_eq!(v1_instance.build_version(), "v1");

        let mut next = app.config.read().clone();
        next.version = "v2".to_string();
        app.update_config(next);

        let v2_instance = app.allocate_instance();
        assert_eq!(v2_instance.build_version(), "v2");
    }

    #[test]
    fn test_instance_internal_token_is_stable() {
        let instance = Instance::new("test-1".to_string(), "v1".to_string(), noop_log_handle());
        let token = instance.internal_token().to_string();
        assert!(!token.is_empty());
        assert_eq!(instance.internal_token(), token);
    }

    #[test]
    fn test_instance_port_round_trips() {
        let instance = Instance::new("test-1".to_string(), "v1".to_string(), noop_log_handle());
        assert_eq!(instance.port(), None);
        instance.set_port(48_123);
        assert_eq!(instance.port(), Some(48_123));
    }

    #[tokio::test]
    async fn test_app_manager_register() {
        let manager = AppManager::new(PathBuf::from("/tmp/tako-test"));

        let config = AppConfig {
            name: "my-app".to_string(),
            version: "1.0.0".to_string(),
            ..Default::default()
        };

        manager.register_app(config);

        let app = manager.get_app("my-app").unwrap();
        assert_eq!(app.name(), "my-app");
        assert_eq!(app.version(), "1.0.0");

        let apps = manager.list_apps();
        assert_eq!(apps.len(), 1);
        assert!(apps.contains(&"my-app".to_string()));
    }

    #[tokio::test]
    async fn app_manager_shutdown_all_stops_registered_instances() {
        let dir = tempfile::tempdir().unwrap();
        let manager = AppManager::new(dir.path().to_path_buf());

        let app = manager.register_app(AppConfig {
            name: "my-app".to_string(),
            version: "1.0.0".to_string(),
            ..Default::default()
        });

        let instance = app.allocate_instance();
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn test child");
        instance.set_process(child);
        assert!(instance.is_alive().await);

        manager.shutdown_all().await;

        assert!(app.get_instances().is_empty());
        assert_eq!(app.state(), AppState::Stopped);
    }

    #[test]
    fn test_get_healthy_instances() {
        let (tx, _rx) = mpsc::channel(16);
        let config = AppConfig {
            name: "test-app".to_string(),
            ..Default::default()
        };
        let app = App::new(config, tx, noop_log_handle());

        let i1 = app.allocate_instance();
        let i2 = app.allocate_instance();
        let i3 = app.allocate_instance();

        i1.set_state(InstanceState::Healthy);
        i2.set_state(InstanceState::Starting);
        i3.set_state(InstanceState::Healthy);

        let healthy = app.get_healthy_instances();
        assert_eq!(healthy.len(), 2);
    }

    #[test]
    fn app_last_error_roundtrip() {
        let (tx, _rx) = mpsc::channel(1);
        let app = App::new(AppConfig::default(), tx, noop_log_handle());
        assert_eq!(app.last_error(), None);

        app.set_last_error("boom");
        assert_eq!(app.last_error(), Some("boom".to_string()));

        app.clear_last_error();
        assert_eq!(app.last_error(), None);
    }
}

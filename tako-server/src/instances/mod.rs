//! Instance lifecycle management
//!
//! Manages app instances - spawning, health checking, and cleanup.

mod health;
pub mod logger;
mod network;
mod rolling;
mod spawner;

pub use health::*;
pub use logger::{
    AppLogHandle, LogStream, app_log_tracing_layer, log_pipe, register_app_logger,
    spawn_app_logger, unregister_app_logger,
};
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
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Child;
use tokio::sync::mpsc;

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub const INTERNAL_HOST_SUFFIX: &str = ".tako";
const LEGACY_INTERNAL_STATUS_HOST: &str = "tako.internal";
pub const INTERNAL_TOKEN_HEADER: &str = "X-Tako-Internal-Token";

pub fn internal_app_host(app_name: &str) -> String {
    let app_name = app_name.trim();
    let app_name = if app_name.is_empty() { "app" } else { app_name };
    format!("{app_name}{INTERNAL_HOST_SUFFIX}")
}

pub fn internal_app_host_for_app_id(app_id: &str) -> String {
    let app_name = tako_core::split_deployment_app_id(app_id)
        .map(|(name, _)| name)
        .unwrap_or(app_id);
    internal_app_host(app_name)
}

/// Generate a short random instance ID
fn generate_instance_id() -> String {
    nanoid::nanoid!(8)
}

fn generate_internal_token() -> String {
    nanoid::nanoid!(32)
}

fn encode_instance_state(state: InstanceState) -> u8 {
    match state {
        InstanceState::Starting => 0,
        InstanceState::Ready => 1,
        InstanceState::Healthy => 2,
        InstanceState::Unhealthy => 3,
        InstanceState::Draining => 4,
        InstanceState::Stopped => 5,
    }
}

fn decode_instance_state(encoded: u8) -> InstanceState {
    match encoded {
        0 => InstanceState::Starting,
        1 => InstanceState::Ready,
        2 => InstanceState::Healthy,
        3 => InstanceState::Unhealthy,
        4 => InstanceState::Draining,
        5 => InstanceState::Stopped,
        _ => InstanceState::Unhealthy,
    }
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
    /// Storage bindings loaded from encrypted server state.
    pub storages: HashMap<String, tako_core::StorageBinding>,
    /// Client source-IP mode for requests routed to this app.
    pub source_ip: tako_core::SourceIpMode,
    /// Public image optimizer configuration from app.json.
    pub images: tako_images::ImagesConfig,
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

    fn internal_host(&self) -> String {
        internal_app_host(&self.name)
    }

    fn apply_internal_defaults(&mut self) {
        if self.health_check_host.is_empty()
            || self.health_check_host == LEGACY_INTERNAL_STATUS_HOST
        {
            self.health_check_host = self.internal_host();
        }
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
            storages: HashMap::new(),
            source_ip: tako_core::SourceIpMode::Auto,
            images: tako_images::ImagesConfig::default(),
            min_instances: 1,
            max_instances: 4,
            health_check_path: "/status".to_string(),
            health_check_host: LEGACY_INTERNAL_STATUS_HOST.to_string(),
            startup_timeout: Duration::from_secs(30),
            idle_timeout: crate::defaults::DEFAULT_IDLE_TIMEOUT,
        }
    }
}

pub(crate) fn validate_requested_instances(
    requested_instances: u32,
    max_instances: u32,
) -> Result<(), String> {
    let limit = effective_instance_limit(max_instances);
    if requested_instances > limit {
        return Err(format!(
            "Requested {requested_instances} instances, but this server allows at most {limit}. Use fewer instances or spread traffic across more servers."
        ));
    }
    Ok(())
}

pub(crate) fn default_max_instances_for_host() -> u32 {
    host_instance_limit_for_parallelism(host_parallelism())
}

pub(crate) fn effective_instance_limit(max_instances: u32) -> u32 {
    max_instances.min(default_max_instances_for_host())
}

pub(crate) fn clamp_instances_to_limit(config: &mut AppConfig) -> Option<(u32, u32)> {
    let limit = effective_instance_limit(config.max_instances);
    if config.min_instances <= limit {
        return None;
    }

    let requested = config.min_instances;
    config.min_instances = limit;
    Some((requested, limit))
}

fn host_parallelism() -> u32 {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .try_into()
        .unwrap_or(u32::MAX)
}

fn host_instance_limit_for_parallelism(parallelism: u32) -> u32 {
    parallelism.saturating_mul(2).max(1)
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
    state: AtomicU8,
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
    #[cfg(test)]
    pub fn new(id: String, build_version: String, log_handle: AppLogHandle) -> Self {
        Self::new_inner(id, build_version, log_handle)
    }

    fn new_inner(id: String, build_version: String, log_handle: AppLogHandle) -> Self {
        Self {
            id,
            build_version,
            internal_token: generate_internal_token(),
            upstream: RwLock::new(None),
            process: RwLock::new(None),
            pid: AtomicU32::new(0),
            state: AtomicU8::new(encode_instance_state(InstanceState::Starting)),
            started_at: RwLock::new(None),
            requests_total: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            last_request_ms: AtomicU64::new(now_unix_millis()),
            last_heartbeat_ms: AtomicU64::new(now_unix_millis()),
            log_handle,
        }
    }

    pub fn state(&self) -> InstanceState {
        decode_instance_state(self.state.load(Ordering::Acquire))
    }

    /// Set raw lifecycle state. Use `App::set_instance_state` for transitions
    /// that can add or remove the instance from request routing.
    pub fn set_state(&self, state: InstanceState) -> InstanceState {
        let encoded = encode_instance_state(state);
        let previous = self.state.swap(encoded, Ordering::AcqRel);
        decode_instance_state(previous)
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
    /// Instances currently eligible for request routing.
    healthy_instances: RwLock<Vec<Arc<Instance>>>,
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
        mut config: AppConfig,
        instance_tx: mpsc::Sender<InstanceEvent>,
        log_handle: AppLogHandle,
    ) -> Self {
        config.apply_internal_defaults();
        Self {
            config: RwLock::new(config),
            instances: DashMap::new(),
            healthy_instances: RwLock::new(Vec::new()),
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

    #[cfg(test)]
    pub(crate) fn healthy_instances(&self) -> Vec<Arc<Instance>> {
        self.healthy_instances.read().clone()
    }

    pub(crate) fn set_instance_state(
        &self,
        instance: &Arc<Instance>,
        state: InstanceState,
    ) -> InstanceState {
        let previous = instance.set_state(state);
        if previous == state {
            return previous;
        }

        match (previous, state) {
            (InstanceState::Healthy, InstanceState::Healthy) => {}
            (InstanceState::Healthy, _) => self.remove_healthy_instance(&instance.id),
            (_, InstanceState::Healthy) => self.add_healthy_instance(instance),
            _ => {}
        }

        previous
    }

    pub(crate) fn healthy_instance_for_request(
        &self,
        request_index: usize,
    ) -> Option<Arc<Instance>> {
        let instances = self.healthy_instances.read();
        if instances.is_empty() {
            return None;
        }

        Some(instances[request_index % instances.len()].clone())
    }

    fn add_healthy_instance(&self, instance: &Arc<Instance>) {
        let mut healthy_instances = self.healthy_instances.write();
        if healthy_instances
            .iter()
            .any(|healthy| healthy.id == instance.id)
        {
            return;
        }

        healthy_instances.push(instance.clone());
    }

    fn remove_healthy_instance(&self, instance_id: &str) {
        self.healthy_instances
            .write()
            .retain(|instance| instance.id != instance_id);
    }

    /// Get all healthy instances
    #[cfg(test)]
    pub(crate) fn get_healthy_instances(&self) -> Vec<Arc<Instance>> {
        self.healthy_instances()
    }

    #[cfg(test)]
    pub(crate) fn healthy_instance_count(&self) -> usize {
        self.healthy_instances.read().len()
    }

    #[cfg(test)]
    pub(crate) fn healthy_instance_at(&self, healthy_index: usize) -> Option<Arc<Instance>> {
        self.healthy_instances.read().get(healthy_index).cloned()
    }

    pub(crate) fn has_starting_instance(&self) -> bool {
        self.instances.iter().any(|entry| {
            matches!(
                entry.value().state(),
                InstanceState::Starting | InstanceState::Ready
            )
        })
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
        let instance = Arc::new(Instance::new_inner(
            id.clone(),
            config.version.clone(),
            self.log_handle.clone(),
        ));
        self.instances.insert(id, instance.clone());
        instance
    }

    /// Remove an instance
    pub fn remove_instance(&self, id: &str) -> Option<Arc<Instance>> {
        let removed = self.instances.remove(id).map(|(_, v)| v);
        if removed.is_some() {
            self.remove_healthy_instance(id);
        }
        removed
    }

    /// Update configuration (for reloads/deploys)
    pub fn update_config(&self, mut config: AppConfig) {
        config.apply_internal_defaults();
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
        register_app_logger(&name, log_handle.clone());
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
        let removed = self.apps.remove(name).map(|(_, v)| v);
        if removed.is_some() {
            unregister_app_logger(name);
        }
        removed
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
            app.set_instance_state(&instance, InstanceState::Draining);
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
                app.set_instance_state(&instance, InstanceState::Draining);
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
mod tests;

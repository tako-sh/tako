use crate::socket::{InstanceState, InstanceStatus};
use parking_lot::RwLock;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Child;

use super::{AppLogHandle, LogStream, PreparedInstanceNetwork, log_pipe};

pub(super) fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Generate a short random instance ID
pub(super) fn generate_instance_id() -> String {
    nanoid::nanoid!(8)
}

pub(super) fn generate_internal_token() -> String {
    nanoid::nanoid!(32)
}

pub(super) fn encode_instance_state(state: InstanceState) -> u8 {
    match state {
        InstanceState::Starting => 0,
        InstanceState::Ready => 1,
        InstanceState::Healthy => 2,
        InstanceState::Unhealthy => 3,
        InstanceState::Draining => 4,
        InstanceState::Stopped => 5,
    }
}

pub(super) fn decode_instance_state(encoded: u8) -> InstanceState {
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
    pub(super) last_request_ms: AtomicU64,
    /// Last health-check heartbeat time as millis since UNIX_EPOCH
    last_heartbeat_ms: AtomicU64,

    /// Log handle for forwarding stdout/stderr to the app log writer.
    log_handle: AppLogHandle,
}

#[derive(Clone)]
pub(crate) struct HealthyInstance {
    pub(crate) instance: Arc<Instance>,
    pub(crate) endpoint: Option<SocketAddr>,
}

impl Instance {
    #[cfg(test)]
    pub fn new(id: String, build_version: String, log_handle: AppLogHandle) -> Self {
        Self::new_inner(id, build_version, log_handle)
    }

    pub(super) fn new_inner(id: String, build_version: String, log_handle: AppLogHandle) -> Self {
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
        if self.in_flight.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.last_request_ms
                .store(now_unix_millis(), Ordering::Relaxed);
        }
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

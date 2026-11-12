use crate::socket::{InstanceState, InstanceStatus};
use parking_lot::RwLock;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
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

// One atomic holds both in-flight counts so a drain reads a consistent pair:
// the total in the high half, the long-lived (protocol-upgraded) subset in
// the low half. Concurrent requests are bounded far below 2^32.
const IN_FLIGHT_ONE: u64 = 1 << 32;
const LONG_LIVED_ONE: u64 = 1;
const LONG_LIVED_MASK: u64 = IN_FLIGHT_ONE - 1;

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
    sdk_health: AtomicBool,
    health_probe: RwLock<Option<(String, String)>>,
    /// Upstream endpoint and runtime cleanup metadata.
    upstream: RwLock<Option<PreparedInstanceNetwork>>,
    /// Process handle
    process: RwLock<Option<Child>>,
    container: RwLock<Option<(crate::container_runtime::ContainerEngine, String)>>,
    compose: RwLock<Option<ComposeInstance>>,
    stop_lock: tokio::sync::Mutex<()>,
    terminate_at: RwLock<Option<tokio::time::Instant>>,
    /// Process ID
    pid: AtomicU32,
    /// Current state
    state: AtomicU8,
    /// Set once the instance has been healthy; tells a crash from a failed start.
    was_healthy: AtomicBool,
    /// Exit status and wall-clock time of the process, where its exit was observed.
    observed_exit: RwLock<Option<(std::process::ExitStatus, u64)>>,
    /// When the instance started
    started_at: RwLock<Option<Instant>>,
    /// Total requests handled
    requests_total: AtomicU64,

    /// In-flight requests (best-effort; used to avoid killing while serving),
    /// packed with the long-lived subset; see `IN_FLIGHT_ONE`.
    in_flight: AtomicU64,
    /// Last request completion time as millis since UNIX_EPOCH (for idle timeout)
    pub(super) last_request_ms: AtomicU64,
    /// Last health-check heartbeat time as millis since UNIX_EPOCH
    last_heartbeat_ms: AtomicU64,

    /// Log handle for forwarding stdout/stderr to the app log writer.
    log_handle: AppLogHandle,
}

#[derive(Clone)]
struct ComposeInstance {
    runtime: crate::compose_runtime::ComposeRuntime,
    container_id: String,
    owned: bool,
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
            sdk_health: AtomicBool::new(true),
            health_probe: RwLock::new(None),
            upstream: RwLock::new(None),
            process: RwLock::new(None),
            container: RwLock::new(None),
            compose: RwLock::new(None),
            stop_lock: tokio::sync::Mutex::new(()),
            terminate_at: RwLock::new(None),
            pid: AtomicU32::new(0),
            state: AtomicU8::new(encode_instance_state(InstanceState::Starting)),
            was_healthy: AtomicBool::new(false),
            observed_exit: RwLock::new(None),
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
        if state == InstanceState::Healthy {
            self.was_healthy.store(true, Ordering::Release);
        }
        let encoded = encode_instance_state(state);
        let previous = self.state.swap(encoded, Ordering::AcqRel);
        decode_instance_state(previous)
    }

    /// Describe the loss of this instance for status reporting. Call before
    /// `kill`, which detaches Compose containers.
    pub(crate) fn exit_report(&self) -> tako_core::InstanceExit {
        let observed = *self.observed_exit.read();
        // A Compose instance's process is its log follower, whose status says
        // nothing about the container; `AppStatus.services` reports that.
        let status = observed
            .map(|(status, _)| status)
            .filter(|_| self.compose.read().is_none());
        let reason = if !self.was_healthy.load(Ordering::Acquire) {
            tako_core::InstanceExitReason::FailedToStart
        } else if observed.is_some() {
            tako_core::InstanceExitReason::Crashed
        } else {
            tako_core::InstanceExitReason::Unhealthy
        };
        #[cfg(unix)]
        let signal =
            status.and_then(|status| std::os::unix::process::ExitStatusExt::signal(&status));
        #[cfg(not(unix))]
        let signal = None;
        tako_core::InstanceExit {
            reason,
            exit_code: status.and_then(|status| status.code()),
            signal,
            at_unix_secs: (observed.map_or_else(now_unix_millis, |(_, at)| at) / 1000) as i64,
        }
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

    pub(crate) fn set_container(
        &self,
        engine: crate::container_runtime::ContainerEngine,
        name: String,
    ) {
        *self.container.write() = Some((engine, name));
    }

    pub(crate) fn set_sdk_health(&self, enabled: bool) {
        self.sdk_health.store(enabled, Ordering::Release);
    }

    pub(crate) fn set_health_probe(&self, host: String, path: String) {
        *self.health_probe.write() = Some((host, path));
    }

    pub(crate) fn health_probe(&self) -> Option<(String, String)> {
        self.health_probe.read().clone()
    }

    pub(crate) fn set_compose(
        &self,
        runtime: crate::compose_runtime::ComposeRuntime,
        container_id: String,
    ) {
        // The pending rollout owns cleanup until its durable commit.
        *self.compose.write() = Some(ComposeInstance {
            runtime,
            container_id,
            owned: false,
        });
    }

    pub(crate) fn compose_container_id(&self) -> Option<String> {
        self.compose.read().as_ref().map(|c| c.container_id.clone())
    }

    pub(crate) fn own_compose_container(&self, owned: bool) {
        if let Some(compose) = self.compose.write().as_mut() {
            compose.owned = owned;
        }
    }

    pub(crate) fn health_token(&self) -> Option<&str> {
        self.sdk_health
            .load(Ordering::Acquire)
            .then_some(self.internal_token())
    }

    pub fn request_started(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.in_flight.fetch_add(IN_FLIGHT_ONE, Ordering::Relaxed);
    }

    /// Reclassify one started request as an upgraded connection (the app
    /// answered `101 Switching Protocols`, for example a WebSocket).
    /// It stays in `in_flight` but no longer holds an instance drain.
    pub fn request_became_long_lived(&self) {
        self.in_flight.fetch_add(LONG_LIVED_ONE, Ordering::Relaxed);
    }

    pub fn request_finished(&self) {
        self.finish_request(IN_FLIGHT_ONE);
    }

    pub fn long_lived_request_finished(&self) {
        self.finish_request(IN_FLIGHT_ONE + LONG_LIVED_ONE);
    }

    fn finish_request(&self, delta: u64) {
        if self.in_flight.fetch_sub(delta, Ordering::Relaxed) >> 32 == 1 {
            self.last_request_ms
                .store(now_unix_millis(), Ordering::Relaxed);
        }
    }

    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed) >> 32
    }

    /// In-flight requests an instance drain waits for: all but long-lived ones.
    pub fn short_in_flight(&self) -> u64 {
        let packed = self.in_flight.load(Ordering::Relaxed);
        (packed >> 32).saturating_sub(packed & LONG_LIVED_MASK)
    }

    pub fn long_lived_in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed) & LONG_LIVED_MASK
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

    /// Keep the first exit this server observed without having stopped the
    /// process itself.
    fn note_exit(&self, status: std::process::ExitStatus) {
        self.observed_exit
            .write()
            .get_or_insert((status, now_unix_millis()));
    }

    /// Check if process is still running
    pub async fn is_alive(&self) -> bool {
        let mut process = self.process.write();
        if let Some(ref mut child) = *process {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.note_exit(status);
                    false
                }
                Ok(None) => true, // Still running
                Err(_) => false,  // Error checking
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
        self.kill_with_grace(Duration::from_secs(15)).await
    }

    pub(super) async fn kill_with_grace(&self, grace: Duration) -> Result<(), std::io::Error> {
        let _guard = self.stop_lock.lock().await;
        let compose = self.compose.read().clone();
        if let Some(compose) = compose {
            if compose.owned {
                compose
                    .runtime
                    .stop_container(&compose.container_id)
                    .await
                    .map_err(std::io::Error::other)?;
            }
            *self.compose.write() = None;
        }
        let container = self.container.read().clone();
        if let Some((engine, name)) = container {
            crate::container_runtime::remove_container(engine, &name).await?;
            *self.container.write() = None;
        }
        let deadline = {
            let mut process = self.process.write();
            let mut terminate_at = self.terminate_at.write();
            if terminate_at.is_none() {
                // A process that is already gone exited by itself; keep that status.
                if let Some(child) = process.as_mut()
                    && child
                        .try_wait()?
                        .map(|status| self.note_exit(status))
                        .is_none()
                {
                    #[cfg(unix)]
                    if let Some(pid) = child.id()
                        && unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0
                    {
                        let error = std::io::Error::last_os_error();
                        if error.raw_os_error() != Some(libc::ESRCH) {
                            return Err(error);
                        }
                    }
                    #[cfg(not(unix))]
                    child.start_kill()?;
                }
                *terminate_at = Some(tokio::time::Instant::now() + grace);
            }
            terminate_at.unwrap()
        };
        loop {
            let stopped = {
                let mut process = self.process.write();
                if let Some(child) = process.as_mut() {
                    if child.try_wait()?.is_some() {
                        process.take();
                        true
                    } else {
                        if tokio::time::Instant::now() >= deadline {
                            child.start_kill()?;
                        }
                        false
                    }
                } else {
                    true
                }
            };
            if stopped {
                break;
            }
            if tokio::time::Instant::now() >= deadline + Duration::from_secs(5) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "process did not exit after SIGKILL",
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
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

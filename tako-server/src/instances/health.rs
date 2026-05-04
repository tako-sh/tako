//! Health checker - monitors instance health via HTTP probing
//!
//! Performs active HTTP health checks to internal host `tako` at `/status` on each
//! instance.
//! This replaces passive heartbeat-only detection with active probing.

use super::{App, INTERNAL_TOKEN_HEADER, Instance, InstanceState};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Health check configuration
#[derive(Debug, Clone)]
pub struct HealthConfig {
    /// Steady-state interval between health checks (after first Healthy probe).
    pub check_interval: Duration,
    /// Faster interval used while any instance is still in startup
    /// (Starting/Ready, not yet Healthy). Drops cold-start probe slack
    /// from up to 1s down to ~100ms.
    pub startup_check_interval: Duration,
    /// Number of consecutive failures before marking unhealthy
    pub unhealthy_threshold: u32,
    /// Number of consecutive failures before marking dead
    pub dead_threshold: u32,
    /// Timeout for individual health check requests
    pub probe_timeout: Duration,
    /// Maximum concurrent probe tasks per app per cycle
    pub max_probe_concurrency: usize,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            check_interval: crate::defaults::HEALTH_CHECK_INTERVAL,
            startup_check_interval: crate::defaults::HEALTH_STARTUP_CHECK_INTERVAL,
            unhealthy_threshold: 1, // 1 failure = unhealthy
            dead_threshold: 1,      // 1 failure = dead/restart
            probe_timeout: crate::defaults::HEALTH_PROBE_TIMEOUT,
            max_probe_concurrency: 16,
        }
    }
}

/// Health check events
#[derive(Debug, Clone)]
pub enum HealthEvent {
    /// Instance became healthy
    Healthy { app: String, instance_id: String },
    /// Instance became unhealthy
    Unhealthy { app: String, instance_id: String },
    /// Instance is dead (no heartbeat for too long)
    Dead { app: String, instance_id: String },
    /// Instance recovered from unhealthy
    Recovered { app: String, instance_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HealthProbeFailure {
    reason: &'static str,
    detail: String,
}

impl HealthProbeFailure {
    fn new(reason: &'static str, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for HealthProbeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.detail.is_empty() {
            write!(f, "{}", self.reason)
        } else {
            write!(f, "{}: {}", self.reason, self.detail)
        }
    }
}

/// Tracks consecutive health check failures per instance
use dashmap::DashMap;

/// Health checker for monitoring instance health via HTTP probing
#[derive(Clone)]
pub struct HealthChecker {
    config: HealthConfig,
    event_tx: mpsc::Sender<HealthEvent>,
    /// Consecutive failure counts per instance (app_name:instance_id -> count)
    failure_counts: Arc<DashMap<String, u32>>,
}

impl HealthChecker {
    pub fn new(config: HealthConfig, event_tx: mpsc::Sender<HealthEvent>) -> Self {
        Self {
            config,
            event_tx,
            failure_counts: Arc::new(DashMap::new()),
        }
    }

    fn effective_probe_concurrency(value: usize) -> usize {
        value.max(1)
    }

    /// Start health check loop for an app.
    ///
    /// Uses `startup_check_interval` while any instance is still in startup
    /// (Starting or Ready); falls back to `check_interval` once all instances
    /// are Healthy. This collapses the worst-case probe slack on cold start
    /// from `check_interval` (typically 1s) to `startup_check_interval`
    /// (typically 100ms) without paying for high-frequency probes at steady
    /// state.
    pub async fn monitor_app(&self, app: Arc<App>) {
        let concurrency = Self::effective_probe_concurrency(self.config.max_probe_concurrency);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));

        loop {
            let interval = if app_has_starting_instance(&app) {
                self.config.startup_check_interval
            } else {
                self.config.check_interval
            };
            tokio::time::sleep(interval).await;

            let instances = app.get_instances();
            let mut checks = tokio::task::JoinSet::new();

            for instance in instances {
                let permit = match semaphore.clone().acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => break,
                };

                let checker = self.clone();
                let app = app.clone();
                checks.spawn(async move {
                    checker.check_instance(&app, &instance).await;
                    drop(permit);
                });
            }

            while checks.join_next().await.is_some() {}
        }
    }

    /// Check health of a single instance via HTTP probe
    async fn check_instance(&self, app: &App, instance: &Instance) {
        let current_state = instance.state();

        // Skip instances that are starting, draining, or already stopped
        if matches!(
            current_state,
            InstanceState::Starting | InstanceState::Draining | InstanceState::Stopped
        ) {
            return;
        }

        let instance_key = format!("{}:{}", app.name(), instance.id);

        // Fast path: detect process exit immediately via try_wait() instead
        // of waiting for the HTTP probe to time out.
        if !instance.is_alive().await {
            self.failure_counts.remove(&instance_key);
            instance.set_state(InstanceState::Stopped);
            tracing::error!(
                app = %app.name(),
                instance = %instance.id,
                "Instance process exited"
            );
            let _ = self
                .event_tx
                .send(HealthEvent::Dead {
                    app: app.name(),
                    instance_id: instance.id.clone(),
                })
                .await;
            return;
        }

        // Build health check target using app's configured path and internal host header
        let (health_host, health_path) = {
            let config = app.config.read();
            (
                config.health_check_host.clone(),
                config.health_check_path.clone(),
            )
        };

        // Perform HTTP probe
        let probe_result = probe_instance_health(
            instance,
            &health_host,
            &health_path,
            self.config.probe_timeout,
        )
        .await;

        if probe_result.is_ok() {
            // Reset failure count and record heartbeat
            self.failure_counts.remove(&instance_key);
            instance.record_heartbeat();

            // Mark healthy on first successful probe.
            if current_state != InstanceState::Healthy {
                instance.set_state(InstanceState::Healthy);

                let event = if current_state == InstanceState::Unhealthy {
                    HealthEvent::Recovered {
                        app: app.name(),
                        instance_id: instance.id.clone(),
                    }
                } else {
                    HealthEvent::Healthy {
                        app: app.name(),
                        instance_id: instance.id.clone(),
                    }
                };
                let _ = self.event_tx.send(event).await;
            }
        } else {
            let failure = probe_result.expect_err("probe_result checked as error");
            // Increment failure count
            let mut failures = self.failure_counts.entry(instance_key.clone()).or_insert(0);
            *failures += 1;
            let failure_count = *failures;

            tracing::debug!(
                app = %app.name(),
                instance = %instance.id,
                failures = failure_count,
                reason = failure.reason,
                detail = %failure.detail,
                "Health check failed"
            );

            // Determine new state based on failure count
            let new_state = if failure_count >= self.config.dead_threshold {
                InstanceState::Stopped
            } else if failure_count >= self.config.unhealthy_threshold {
                InstanceState::Unhealthy
            } else {
                current_state
            };

            if new_state != current_state {
                instance.set_state(new_state);

                let event = match new_state {
                    InstanceState::Unhealthy => {
                        tracing::warn!(
                            app = %app.name(),
                            instance = %instance.id,
                            failures = failure_count,
                            reason = failure.reason,
                            detail = %failure.detail,
                            "Instance marked unhealthy"
                        );
                        Some(HealthEvent::Unhealthy {
                            app: app.name(),
                            instance_id: instance.id.clone(),
                        })
                    }
                    InstanceState::Stopped => {
                        tracing::error!(
                            app = %app.name(),
                            instance = %instance.id,
                            failures = failure_count,
                            reason = failure.reason,
                            detail = %failure.detail,
                            "Instance marked dead after {} consecutive failures",
                            failure_count
                        );
                        Some(HealthEvent::Dead {
                            app: app.name(),
                            instance_id: instance.id.clone(),
                        })
                    }
                    _ => None,
                };

                if let Some(event) = event {
                    let _ = self.event_tx.send(event).await;
                }
            }
        }
    }

    /// Get current failure count for an instance
    pub fn get_failure_count(&self, app_name: &str, instance_id: &str) -> u32 {
        let key = format!("{}:{}", app_name, instance_id);
        self.failure_counts.get(&key).map(|v| *v).unwrap_or(0)
    }

    /// Clear failure count for an instance (e.g., after restart)
    pub fn clear_failure_count(&self, app_name: &str, instance_id: &str) {
        let key = format!("{}:{}", app_name, instance_id);
        self.failure_counts.remove(&key);
    }
}

/// True if any of the app's instances is in a startup state — i.e. it has
/// been spawned but not yet observed Healthy by a probe. Drives the choice
/// between fast and steady-state probe intervals.
fn app_has_starting_instance(app: &App) -> bool {
    app.get_instances()
        .iter()
        .any(|i| matches!(i.state(), InstanceState::Starting | InstanceState::Ready))
}

async fn probe_instance_health(
    instance: &Instance,
    health_host: &str,
    health_path: &str,
    probe_timeout: Duration,
) -> Result<(), HealthProbeFailure> {
    let Some(endpoint) = instance.endpoint() else {
        return Err(HealthProbeFailure::new(
            "missing_endpoint",
            "instance has no private upstream endpoint",
        ));
    };
    probe_endpoint_tcp(
        endpoint,
        health_host,
        health_path,
        instance.internal_token(),
        probe_timeout,
    )
    .await
}

async fn probe_endpoint_tcp(
    endpoint: std::net::SocketAddr,
    health_host: &str,
    health_path: &str,
    internal_token: &str,
    probe_timeout: Duration,
) -> Result<(), HealthProbeFailure> {
    use tokio::io::AsyncWriteExt;

    let mut socket = match timeout(probe_timeout, tokio::net::TcpStream::connect(endpoint)).await {
        Ok(Ok(socket)) => socket,
        Ok(Err(error)) => {
            return Err(HealthProbeFailure::new("connect_failed", error.to_string()));
        }
        Err(_) => {
            return Err(HealthProbeFailure::new(
                "connect_timeout",
                format!("timed out after {:?}", probe_timeout),
            ));
        }
    };
    let request = format!(
        "GET {health_path} HTTP/1.1\r\nHost: {health_host}\r\n{INTERNAL_TOKEN_HEADER}: {internal_token}\r\nConnection: close\r\n\r\n"
    );
    match timeout(probe_timeout, socket.write_all(request.as_bytes())).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            return Err(HealthProbeFailure::new("write_failed", error.to_string()));
        }
        Err(_) => {
            return Err(HealthProbeFailure::new(
                "write_timeout",
                format!("timed out after {:?}", probe_timeout),
            ));
        }
    }

    let response = read_http_response_headers(&mut socket, probe_timeout).await?;
    http_response_is_internal_success(&response, internal_token)
}

async fn read_http_response_headers(
    socket: &mut tokio::net::TcpStream,
    io_timeout: Duration,
) -> Result<String, HealthProbeFailure> {
    use tokio::io::AsyncReadExt;

    let mut response = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];

    loop {
        let bytes_read = match timeout(io_timeout, socket.read(&mut chunk)).await {
            Ok(Ok(bytes_read)) => bytes_read,
            Ok(Err(error)) => {
                return Err(HealthProbeFailure::new("read_failed", error.to_string()));
            }
            Err(_) => {
                return Err(HealthProbeFailure::new(
                    "read_timeout",
                    format!("timed out after {:?}", io_timeout),
                ));
            }
        };

        if bytes_read == 0 {
            break;
        }

        response.extend_from_slice(&chunk[..bytes_read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    if response.is_empty() {
        return Err(HealthProbeFailure::new(
            "empty_response",
            "connection closed before response headers",
        ));
    }

    Ok(String::from_utf8_lossy(&response).into_owned())
}

fn http_status_is_success(status_line: &str) -> bool {
    let mut parts = status_line.split_whitespace();
    let Some(http_version) = parts.next() else {
        return false;
    };
    if !http_version.starts_with("HTTP/") {
        return false;
    }
    parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .map(|code| (200..300).contains(&code))
        .unwrap_or(false)
}

fn http_response_is_internal_success(
    response: &str,
    expected_token: &str,
) -> Result<(), HealthProbeFailure> {
    let mut lines = response.lines();
    let status_line = lines.next().unwrap_or_default();
    if !http_status_is_success(status_line) {
        return Err(HealthProbeFailure::new(
            "bad_status",
            status_line.to_string(),
        ));
    }

    let has_token = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.eq_ignore_ascii_case(INTERNAL_TOKEN_HEADER) && value.trim() == expected_token
        });
    if !has_token {
        return Err(HealthProbeFailure::new(
            "missing_internal_token",
            "status response did not echo the expected internal token",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::AppConfig;
    use crate::instances::logger::noop_log_handle;
    use tokio::sync::mpsc;

    fn create_test_app() -> Arc<App> {
        let (tx, _rx) = mpsc::channel(16);
        let config = AppConfig {
            name: "test-app".to_string(),
            ..Default::default()
        };
        Arc::new(App::new(config, tx, noop_log_handle()))
    }

    #[test]
    fn test_health_config_defaults() {
        let config = HealthConfig::default();
        assert_eq!(
            config.check_interval,
            crate::defaults::HEALTH_CHECK_INTERVAL
        );
        assert_eq!(
            config.startup_check_interval,
            crate::defaults::HEALTH_STARTUP_CHECK_INTERVAL
        );
        assert!(
            config.startup_check_interval < config.check_interval,
            "startup probe must be faster than steady-state"
        );
        assert_eq!(config.unhealthy_threshold, 1);
        assert_eq!(config.dead_threshold, 1);
        assert_eq!(config.probe_timeout, crate::defaults::HEALTH_PROBE_TIMEOUT);
        assert_eq!(config.max_probe_concurrency, 16);
    }

    #[test]
    fn test_app_has_starting_instance_detects_startup_states() {
        let app = create_test_app();
        let instance = app.allocate_instance();

        instance.set_state(InstanceState::Starting);
        assert!(app_has_starting_instance(&app));

        instance.set_state(InstanceState::Ready);
        assert!(app_has_starting_instance(&app));

        instance.set_state(InstanceState::Healthy);
        assert!(!app_has_starting_instance(&app));

        instance.set_state(InstanceState::Unhealthy);
        assert!(!app_has_starting_instance(&app));
    }

    #[test]
    fn test_effective_probe_concurrency_never_zero() {
        assert_eq!(HealthChecker::effective_probe_concurrency(0), 1);
        assert_eq!(HealthChecker::effective_probe_concurrency(7), 7);
    }

    #[tokio::test]
    async fn test_health_checker_creation() {
        let (tx, _rx) = mpsc::channel(16);
        let config = HealthConfig::default();
        let checker = HealthChecker::new(config, tx);

        // Verify failure counts start empty
        assert_eq!(checker.get_failure_count("test-app", "1"), 0);
    }

    #[tokio::test]
    async fn test_health_checker_failure_tracking() {
        let (tx, _rx) = mpsc::channel(16);
        let config = HealthConfig::default();
        let checker = HealthChecker::new(config, tx);

        // Simulate failure count increment (this would normally happen in check_instance)
        let key = "test-app:1".to_string();
        checker.failure_counts.insert(key.clone(), 3);

        assert_eq!(checker.get_failure_count("test-app", "1"), 3);

        // Clear and verify
        checker.clear_failure_count("test-app", "1");
        assert_eq!(checker.get_failure_count("test-app", "1"), 0);
    }

    #[tokio::test]
    async fn test_health_checker_skips_non_running_instances() {
        let (tx, mut rx) = mpsc::channel(16);
        let config = HealthConfig::default();
        let checker = HealthChecker::new(config, tx);

        let app = create_test_app();
        let instance = app.allocate_instance();

        // Instance in Starting state should be skipped
        instance.set_state(InstanceState::Starting);
        checker.check_instance(&app, &instance).await;

        // No events should be emitted
        assert!(rx.try_recv().is_err());

        // Instance in Draining state should be skipped
        instance.set_state(InstanceState::Draining);
        checker.check_instance(&app, &instance).await;
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_health_event_types() {
        let healthy = HealthEvent::Healthy {
            app: "test".to_string(),
            instance_id: "abc123".to_string(),
        };
        let unhealthy = HealthEvent::Unhealthy {
            app: "test".to_string(),
            instance_id: "abc123".to_string(),
        };
        let dead = HealthEvent::Dead {
            app: "test".to_string(),
            instance_id: "abc123".to_string(),
        };
        let recovered = HealthEvent::Recovered {
            app: "test".to_string(),
            instance_id: "abc123".to_string(),
        };

        // Just verify they can be created and formatted
        assert!(format!("{:?}", healthy).contains("Healthy"));
        assert!(format!("{:?}", unhealthy).contains("Unhealthy"));
        assert!(format!("{:?}", dead).contains("Dead"));
        assert!(format!("{:?}", recovered).contains("Recovered"));
    }

    #[tokio::test]
    async fn test_probe_uses_tcp_when_port_is_configured() {
        let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
            return;
        };
        let port = listener.local_addr().expect("listener addr").port();

        let (tx, _rx) = mpsc::channel(16);
        let config = AppConfig {
            name: "test-app".to_string(),
            min_instances: 1,
            ..Default::default()
        };
        let app = App::new(config, tx, noop_log_handle());
        let instance = app.allocate_instance();
        instance.set_port(port);
        let token = instance.internal_token().to_string();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request_buf = [0_u8; 2048];
            let n = tokio::io::AsyncReadExt::read(&mut socket, &mut request_buf)
                .await
                .expect("read request");
            let request = String::from_utf8_lossy(&request_buf[..n]);
            let is_internal_status = request.starts_with("GET /status ")
                && request
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("host: tako"));
            let has_token = request.lines().any(|line| {
                line.eq_ignore_ascii_case(&format!("{INTERNAL_TOKEN_HEADER}: {token}"))
            });

            let response = if is_internal_status && has_token {
                format!(
                    "HTTP/1.1 200 OK\r\n{INTERNAL_TOKEN_HEADER}: {token}\r\nContent-Length: 2\r\n\r\nok"
                )
            } else {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found".to_string()
            };

            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
        });

        let healthy =
            probe_instance_health(&instance, "tako", "/status", Duration::from_millis(200)).await;
        assert!(healthy.is_ok());
    }

    #[tokio::test]
    async fn test_probe_reads_split_response_headers() {
        let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
            return;
        };
        let port = listener.local_addr().expect("listener addr").port();

        let (tx, _rx) = mpsc::channel(16);
        let config = AppConfig {
            name: "test-app".to_string(),
            min_instances: 1,
            ..Default::default()
        };
        let app = App::new(config, tx, noop_log_handle());
        let instance = app.allocate_instance();
        instance.set_port(port);
        let token = instance.internal_token().to_string();

        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request_buf = [0_u8; 2048];
            let n = tokio::io::AsyncReadExt::read(&mut socket, &mut request_buf)
                .await
                .expect("read request");
            let request = String::from_utf8_lossy(&request_buf[..n]);
            let is_internal_status = request.starts_with("GET /status ")
                && request
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("host: tako"));
            let has_token = request.lines().any(|line| {
                line.eq_ignore_ascii_case(&format!("{INTERNAL_TOKEN_HEADER}: {token}"))
            });

            if is_internal_status && has_token {
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nX-Tako-Internal-Token: ")
                    .await
                    .expect("write response prefix");
                tokio::time::sleep(Duration::from_millis(10)).await;
                socket
                    .write_all(format!("{token}\r\nContent-Length: 2\r\n\r\nok").as_bytes())
                    .await
                    .expect("write response suffix");
            } else {
                socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found")
                    .await
                    .expect("write not found");
            }
        });

        let healthy =
            probe_instance_health(&instance, "tako", "/status", Duration::from_millis(200)).await;
        assert!(healthy.is_ok());
    }

    #[tokio::test]
    async fn test_probe_reports_connect_failure_reason() {
        let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
            return;
        };
        let port = listener.local_addr().expect("listener addr").port();
        drop(listener);

        let (tx, _rx) = mpsc::channel(16);
        let app = App::new(AppConfig::default(), tx, noop_log_handle());
        let instance = app.allocate_instance();
        instance.set_port(port);

        let failure =
            probe_instance_health(&instance, "tako", "/status", Duration::from_millis(200))
                .await
                .expect_err("closed port should fail");

        assert_eq!(failure.reason, "connect_failed");
        assert!(failure.detail.contains("Connection refused"));
    }

    #[tokio::test]
    async fn test_probe_reports_missing_internal_token_reason() {
        let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
            return;
        };
        let port = listener.local_addr().expect("listener addr").port();

        let (tx, _rx) = mpsc::channel(16);
        let app = App::new(AppConfig::default(), tx, noop_log_handle());
        let instance = app.allocate_instance();
        instance.set_port(port);

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request_buf = [0_u8; 2048];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut request_buf).await;
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
        });

        let failure =
            probe_instance_health(&instance, "tako", "/status", Duration::from_millis(200))
                .await
                .expect_err("response without echoed token should fail");

        assert_eq!(failure.reason, "missing_internal_token");
    }

    #[tokio::test]
    async fn test_check_instance_detects_process_exit() {
        let (tx, mut rx) = mpsc::channel(16);
        let config = HealthConfig::default();
        let checker = HealthChecker::new(config, tx);

        let (app_tx, _app_rx) = mpsc::channel(16);
        let app_config = AppConfig {
            name: "test-app".to_string(),
            ..Default::default()
        };
        let app = Arc::new(App::new(app_config, app_tx, noop_log_handle()));
        let instance = app.allocate_instance();

        // Spawn a process that exits immediately.
        let child = tokio::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        instance.set_process(child);
        instance.set_state(InstanceState::Healthy);

        // Wait for the process to actually exit.
        tokio::time::sleep(Duration::from_millis(100)).await;

        checker.check_instance(&app, &instance).await;

        // Should emit Dead event (process exited).
        let event = rx.try_recv().expect("should emit event");
        assert!(matches!(event, HealthEvent::Dead { .. }));
        assert_eq!(instance.state(), InstanceState::Stopped);
    }

    #[tokio::test]
    async fn test_single_probe_failure_triggers_dead() {
        let (tx, mut rx) = mpsc::channel(16);
        let config = HealthConfig::default();
        let checker = HealthChecker::new(config, tx);

        let (app_tx, _app_rx) = mpsc::channel(16);
        let app_config = AppConfig {
            name: "test-app".to_string(),
            ..Default::default()
        };
        let app = Arc::new(App::new(app_config, app_tx, noop_log_handle()));
        let instance = app.allocate_instance();

        // Set instance as Healthy with a port nobody is listening on.
        instance.set_port(19999);
        instance.set_state(InstanceState::Healthy);

        // Spawn a long-running process so is_alive() returns true, forcing
        // the probe path (which will fail because nothing listens on 19999).
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        instance.set_process(child);

        checker.check_instance(&app, &instance).await;

        // The SDK owns the internal health endpoint; one failed probe after a
        // healthy startup means the instance cannot satisfy the runtime contract.
        let event = rx.try_recv().expect("should emit event");
        assert!(matches!(event, HealthEvent::Dead { .. }));
        assert_eq!(instance.state(), InstanceState::Stopped);

        // Clean up.
        let _ = instance.kill().await;
    }
}

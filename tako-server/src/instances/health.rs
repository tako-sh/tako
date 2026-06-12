//! Health checker - monitors instance health via HTTP probing
//!
//! Performs active HTTP health checks to each app's internal `.tako` host at
//! `/status` on each instance.
//! This replaces passive heartbeat-only detection with active probing.

use super::{App, AppLaunch, INTERNAL_TOKEN_HEADER, Instance, InstanceState};
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
            unhealthy_threshold: 2,
            dead_threshold: 3,
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
    async fn check_instance(&self, app: &App, instance: &Arc<Instance>) {
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
            app.set_instance_state(instance, InstanceState::Stopped);
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
        let (health_host, health_path, require_internal_token) = {
            let config = app.config.read();
            (
                config.health_check_host.clone(),
                config.health_check_path.clone(),
                !matches!(config.launch, AppLaunch::Container { .. }),
            )
        };

        // Perform HTTP probe
        let probe_result = probe_instance_health(
            instance,
            &health_host,
            &health_path,
            require_internal_token,
            self.config.probe_timeout,
        )
        .await;

        if probe_result.is_ok() {
            // Reset failure count and record heartbeat
            self.failure_counts.remove(&instance_key);
            instance.record_heartbeat();

            // Mark healthy on first successful probe.
            if current_state != InstanceState::Healthy {
                app.set_instance_state(instance, InstanceState::Healthy);

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
                app.set_instance_state(instance, new_state);

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

    #[cfg(test)]
    /// Get current failure count for an instance
    pub fn get_failure_count(&self, app_name: &str, instance_id: &str) -> u32 {
        let key = format!("{}:{}", app_name, instance_id);
        self.failure_counts.get(&key).map(|v| *v).unwrap_or(0)
    }

    #[cfg(test)]
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
    app.has_starting_instance()
}

async fn probe_instance_health(
    instance: &Instance,
    health_host: &str,
    health_path: &str,
    require_internal_token: bool,
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
        require_internal_token.then_some(instance.internal_token()),
        probe_timeout,
    )
    .await
}

async fn probe_endpoint_tcp(
    endpoint: std::net::SocketAddr,
    health_host: &str,
    health_path: &str,
    internal_token: Option<&str>,
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
    let token_header = internal_token
        .map(|token| format!("{INTERNAL_TOKEN_HEADER}: {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET {health_path} HTTP/1.1\r\nHost: {health_host}\r\n{token_header}Connection: close\r\n\r\n"
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
    http_response_is_success(&response, internal_token)
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

fn http_response_is_success(
    response: &str,
    expected_token: Option<&str>,
) -> Result<(), HealthProbeFailure> {
    let mut lines = response.lines();
    let status_line = lines.next().unwrap_or_default();
    if !http_status_is_success(status_line) {
        return Err(HealthProbeFailure::new(
            "bad_status",
            status_line.to_string(),
        ));
    }
    let Some(expected_token) = expected_token else {
        return Ok(());
    };

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
mod tests;

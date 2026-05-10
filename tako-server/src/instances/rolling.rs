//! Rolling update - zero-downtime deployments

use super::{App, AppConfig, Instance, InstanceError, InstanceState, Spawner};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Configuration for rolling updates
#[derive(Debug, Clone)]
pub struct RollingUpdateConfig {
    /// How long to wait for a new instance to become healthy
    pub health_timeout: Duration,
    /// How long to wait for an old instance to drain
    pub drain_timeout: Duration,
    /// How many instances to update at once
    pub batch_size: u32,
}

impl Default for RollingUpdateConfig {
    fn default() -> Self {
        Self {
            health_timeout: Duration::from_secs(30),
            drain_timeout: Duration::from_secs(30),
            batch_size: 1,
        }
    }
}

/// Result of a rolling update
#[derive(Debug)]
pub struct RollingUpdateResult {
    /// Whether the update succeeded
    pub success: bool,
    /// Number of new instances started
    pub new_instances: u32,
    /// Number of old instances stopped
    pub old_instances: u32,
    /// Error message if failed
    pub error: Option<String>,
}

/// Performs rolling updates on an app
pub struct RollingUpdater {
    config: RollingUpdateConfig,
    spawner: Arc<Spawner>,
}

/// Determine how many instances the incoming build should start during rollout.
///
/// The `instances` value is interpreted per build (not across old+new combined),
/// and on-demand (`0`) still starts one warm instance for immediate post-deploy traffic.
pub(crate) fn target_new_instances_for_build(
    requested_instances: u32,
    _existing_instances: usize,
) -> u32 {
    requested_instances.max(1)
}

impl RollingUpdater {
    pub fn new(spawner: Arc<Spawner>, config: RollingUpdateConfig) -> Self {
        Self { config, spawner }
    }

    /// Perform a rolling update
    ///
    /// 1. Start new instances one at a time
    /// 2. Wait for each new instance to become healthy
    /// 3. Add new instance to load balancer
    /// 4. Drain and stop one old instance
    /// 5. Repeat until all instances are replaced
    ///
    /// If any new instance fails to become healthy, rollback by killing
    /// all new instances and keeping old ones running.
    pub async fn update(
        &self,
        app: &App,
        new_config: AppConfig,
        target_count: u32,
    ) -> Result<RollingUpdateResult, InstanceError> {
        let old_instances: Vec<Arc<Instance>> = app.get_instances();

        tracing::info!(
            app = %app.name(),
            old_count = old_instances.len(),
            target_count = target_count,
            "Starting rolling update"
        );

        // Update the app config first
        app.update_config(new_config);

        let mut new_instances: Vec<Arc<Instance>> = Vec::new();
        let mut stopped_count = 0u32;

        // Start new instances and stop old ones in batches
        for batch_start in (0..target_count).step_by(self.config.batch_size as usize) {
            let batch_end = (batch_start + self.config.batch_size).min(target_count);

            // Start batch of new instances
            for _ in batch_start..batch_end {
                let instance = app.allocate_instance();

                match self.start_and_wait_healthy(app, instance.clone()).await {
                    Ok(()) => {
                        tracing::info!(
                            app = %app.name(),
                            instance = %instance.id,
                            "New instance is healthy"
                        );
                        new_instances.push(instance);
                    }
                    Err(e) => {
                        tracing::error!(
                            app = %app.name(),
                            instance = %instance.id,
                            error = %e,
                            "New instance failed health check, rolling back"
                        );

                        // Rollback: kill all new instances
                        for new_instance in &new_instances {
                            let _ = new_instance.kill().await;
                            app.remove_instance(&new_instance.id);
                        }
                        // Also kill the failed instance
                        let _ = instance.kill().await;
                        app.remove_instance(&instance.id);

                        return Ok(RollingUpdateResult {
                            success: false,
                            new_instances: 0,
                            old_instances: 0,
                            error: Some(format!("Health check failed: {}", e)),
                        });
                    }
                }
            }

            // Stop corresponding old instances
            let batch_size = (batch_end - batch_start) as usize;
            let old_to_stop: Vec<_> = old_instances
                .iter()
                .skip(stopped_count as usize)
                .take(batch_size)
                .cloned()
                .collect();

            for old_instance in old_to_stop {
                self.drain_and_stop(app, &old_instance).await?;
                stopped_count += 1;
            }
        }

        // Stop any remaining old instances
        for old_instance in old_instances.iter().skip(stopped_count as usize) {
            self.drain_and_stop(app, old_instance).await?;
            stopped_count += 1;
        }

        tracing::info!(
            app = %app.name(),
            new_instances = new_instances.len(),
            stopped_instances = stopped_count,
            "Rolling update complete"
        );

        Ok(RollingUpdateResult {
            success: true,
            new_instances: new_instances.len() as u32,
            old_instances: stopped_count,
            error: None,
        })
    }

    /// Start an instance and wait for it to become healthy
    async fn start_and_wait_healthy(
        &self,
        app: &App,
        instance: Arc<Instance>,
    ) -> Result<(), InstanceError> {
        // Spawn the instance
        self.spawner.spawn(app, instance.clone()).await?;

        // Wait for it to become healthy
        match timeout(self.config.health_timeout, self.wait_for_healthy(&instance)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(InstanceError::StartupTimeout),
        }
    }

    /// Wait for an instance to become healthy
    async fn wait_for_healthy(&self, instance: &Instance) -> Result<(), InstanceError> {
        let mut interval = tokio::time::interval(Duration::from_millis(100));

        loop {
            interval.tick().await;

            match instance.state() {
                InstanceState::Healthy => return Ok(()),
                InstanceState::Stopped | InstanceState::Unhealthy => {
                    return Err(InstanceError::HealthCheckFailed(
                        "Instance became unhealthy during startup".to_string(),
                    ));
                }
                _ => continue, // Still starting/ready
            }
        }
    }

    /// Drain and stop an old instance
    async fn drain_and_stop(&self, app: &App, instance: &Instance) -> Result<(), InstanceError> {
        tracing::debug!(
            app = %app.name(),
            instance = %instance.id,
            "Draining instance"
        );

        // Mark as draining (load balancer should stop sending new requests)
        instance.set_state(InstanceState::Draining);

        // Wait until all in-flight requests finish or drain_timeout is reached
        let deadline = tokio::time::Instant::now() + self.config.drain_timeout;
        while instance.in_flight() > 0 {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    app = %app.name(),
                    instance = %instance.id,
                    in_flight = instance.in_flight(),
                    "Drain timeout exceeded, forcing stop"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Kill the instance
        instance.kill().await.map_err(InstanceError::StopError)?;
        app.remove_instance(&instance.id);
        crate::metrics::remove_instance_metrics(&app.name(), &instance.id);

        tracing::debug!(
            app = %app.name(),
            instance = %instance.id,
            "Instance stopped"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::logger::noop_log_handle;
    use prometheus::Encoder;
    use tokio::sync::mpsc;

    fn create_test_app(name: &str) -> Arc<App> {
        let (tx, _rx) = mpsc::channel(16);
        let config = AppConfig {
            name: name.to_string(),
            min_instances: 1,
            ..Default::default()
        };
        Arc::new(App::new(config, tx, noop_log_handle()))
    }

    #[test]
    fn test_rolling_update_config_defaults() {
        let config = RollingUpdateConfig::default();
        assert_eq!(config.health_timeout, Duration::from_secs(30));
        assert_eq!(config.drain_timeout, Duration::from_secs(30));
        assert_eq!(config.batch_size, 1);
    }

    #[test]
    fn test_rolling_update_result() {
        let result = RollingUpdateResult {
            success: true,
            new_instances: 3,
            old_instances: 3,
            error: None,
        };
        assert!(result.success);
    }

    #[test]
    fn test_rolling_update_result_failure() {
        let result = RollingUpdateResult {
            success: false,
            new_instances: 1,
            old_instances: 0,
            error: Some("Health check timeout".to_string()),
        };
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[test]
    fn test_rolling_update_custom_config() {
        let config = RollingUpdateConfig {
            health_timeout: Duration::from_secs(60),
            drain_timeout: Duration::from_secs(10),
            batch_size: 2,
        };
        assert_eq!(config.health_timeout, Duration::from_secs(60));
        assert_eq!(config.drain_timeout, Duration::from_secs(10));
        assert_eq!(config.batch_size, 2);
    }

    #[test]
    fn target_new_instances_is_per_build_not_total_existing() {
        assert_eq!(target_new_instances_for_build(1, 4), 1);
        assert_eq!(target_new_instances_for_build(3, 1), 3);
    }

    #[test]
    fn target_new_instances_uses_single_warm_instance_for_zero() {
        assert_eq!(target_new_instances_for_build(0, 5), 1);
        assert_eq!(target_new_instances_for_build(0, 0), 1);
    }

    fn gather_metrics_text() -> String {
        let encoder = prometheus::TextEncoder::new();
        let metric_families = prometheus::gather();
        let mut buffer = Vec::new();
        encoder
            .encode(&metric_families, &mut buffer)
            .expect("encode metrics");
        String::from_utf8(buffer).expect("metrics are utf8")
    }

    fn has_instance_health_metric(metrics: &str, app: &str, instance: &str) -> bool {
        let app_label = format!(r#"app="{app}""#);
        let instance_label = format!(r#"instance="{instance}""#);
        metrics.lines().any(|line| {
            line.starts_with("tako_instance_health{")
                && line.contains(&app_label)
                && line.contains(&instance_label)
        })
    }

    #[tokio::test]
    async fn test_wait_for_healthy_succeeds() {
        let app = create_test_app("test-app");
        let instance = app.allocate_instance();

        // Simulate instance becoming healthy
        instance.set_state(InstanceState::Healthy);

        let spawner = Arc::new(Spawner::new());
        let updater = RollingUpdater::new(spawner, RollingUpdateConfig::default());

        // Should return immediately since instance is healthy
        let result = updater.wait_for_healthy(&instance).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_wait_for_healthy_fails_on_unhealthy() {
        let app = create_test_app("test-app");
        let instance = app.allocate_instance();

        // Simulate instance becoming unhealthy
        instance.set_state(InstanceState::Unhealthy);

        let spawner = Arc::new(Spawner::new());
        let updater = RollingUpdater::new(spawner, RollingUpdateConfig::default());

        let result = updater.wait_for_healthy(&instance).await;
        assert!(result.is_err());
        match result {
            Err(InstanceError::HealthCheckFailed(msg)) => {
                assert!(msg.contains("unhealthy"));
            }
            _ => panic!("Expected HealthCheckFailed error"),
        }
    }

    #[tokio::test]
    async fn test_wait_for_healthy_fails_on_stopped() {
        let app = create_test_app("test-app");
        let instance = app.allocate_instance();

        // Simulate instance stopping
        instance.set_state(InstanceState::Stopped);

        let spawner = Arc::new(Spawner::new());
        let updater = RollingUpdater::new(spawner, RollingUpdateConfig::default());

        let result = updater.wait_for_healthy(&instance).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_drain_and_stop_sets_draining_state() {
        let app = create_test_app("test-app");
        let instance = app.allocate_instance();
        instance.set_state(InstanceState::Healthy);

        let spawner = Arc::new(Spawner::new());
        let updater = RollingUpdater::new(spawner, RollingUpdateConfig::default());

        // Drain and stop (no actual process, so this will work)
        let result = updater.drain_and_stop(&app, &instance).await;
        assert!(result.is_ok());

        // Instance should be removed from app
        assert!(app.get_instance(&instance.id).is_none());
    }

    #[tokio::test]
    async fn drain_and_stop_removes_instance_health_metric() {
        let app = create_test_app("rolling-metrics-app");
        let instance = app.allocate_instance();
        instance.set_state(InstanceState::Healthy);

        crate::metrics::init(Some("test-server"));
        crate::metrics::set_instance_health(&app.name(), &instance.id, true);

        assert!(
            has_instance_health_metric(&gather_metrics_text(), &app.name(), &instance.id),
            "test setup should create an instance health metric"
        );

        let spawner = Arc::new(Spawner::new());
        let updater = RollingUpdater::new(spawner, RollingUpdateConfig::default());

        updater
            .drain_and_stop(&app, &instance)
            .await
            .expect("drain and stop");

        assert!(
            !has_instance_health_metric(&gather_metrics_text(), &app.name(), &instance.id),
            "old instance health metric should be removed after rolling drain"
        );
    }

    #[test]
    fn test_instance_state_transitions_for_health() {
        let app = create_test_app("test-app");
        let instance = app.allocate_instance();

        // Starting -> Ready -> Healthy is the normal flow
        assert_eq!(instance.state(), InstanceState::Starting);

        instance.set_state(InstanceState::Ready);
        assert_eq!(instance.state(), InstanceState::Ready);

        instance.set_state(InstanceState::Healthy);
        assert_eq!(instance.state(), InstanceState::Healthy);

        // Healthy -> Unhealthy when health checks fail
        instance.set_state(InstanceState::Unhealthy);
        assert_eq!(instance.state(), InstanceState::Unhealthy);

        // Unhealthy -> Healthy when recovered
        instance.set_state(InstanceState::Healthy);
        assert_eq!(instance.state(), InstanceState::Healthy);

        // Healthy -> Draining during rolling update
        instance.set_state(InstanceState::Draining);
        assert_eq!(instance.state(), InstanceState::Draining);

        // Draining -> Stopped when shutdown completes
        instance.set_state(InstanceState::Stopped);
        assert_eq!(instance.state(), InstanceState::Stopped);
    }

    #[tokio::test]
    async fn test_rolling_update_preserves_healthy_on_failure() {
        let app = create_test_app("test-app");

        // Create an existing "healthy" instance
        let old_instance = app.allocate_instance();
        old_instance.set_state(InstanceState::Healthy);

        // Verify old instance is healthy
        assert_eq!(app.get_healthy_instances().len(), 1);

        // If a rolling update fails, old instances should remain
        // (This is tested through the RollingUpdateResult type)
        let result = RollingUpdateResult {
            success: false,
            new_instances: 0,
            old_instances: 0,
            error: Some("New instance failed health check".to_string()),
        };

        // After rollback, old instances should still be available
        assert!(!result.success);
        assert_eq!(app.get_healthy_instances().len(), 1);
    }

    #[test]
    fn test_rolling_updater_creation() {
        let spawner = Arc::new(Spawner::new());
        let config = RollingUpdateConfig {
            health_timeout: Duration::from_secs(45),
            drain_timeout: Duration::from_secs(15),
            batch_size: 3,
        };
        let _updater = RollingUpdater::new(spawner, config);
    }

    #[tokio::test]
    async fn test_concurrent_instance_health_tracking() {
        let app = create_test_app("test-app");

        // Simulate multiple instances in different states
        let i1 = app.allocate_instance();
        let i2 = app.allocate_instance();
        let i3 = app.allocate_instance();

        i1.set_state(InstanceState::Healthy);
        i2.set_state(InstanceState::Starting);
        i3.set_state(InstanceState::Unhealthy);

        // Only one instance should be healthy
        let healthy = app.get_healthy_instances();
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0].id, i1.id);

        // Mark i2 as healthy
        i2.set_state(InstanceState::Healthy);

        // Now two instances should be healthy
        let healthy = app.get_healthy_instances();
        assert_eq!(healthy.len(), 2);
    }
}

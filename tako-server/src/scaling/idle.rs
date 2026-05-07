//! Idle timeout management - stops instances after period of inactivity

use crate::instances::App;
#[cfg(test)]
use crate::instances::Instance;
use crate::socket::InstanceState;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::interval;

/// Configuration for idle timeout
#[derive(Debug, Clone)]
pub struct IdleConfig {
    /// How often to check for idle instances
    pub check_interval: Duration,
}

impl Default for IdleConfig {
    fn default() -> Self {
        Self {
            // Faster feedback in debug/test; production can be coarser.
            check_interval: if cfg!(debug_assertions) {
                crate::defaults::IDLE_CHECK_INTERVAL_DEBUG
            } else {
                crate::defaults::IDLE_CHECK_INTERVAL_RELEASE
            },
        }
    }
}

/// Events from idle monitor
#[derive(Debug, Clone)]
pub enum IdleEvent {
    /// Instance became idle and should be stopped
    InstanceIdle { app: String, instance_id: String },
    /// App became fully idle (all instances stopped)
    AppIdle { app: String },
}

/// Monitors instances for idle timeout
pub struct IdleMonitor {
    config: IdleConfig,
    event_tx: mpsc::Sender<IdleEvent>,
}

impl IdleMonitor {
    pub fn new(config: IdleConfig, event_tx: mpsc::Sender<IdleEvent>) -> Self {
        Self { config, event_tx }
    }

    /// Start monitoring an app for idle instances
    pub async fn monitor_app(&self, app: Arc<App>) {
        let mut check_interval = interval(self.config.check_interval);

        loop {
            check_interval.tick().await;

            let (idle_timeout, min_instances) = {
                let config = app.config.read();
                (config.idle_timeout, config.min_instances)
            };

            let instances = app.get_instances();
            let healthy_count = instances
                .iter()
                .filter(|i| i.state() == InstanceState::Healthy)
                .count();

            // Find idle instances that can be stopped
            let mut idle_instances: Vec<_> = instances
                .iter()
                .filter(|i| i.state() == InstanceState::Healthy && i.idle_time() > idle_timeout)
                .cloned()
                .collect();

            // Sort by idle time (most idle first)
            idle_instances.sort_by_key(|instance| std::cmp::Reverse(instance.idle_time()));

            // Calculate how many we can stop while respecting min_instances
            let can_stop = healthy_count.saturating_sub(min_instances as usize);

            // Stop idle instances
            for instance in idle_instances.into_iter().take(can_stop) {
                tracing::info!(
                    app = %app.name(),
                    instance = %instance.id,
                    idle_time = ?instance.idle_time(),
                    "Stopping idle instance"
                );

                let _ = self
                    .event_tx
                    .send(IdleEvent::InstanceIdle {
                        app: app.name(),
                        instance_id: instance.id.clone(),
                    })
                    .await;
            }

            // Check if app is fully idle (no running instances)
            let running_count = instances
                .iter()
                .filter(|i| {
                    matches!(
                        i.state(),
                        InstanceState::Starting | InstanceState::Ready | InstanceState::Healthy
                    )
                })
                .count();

            if running_count == 0 && min_instances == 0 {
                let _ = self
                    .event_tx
                    .send(IdleEvent::AppIdle { app: app.name() })
                    .await;
            }
        }
    }

    #[cfg(test)]
    /// Check if an instance should be stopped due to idle timeout
    pub fn should_stop_instance(
        &self,
        instance: &Instance,
        idle_timeout: Duration,
        min_instances: u32,
        current_healthy: u32,
    ) -> bool {
        // Never stop if we're at or below minimum
        if current_healthy <= min_instances {
            return false;
        }

        // Stop if instance is idle for longer than timeout.
        // Avoid killing instances while they have in-flight requests.
        instance.state() == InstanceState::Healthy
            && instance.in_flight() == 0
            && instance.idle_time() > idle_timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::logger::noop_log_handle;
    use tokio::sync::mpsc;

    #[test]
    fn test_idle_config_defaults() {
        let config = IdleConfig::default();
        if cfg!(debug_assertions) {
            assert_eq!(config.check_interval, Duration::from_secs(1));
        } else {
            assert_eq!(config.check_interval, Duration::from_secs(30));
        }
    }

    #[tokio::test]
    async fn test_idle_monitor_creation() {
        let (tx, _rx) = mpsc::channel(16);
        let config = IdleConfig::default();
        let _monitor = IdleMonitor::new(config, tx);
    }

    #[test]
    fn test_should_stop_instance() {
        let (tx, _rx) = mpsc::channel(16);
        let monitor = IdleMonitor::new(IdleConfig::default(), tx);

        let instance = Instance::new("test-1".to_string(), "v1".to_string(), noop_log_handle());
        instance.set_state(InstanceState::Healthy);

        // Can't stop if at min_instances
        assert!(!monitor.should_stop_instance(
            &instance,
            Duration::from_secs(0),
            1, // min
            1  // current
        ));

        // Can't stop while in-flight.
        instance.request_started();
        assert!(!monitor.should_stop_instance(
            &instance,
            Duration::from_secs(0),
            0, // min
            1  // current
        ));
        instance.request_finished();

        // Can stop if above min_instances and idle
        // (but idle_time() will be very small, so this test is limited)
    }
}

//! Cold start handler - manages request queuing during instance startup

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, broadcast};

/// Configuration for cold start handling
#[derive(Debug, Clone)]
pub struct ColdStartConfig {
    /// Maximum time to wait for an instance to start
    pub startup_timeout: Duration,
    /// Maximum number of requests to queue during cold start
    pub max_queued_requests: usize,
}

impl Default for ColdStartConfig {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(30),
            max_queued_requests: 1000,
        }
    }
}

/// State of a cold start for an app
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdStartState {
    /// No cold start in progress
    Idle,
    /// Instance is starting
    Starting,
    /// Instance is ready
    Ready,
    /// Cold start failed
    Failed,
}

/// Tracks cold start state for an app
struct AppColdStart {
    /// Current state
    state: ColdStartState,
    /// When the cold start began
    started_at: Option<Instant>,
    /// Channel to notify waiters when ready
    ready_tx: Option<broadcast::Sender<bool>>,
    /// Queue permits belong to this attempt, including after reset or retry.
    queue_slots: Arc<Semaphore>,
}

impl AppColdStart {
    fn new() -> Self {
        Self {
            state: ColdStartState::Idle,
            started_at: None,
            ready_tx: None,
            queue_slots: Arc::new(Semaphore::new(0)),
        }
    }
}

/// Manages cold starts for all apps
pub struct ColdStartManager {
    config: ColdStartConfig,
    /// Per-app cold start state
    apps: Mutex<HashMap<String, AppColdStart>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColdStartBegin {
    pub leader: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitForReadyOutcome {
    Ready,
    Failed,
    Timeout,
    QueueFull,
}

impl ColdStartManager {
    pub fn new(config: ColdStartConfig) -> Self {
        Self {
            config,
            apps: Mutex::new(HashMap::new()),
        }
    }

    /// Check if an app is currently in cold start
    pub fn is_cold_starting(&self, app_name: &str) -> bool {
        let apps = self.apps.lock();
        apps.get(app_name)
            .map(|cs| cs.state == ColdStartState::Starting)
            .unwrap_or(false)
    }

    /// Begin a cold start for an app.
    ///
    /// Returns whether the caller is responsible for actually spawning the instance.
    pub fn begin(&self, app_name: &str) -> ColdStartBegin {
        let mut apps = self.apps.lock();
        let cold_start = apps
            .entry(app_name.to_string())
            .or_insert_with(AppColdStart::new);

        match cold_start.state {
            ColdStartState::Idle | ColdStartState::Failed => {
                let (tx, _rx) = broadcast::channel(1);
                cold_start.state = ColdStartState::Starting;
                cold_start.started_at = Some(Instant::now());
                cold_start.ready_tx = Some(tx);
                cold_start.queue_slots = Arc::new(Semaphore::new(self.config.max_queued_requests));
                ColdStartBegin { leader: true }
            }
            ColdStartState::Starting | ColdStartState::Ready => ColdStartBegin { leader: false },
        }
    }

    /// Wait for a cold start to complete
    /// Returns true if instance is ready, false if failed/timeout
    pub async fn wait_for_ready(&self, app_name: &str) -> bool {
        matches!(
            self.wait_for_ready_outcome(app_name).await,
            WaitForReadyOutcome::Ready
        )
    }

    pub async fn wait_for_ready_outcome(&self, app_name: &str) -> WaitForReadyOutcome {
        let (rx, _permit) = {
            let apps = self.apps.lock();
            match apps.get(app_name) {
                Some(cs) if cs.state == ColdStartState::Ready => return WaitForReadyOutcome::Ready,
                Some(cs) if cs.state == ColdStartState::Starting => {
                    let Ok(permit) = cs.queue_slots.clone().try_acquire_owned() else {
                        return WaitForReadyOutcome::QueueFull;
                    };
                    (cs.ready_tx.as_ref().map(|tx| tx.subscribe()), permit)
                }
                _ => return WaitForReadyOutcome::Failed,
            }
        };

        if let Some(mut rx) = rx {
            match tokio::time::timeout(self.config.startup_timeout, rx.recv()).await {
                Ok(Ok(true)) => WaitForReadyOutcome::Ready,
                Ok(Ok(false)) | Ok(Err(_)) => WaitForReadyOutcome::Failed,
                Err(_) => WaitForReadyOutcome::Timeout,
            }
        } else {
            WaitForReadyOutcome::Failed
        }
    }

    /// Mark cold start as complete (instance is ready)
    pub fn mark_ready(&self, app_name: &str) {
        let mut apps = self.apps.lock();
        if let Some(cold_start) = apps.get_mut(app_name) {
            if let Some(started_at) = cold_start.started_at {
                crate::metrics::record_cold_start(app_name, started_at.elapsed().as_secs_f64());
            }
            cold_start.state = ColdStartState::Ready;
            if let Some(tx) = cold_start.ready_tx.take() {
                let _ = tx.send(true);
            }
        }
    }

    /// Mark cold start as failed. `reason` becomes a label on
    /// `tako_cold_start_failures_total`; use a small fixed set such as
    /// `spawn_failed` or `instance_dead`.
    pub fn mark_failed(&self, app_name: &str, reason: &str) {
        let mut apps = self.apps.lock();
        if let Some(cold_start) = apps.get_mut(app_name) {
            if let Some(started_at) = cold_start.started_at {
                crate::metrics::record_cold_start(app_name, started_at.elapsed().as_secs_f64());
            }
            crate::metrics::record_cold_start_failure(app_name, reason);
            cold_start.state = ColdStartState::Failed;
            if let Some(tx) = cold_start.ready_tx.take() {
                let _ = tx.send(false);
            }
        }
    }

    /// Reset cold start state (e.g., when app is stopped)
    pub fn reset(&self, app_name: &str) {
        let mut apps = self.apps.lock();
        apps.remove(app_name);
    }

    /// Get elapsed time since cold start began
    pub fn elapsed(&self, app_name: &str) -> Option<Duration> {
        let apps = self.apps.lock();
        apps.get(app_name)
            .and_then(|cs| cs.started_at.map(|t| t.elapsed()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::time::Duration;

    #[test]
    fn test_cold_start_config_defaults() {
        let config = ColdStartConfig::default();
        assert_eq!(config.startup_timeout, Duration::from_secs(30));
        assert_eq!(config.max_queued_requests, 1000);
    }

    #[test]
    fn test_cold_start_manager_creation() {
        let manager = ColdStartManager::new(ColdStartConfig::default());
        assert!(!manager.is_cold_starting("my-app"));
    }

    #[test]
    fn test_begin_cold_start() {
        let manager = ColdStartManager::new(ColdStartConfig::default());

        // First call should be leader
        let r1 = manager.begin("my-app");
        assert!(r1.leader);
        assert!(manager.is_cold_starting("my-app"));

        // Second call should not be leader
        let r2 = manager.begin("my-app");
        assert!(!r2.leader);
    }

    #[test]
    fn test_mark_ready() {
        let manager = ColdStartManager::new(ColdStartConfig::default());

        manager.begin("my-app");
        assert!(manager.is_cold_starting("my-app"));

        manager.mark_ready("my-app");
        assert!(!manager.is_cold_starting("my-app"));
    }

    #[test]
    fn test_mark_failed() {
        let manager = ColdStartManager::new(ColdStartConfig::default());

        manager.begin("my-app");
        manager.mark_failed("my-app", "spawn_failed");

        assert!(!manager.is_cold_starting("my-app"));
    }

    #[test]
    fn test_mark_failed_records_failure_metric() {
        // Use unique app name so the counter starts at 0
        let manager = ColdStartManager::new(ColdStartConfig::default());
        crate::metrics::init(Some("test-server"));

        manager.begin("metric-test-app");
        let before = crate::metrics::COLD_START_FAILURES_TOTAL
            .with_label_values(&["test-server", "metric-test-app", "spawn_failed"])
            .get();
        manager.mark_failed("metric-test-app", "spawn_failed");
        let after = crate::metrics::COLD_START_FAILURES_TOTAL
            .with_label_values(&["test-server", "metric-test-app", "spawn_failed"])
            .get();
        assert_eq!(after, before + 1);
    }

    #[test]
    fn test_reset() {
        let manager = ColdStartManager::new(ColdStartConfig::default());

        manager.begin("my-app");
        manager.reset("my-app");

        assert!(!manager.is_cold_starting("my-app"));
    }

    #[tokio::test]
    async fn test_wait_for_ready_immediate() {
        let manager = Arc::new(ColdStartManager::new(ColdStartConfig::default()));

        manager.begin("my-app");
        manager.mark_ready("my-app");

        // Should return immediately since already ready
        let result = manager.wait_for_ready("my-app").await;
        assert!(result);
    }

    #[tokio::test]
    async fn test_wait_for_ready_with_delay() {
        let manager = Arc::new(ColdStartManager::new(ColdStartConfig::default()));

        manager.begin("my-app");

        let manager_clone = manager.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            manager_clone.mark_ready("my-app");
        });

        let result = manager.wait_for_ready("my-app").await;
        assert!(result);
    }

    #[tokio::test]
    async fn test_wait_for_ready_outcome_returns_queue_full_when_limit_reached() {
        let manager = Arc::new(ColdStartManager::new(ColdStartConfig {
            startup_timeout: Duration::from_secs(1),
            max_queued_requests: 1,
        }));

        manager.begin("my-app");

        let manager_clone = manager.clone();
        let first_waiter =
            tokio::spawn(async move { manager_clone.wait_for_ready_outcome("my-app").await });

        tokio::time::sleep(Duration::from_millis(25)).await;

        let second = manager.wait_for_ready_outcome("my-app").await;
        assert_eq!(second, WaitForReadyOutcome::QueueFull);

        manager.mark_failed("my-app", "spawn_failed");
        assert_eq!(
            first_waiter.await.expect("first waiter should complete"),
            WaitForReadyOutcome::Failed
        );
    }

    #[tokio::test]
    async fn cancelled_waiter_releases_its_queue_slot() {
        use std::future::{Future, poll_fn};
        use std::task::Poll;

        let manager = ColdStartManager::new(ColdStartConfig {
            startup_timeout: Duration::from_secs(1),
            max_queued_requests: 1,
        });
        manager.begin("app");
        let mut cancelled = Box::pin(manager.wait_for_ready_outcome("app"));
        assert!(
            poll_fn(|cx| Poll::Ready(cancelled.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        drop(cancelled);

        let mut next = Box::pin(manager.wait_for_ready_outcome("app"));
        assert!(
            poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        manager.mark_ready("app");
        assert_eq!(next.await, WaitForReadyOutcome::Ready);
    }

    #[tokio::test]
    async fn old_waiter_cannot_release_a_new_attempts_queue_slot() {
        use std::future::{Future, poll_fn};
        use std::task::Poll;

        let manager = ColdStartManager::new(ColdStartConfig {
            startup_timeout: Duration::from_secs(1),
            max_queued_requests: 1,
        });
        for reset in [false, true] {
            manager.reset("app");
            manager.begin("app");
            let mut old = Box::pin(manager.wait_for_ready_outcome("app"));
            assert!(
                poll_fn(|cx| Poll::Ready(old.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            if reset {
                manager.reset("app");
            } else {
                manager.mark_failed("app", "spawn_failed");
            }
            assert!(manager.begin("app").leader);
            let mut current = Box::pin(manager.wait_for_ready_outcome("app"));
            assert!(
                poll_fn(|cx| Poll::Ready(current.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );

            assert_eq!(old.await, WaitForReadyOutcome::Failed);
            assert_eq!(
                manager.wait_for_ready_outcome("app").await,
                WaitForReadyOutcome::QueueFull,
            );
            manager.mark_ready("app");
            assert_eq!(current.await, WaitForReadyOutcome::Ready);
        }
    }
}

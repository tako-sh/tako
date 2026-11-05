use crate::ServerState;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

pub(crate) fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub(crate) fn spawn(
    rt: &tokio::runtime::Runtime,
    state: Arc<ServerState>,
) -> tokio::task::JoinHandle<()> {
    assert!(
        !state.drain_observer_started.swap(true, Ordering::AcqRel),
        "drain observer already started"
    );
    rt.spawn(async move {
        loop {
            if let Err(error) = state.observe_drain_once().await {
                tracing::warn!(%error, "Shared server drain reconciliation failed");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
}

impl ServerState {
    pub(crate) async fn observe_drain_once(&self) -> Result<(), String> {
        let requested = self.state_store.drain_requested().map_err(|error| {
            self.drain_requested.store(true, Ordering::Release);
            error.to_string()
        })?;
        if requested {
            self.drain_requested.store(true, Ordering::Release);
        }
        if !self.drain_requested.load(Ordering::Acquire) {
            return Ok(());
        }
        let _management = self.management_lock.lock().await;
        let requested = self
            .state_store
            .drain_requested()
            .map_err(|error| error.to_string())?;
        if requested {
            if self.drain_activity.lock().is_some() {
                let wall_deadline = self
                    .state_store
                    .drain_deadline_unix_ms()
                    .map_err(|error| error.to_string())?;
                let remaining = Duration::from_millis(
                    wall_deadline.saturating_sub(unix_millis()).min(3_600_000),
                );
                let report = self.drain_runtime_until(Instant::now() + remaining).await;
                if !report.completed {
                    return Err(format!(
                        "Local runtime cleanup remains incomplete: {}",
                        serde_json::to_string(&report).unwrap()
                    ));
                }
            }
            return Ok(());
        }
        let Some(_operation) = self
            .state_store
            .try_drain_operation()
            .map_err(|error| error.to_string())?
        else {
            return Ok(());
        };
        // A drain may have won the operation lock after the previous read.
        if self
            .state_store
            .drain_requested()
            .map_err(|error| error.to_string())?
        {
            return Ok(());
        }
        let cleanup = self.drain_runtime_until(Instant::now()).await;
        if !cleanup.completed {
            return Err("Local cleanup remains incomplete before resume".into());
        }
        self.restore_drained_runtime(false).await
    }
}

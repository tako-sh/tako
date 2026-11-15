use super::super::TakoProxy;
use crate::lb::Backend;
use crate::scaling::WaitForReadyOutcome;
use std::time::Duration;

pub(crate) enum BackendResolution {
    Ready {
        backend: Backend,
        cold_start_wait: Option<Duration>,
    },
    StartupTimeout,
    StartupFailed,
    QueueFull,
    Unavailable,
    AppMissing,
}

impl TakoProxy {
    pub(crate) async fn resolve_backend(&self, app_name: &str) -> BackendResolution {
        self.resolve_service_backend(app_name, None).await
    }

    pub(crate) async fn resolve_service_backend(
        &self,
        app_name: &str,
        service: Option<&str>,
    ) -> BackendResolution {
        resolve_service_backend(
            &self.lb,
            &self.cold_start,
            &self.draining,
            &self.spawn_gate,
            app_name,
            service,
        )
        .await
    }
}

pub(crate) async fn resolve_app_backend(
    lb: &std::sync::Arc<crate::lb::LoadBalancer>,
    cold_start: &std::sync::Arc<crate::scaling::ColdStartManager>,
    draining: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    spawn_gate: &std::sync::Arc<tokio::sync::RwLock<()>>,
    app_name: &str,
) -> BackendResolution {
    resolve_service_backend(lb, cold_start, draining, spawn_gate, app_name, None).await
}

pub(crate) async fn resolve_service_backend(
    lb: &std::sync::Arc<crate::lb::LoadBalancer>,
    cold_start: &std::sync::Arc<crate::scaling::ColdStartManager>,
    draining: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    spawn_gate: &std::sync::Arc<tokio::sync::RwLock<()>>,
    app_name: &str,
    service: Option<&str>,
) -> BackendResolution {
    if draining.load(std::sync::atomic::Ordering::Acquire) {
        return BackendResolution::Unavailable;
    }
    let Some(app) = lb.app_manager().get_app(app_name) else {
        return BackendResolution::AppMissing;
    };
    let lifecycle = if app.state() == crate::socket::AppState::Deploying {
        // A request arriving at cutover waits for the committed backend set.
        app.lifecycle_gate.clone().read_owned().await
    } else {
        match app.lifecycle_gate.clone().try_read_owned() {
            Ok(admission) => admission,
            // A deploy may have started between the state check and admission.
            Err(_) if app.state() == crate::socket::AppState::Deploying => {
                app.lifecycle_gate.clone().read_owned().await
            }
            Err(_) => return BackendResolution::Unavailable,
        }
    };
    if draining.load(std::sync::atomic::Ordering::Acquire)
        || app.state() == crate::socket::AppState::Stopped
    {
        return BackendResolution::Unavailable;
    }
    let backend = match service {
        Some(service) => lb.get_service_backend(app_name, service),
        None => lb.get_backend(app_name),
    };
    if let Some(backend) = backend {
        return BackendResolution::Ready {
            backend,
            cold_start_wait: None,
        };
    }

    let allows_cold_start = {
        let config = app.config.read();
        config.compose.is_none() && config.min_instances == 0
    };
    if service.is_some() || !allows_cold_start {
        return BackendResolution::Unavailable;
    }

    let begin = cold_start.begin(app_name);
    if begin.leader {
        let app_name = app_name.to_string();
        let app = app.clone();
        let spawner = lb.app_manager().spawner();
        let cold_start = cold_start.clone();
        let draining = draining.clone();
        let spawn_gate = spawn_gate.clone();

        tokio::spawn(async move {
            // Transfer admission to the task before it can be scheduled. A stop
            // waits for this attempt, then removes every instance it produced.
            let _lifecycle = lifecycle;
            // A drain holds the gate for writing while it stops instances, so
            // a spawn admitted here is still stopped by it. Never wait: a
            // pending drain fails every cold start anyway.
            let Ok(_spawning) = spawn_gate.try_read() else {
                cold_start.mark_failed(&app_name, "server_draining");
                return;
            };
            if draining.load(std::sync::atomic::Ordering::Acquire) {
                cold_start.mark_failed(&app_name, "server_draining");
                return;
            }
            app.set_state_unless_deploying(crate::socket::AppState::Running);
            let instance = app.allocate_instance();
            if let Err(e) = spawner.spawn(&app, instance.clone()).await {
                tracing::error!(app = %app_name, "cold start spawn failed: {}", e);
                app.set_state_unless_deploying(crate::socket::AppState::Error);
                app.set_last_error(format!("Cold start failed: {}", e));
                app.remove_instance(&instance.id);
                cold_start.mark_failed(&app_name, "spawn_failed");
            }
        });
    } else {
        drop(lifecycle);
    }

    let wait_started_at = std::time::Instant::now();
    match cold_start.wait_for_ready_outcome(app_name).await {
        WaitForReadyOutcome::Ready => {
            lb.get_backend(app_name)
                .map_or(BackendResolution::StartupFailed, |backend| {
                    BackendResolution::Ready {
                        backend,
                        cold_start_wait: Some(wait_started_at.elapsed()),
                    }
                })
        }
        WaitForReadyOutcome::Timeout => BackendResolution::StartupTimeout,
        WaitForReadyOutcome::Failed => BackendResolution::StartupFailed,
        WaitForReadyOutcome::QueueFull => BackendResolution::QueueFull,
    }
}

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
        if let Some(backend) = self.lb.get_backend(app_name) {
            return BackendResolution::Ready {
                backend,
                cold_start_wait: None,
            };
        }

        let Some(app) = self.lb.app_manager().get_app(app_name) else {
            return BackendResolution::AppMissing;
        };

        if app.config.read().min_instances != 0 {
            return BackendResolution::Unavailable;
        }

        let begin = self.cold_start.begin(app_name);
        if begin.leader {
            app.set_state(crate::socket::AppState::Running);

            let app_name = app_name.to_string();
            let app = app.clone();
            let spawner = self.lb.app_manager().spawner();
            let cold_start = self.cold_start.clone();

            tokio::spawn(async move {
                let instance = app.allocate_instance();
                if let Err(e) = spawner.spawn(&app, instance.clone()).await {
                    tracing::error!(app = %app_name, "cold start spawn failed: {}", e);
                    app.set_state(crate::socket::AppState::Error);
                    app.set_last_error(format!("Cold start failed: {}", e));
                    app.remove_instance(&instance.id);
                    cold_start.mark_failed(&app_name, "spawn_failed");
                }
            });
        }

        let wait_started_at = std::time::Instant::now();
        match self.cold_start.wait_for_ready_outcome(app_name).await {
            WaitForReadyOutcome::Ready => {
                self.lb
                    .get_backend(app_name)
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
}

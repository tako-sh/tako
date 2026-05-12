//! Instance spawner - spawns and monitors app processes

#[cfg(test)]
mod health_probe;
mod readiness;
mod spawn_command;

use super::{App, Instance, InstanceError, InstanceEvent, InstanceState};
#[cfg(test)]
use health_probe::probe_endpoint_tcp;
use readiness::{startup_timeout_detail, wait_for_ready};
use spawn_command::{
    build_instance_args, build_instance_env, resolve_app_user, spawn_child_process,
};
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;
use tokio::time::timeout;

/// Spawns and monitors app instances
pub struct Spawner {
    /// UID/GID of the `tako-app` user for process isolation (Unix only).
    #[cfg(unix)]
    app_user: Result<Option<(u32, u32)>, String>,
    /// Path to the shared Tako internal socket. When present, injected into
    /// every spawned instance as `TAKO_INTERNAL_SOCKET` so workflow `.enqueue()`
    /// and channel `.publish()` from app code work. `None` in tests.
    internal_socket: Option<PathBuf>,
}

impl Spawner {
    pub fn new() -> Self {
        Self {
            #[cfg(unix)]
            app_user: resolve_app_user().map_err(|error| error.to_string()),
            internal_socket: None,
        }
    }

    pub fn with_internal_socket(mut self, path: PathBuf) -> Self {
        self.internal_socket = Some(path);
        self
    }
}

impl Spawner {
    /// Spawn a new instance
    pub async fn spawn(&self, app: &App, instance: Arc<Instance>) -> Result<(), InstanceError> {
        let config = app.config.read().clone();
        let app_name = config.deployment_id();
        let instance_id = instance.id.clone();

        tracing::info!(
            app = %app_name,
            instance = %instance_id,
            "Spawning instance"
        );

        let env = build_instance_env(&config, &instance, self.internal_socket.as_deref());
        let extra_args = build_instance_args(&instance);

        #[cfg(unix)]
        let app_user = app_user_for_spawn(&self.app_user, crate::unix::is_root())
            .map_err(InstanceError::SpawnError)?;
        #[cfg(not(unix))]
        let app_user = None;

        let (child, readiness_fd) = spawn_child_process(
            &config,
            &env,
            &extra_args,
            app_user,
            instance.internal_token(),
            &config.secrets,
            &config.image_secret,
        )
        .map_err(InstanceError::SpawnError)?;

        instance.set_process(child);
        instance.set_state(InstanceState::Starting);

        // Notify about start
        let _ = app
            .instance_tx
            .send(InstanceEvent::Started {
                app: app_name.clone(),
                instance_id: instance_id.clone(),
            })
            .await;

        // Wait for the SDK to report the bound port on fd 4.
        match timeout(
            config.startup_timeout,
            wait_for_ready(instance.clone(), readiness_fd),
        )
        .await
        {
            Ok(Ok(())) => {
                instance.set_state(InstanceState::Healthy);

                instance.drain_pipes();

                tracing::info!(
                    app = %app_name,
                    instance = %instance_id,
                    "Instance is healthy"
                );

                let _ = app
                    .instance_tx
                    .send(InstanceEvent::Ready {
                        app: app_name,
                        instance_id,
                    })
                    .await;

                Ok(())
            }
            Ok(Err(e)) => {
                instance.set_state(InstanceState::Unhealthy);
                let _ = instance.kill().await;
                Err(e)
            }
            Err(_) => {
                instance.set_state(InstanceState::Unhealthy);
                let detail = startup_timeout_detail(instance.clone(), config.startup_timeout).await;
                instance.cleanup_upstream();
                Err(InstanceError::StartupTimeoutWithDetail(detail))
            }
        }
    }

    #[cfg(test)]
    /// Run health check on an instance
    pub async fn health_check(&self, app: &App, instance: &Instance) -> bool {
        let (health_check_path, health_check_host) = {
            let config = app.config.read();
            (
                config.health_check_path.clone(),
                config.health_check_host.clone(),
            )
        };

        self.probe_health(
            instance,
            &health_check_path,
            &health_check_host,
            Duration::from_secs(5),
        )
        .await
    }

    #[cfg(test)]
    async fn probe_health(
        &self,
        instance: &Instance,
        health_check_path: &str,
        health_check_host: &str,
        probe_timeout: Duration,
    ) -> bool {
        let Some(endpoint) = instance.endpoint() else {
            return false;
        };
        matches!(
            probe_endpoint_tcp(
                endpoint,
                health_check_path,
                health_check_host,
                instance.internal_token(),
                probe_timeout,
            )
            .await,
            Ok(true)
        )
    }
}

#[cfg(unix)]
fn app_user_for_spawn(
    app_user: &Result<Option<(u32, u32)>, String>,
    is_root: bool,
) -> std::io::Result<Option<(u32, u32)>> {
    match app_user {
        Ok(Some(user)) => Ok(Some(*user)),
        Ok(None) if is_root => Err(std::io::Error::other(
            "tako-app user not found; refusing to spawn app process as root",
        )),
        Ok(None) => {
            tracing::warn!("tako-app user not found; app processes will run as current user");
            Ok(None)
        }
        Err(error) if is_root => Err(std::io::Error::other(format!(
            "failed to resolve tako-app user: {error}"
        ))),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "Failed to resolve tako-app user; app processes will run as current user"
            );
            Ok(None)
        }
    }
}

impl Default for Spawner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;

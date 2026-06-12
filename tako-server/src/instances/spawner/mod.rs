//! Instance spawner - spawns and monitors app processes

mod health_probe;
mod readiness;
mod spawn_command;

use super::{App, AppLaunch, Instance, InstanceError, InstanceEvent, InstanceState};
use health_probe::probe_endpoint_tcp;
use readiness::{startup_timeout_detail, wait_for_ready};
use spawn_command::{
    build_container_env, build_instance_args, build_instance_env, spawn_child_process,
    spawn_container_process,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Spawns and monitors app instances
pub struct Spawner {
    /// Server data directory used to derive app cgroup paths.
    data_dir: PathBuf,
    /// Path to the shared Tako internal socket. When present, injected into
    /// every spawned instance as `TAKO_INTERNAL_SOCKET` so workflow `.enqueue()`
    /// and channel `.publish()` from app code work. `None` in tests.
    internal_socket: Option<PathBuf>,
}

impl Spawner {
    pub fn new() -> Self {
        Self {
            data_dir: PathBuf::new(),
            internal_socket: None,
        }
    }

    pub fn with_data_dir(mut self, path: PathBuf) -> Self {
        self.data_dir = path;
        self
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
        if let AppLaunch::Container { image, port } = config.launch.clone() {
            return self
                .spawn_container(app, instance, config, image, port)
                .await;
        }

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
        let isolation = crate::isolation::app_process_isolation(&self.data_dir, &app_name)
            .map_err(|error| InstanceError::SpawnError(std::io::Error::other(error)))?;

        let (child, readiness_fd) = spawn_child_process(
            &config,
            &env,
            &extra_args,
            #[cfg(unix)]
            isolation,
            instance.internal_token(),
            &config.secrets,
        )
        .map_err(InstanceError::SpawnError)?;

        instance.set_process(child);
        app.set_instance_state(&instance, InstanceState::Starting);

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
                app.set_instance_state(&instance, InstanceState::Healthy);

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
                app.set_instance_state(&instance, InstanceState::Unhealthy);
                let _ = instance.kill().await;
                Err(e)
            }
            Err(_) => {
                app.set_instance_state(&instance, InstanceState::Unhealthy);
                let detail = startup_timeout_detail(instance.clone(), config.startup_timeout).await;
                instance.cleanup_upstream();
                Err(InstanceError::StartupTimeoutWithDetail(detail))
            }
        }
    }

    async fn spawn_container(
        &self,
        app: &App,
        instance: Arc<Instance>,
        config: super::AppConfig,
        image: String,
        container_port: u16,
    ) -> Result<(), InstanceError> {
        let app_name = config.deployment_id();
        let instance_id = instance.id.clone();
        let host_port = allocate_loopback_port().map_err(InstanceError::SpawnError)?;
        let env = build_container_env(&config, container_port);
        let child = spawn_container_process(
            &config,
            &image,
            container_port,
            host_port,
            &env,
            &config.secrets,
            &instance,
        )
        .map_err(InstanceError::SpawnError)?;

        instance.set_process(child);
        instance.set_port(host_port);
        app.set_instance_state(&instance, InstanceState::Starting);

        let _ = app
            .instance_tx
            .send(InstanceEvent::Started {
                app: app_name.clone(),
                instance_id: instance_id.clone(),
            })
            .await;

        match timeout(
            config.startup_timeout,
            wait_for_container_health(app, instance.clone(), &config),
        )
        .await
        {
            Ok(Ok(())) => {
                app.set_instance_state(&instance, InstanceState::Healthy);
                instance.drain_pipes();
                let _ = app
                    .instance_tx
                    .send(InstanceEvent::Ready {
                        app: app_name,
                        instance_id,
                    })
                    .await;
                Ok(())
            }
            Ok(Err(error)) => {
                app.set_instance_state(&instance, InstanceState::Unhealthy);
                let _ = instance.kill().await;
                Err(error)
            }
            Err(_) => {
                app.set_instance_state(&instance, InstanceState::Unhealthy);
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
                Some(instance.internal_token()),
                probe_timeout,
            )
            .await,
            Ok(true)
        )
    }
}

fn allocate_loopback_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

async fn wait_for_container_health(
    app: &App,
    instance: Arc<Instance>,
    config: &super::AppConfig,
) -> Result<(), InstanceError> {
    loop {
        if !instance.is_alive().await {
            return Err(InstanceError::HealthCheckFailed(
                "container exited before becoming healthy".to_string(),
            ));
        }
        if probe_endpoint_tcp(
            instance
                .endpoint()
                .ok_or_else(|| InstanceError::HealthCheckFailed("missing endpoint".to_string()))?,
            &config.health_check_path,
            &config.health_check_host,
            None,
            Duration::from_millis(500),
        )
        .await
        .unwrap_or(false)
        {
            app.set_instance_state(&instance, InstanceState::Ready);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

impl Default for Spawner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;

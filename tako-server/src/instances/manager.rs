use crate::socket::{AppState, InstanceState};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::{
    App, AppConfig, InstanceError, InstanceEvent, Spawner, register_app_logger, spawn_app_logger,
    unregister_app_logger,
};

/// Manages all apps
pub struct AppManager {
    /// All registered apps
    apps: DashMap<String, Arc<App>>,
    /// Instance spawner
    spawner: Arc<Spawner>,
    /// Event channel sender
    event_tx: mpsc::Sender<InstanceEvent>,
    /// Event channel receiver (for the manager loop)
    event_rx: RwLock<Option<mpsc::Receiver<InstanceEvent>>>,
    /// Server data directory (for app log paths)
    data_dir: PathBuf,
}

impl AppManager {
    pub fn new(data_dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        let internal_socket = tako_workflows::internal_socket_path(&data_dir);
        Self {
            apps: DashMap::new(),
            spawner: Arc::new(
                Spawner::new()
                    .with_data_dir(data_dir.clone())
                    .with_internal_socket(internal_socket),
            ),
            event_tx: tx,
            event_rx: RwLock::new(Some(rx)),
            data_dir,
        }
    }

    /// Take the event receiver (can only be called once)
    pub fn take_event_receiver(&self) -> Option<mpsc::Receiver<InstanceEvent>> {
        self.event_rx.write().take()
    }

    /// Register a new app
    pub fn register_app(&self, config: AppConfig) -> Arc<App> {
        let name = config.deployment_id();
        let log_dir = self.data_dir.join("apps").join(&name).join("logs");
        let log_handle = spawn_app_logger(&name, log_dir);
        register_app_logger(&name, log_handle.clone());
        let app = Arc::new(App::new(config, self.event_tx.clone(), log_handle));
        self.apps.insert(name, app.clone());
        app
    }

    /// Get an app by name
    pub fn get_app(&self, name: &str) -> Option<Arc<App>> {
        self.apps.get(name).map(|entry| entry.value().clone())
    }

    pub fn data_dir(&self) -> &PathBuf {
        &self.data_dir
    }

    /// Remove an app
    pub fn remove_app(&self, name: &str) -> Option<Arc<App>> {
        let removed = self.apps.remove(name).map(|(_, v)| v);
        if removed.is_some() {
            unregister_app_logger(name);
        }
        removed
    }

    /// List all app names
    pub fn list_apps(&self) -> Vec<String> {
        self.apps.iter().map(|entry| entry.key().clone()).collect()
    }

    /// Start an app (spawn minimum instances)
    pub async fn start_app(&self, name: &str) -> Result<(), InstanceError> {
        let app = self
            .get_app(name)
            .ok_or_else(|| InstanceError::AppNotFound(name.to_string()))?;

        let min_instances = app.config.read().min_instances;
        app.set_state(AppState::Running);

        for _ in 0..min_instances {
            let instance = app.allocate_instance();
            self.spawner.spawn(&app, instance).await?;
        }

        Ok(())
    }

    /// Stop an app (kill all instances)
    pub async fn stop_app(&self, name: &str) -> Result<(), InstanceError> {
        let app = self
            .get_app(name)
            .ok_or_else(|| InstanceError::AppNotFound(name.to_string()))?;

        app.set_state(AppState::Stopped);

        // Kill all instances
        let instances = app.get_instances();
        for instance in instances {
            app.set_instance_state(&instance, InstanceState::Draining);
            instance.kill().await.map_err(InstanceError::StopError)?;
            app.remove_instance(&instance.id);
        }

        Ok(())
    }

    /// Stop every app instance owned by this server process.
    pub async fn shutdown_all(&self) {
        let apps: Vec<(String, Arc<App>)> = self
            .apps
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();

        for (name, app) in apps {
            app.set_state(AppState::Stopped);
            for instance in app.get_instances() {
                app.set_instance_state(&instance, InstanceState::Draining);
                if let Err(error) = instance.kill().await {
                    tracing::warn!(
                        app = %name,
                        instance = %instance.id,
                        "Failed to stop instance during server shutdown: {error}"
                    );
                }
                app.remove_instance(&instance.id);
            }
        }
    }

    /// Get spawner for external use
    pub fn spawner(&self) -> Arc<Spawner> {
        self.spawner.clone()
    }
}

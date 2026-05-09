use crate::instances::{RollingUpdateConfig, RollingUpdater, target_new_instances_for_build};
use crate::release::{release_app_path, resolve_release_runtime_bin};
use crate::socket::{AppState, Response};
use std::collections::HashMap;

impl crate::ServerState {
    pub(crate) async fn update_secrets(
        &self,
        app_name: &str,
        new_secrets: HashMap<String, String>,
    ) -> Response {
        tracing::info!(app = app_name, "Updating secrets");

        if let Err(e) = self.state_store.set_secrets(app_name, &new_secrets) {
            return Response::error(format!("Failed to store secrets: {}", e));
        }

        if let Some(app) = self.app_manager.get_app(app_name) {
            let mut config = app.config.read().clone();
            config.secrets = new_secrets;
            app.update_config(config.clone());
            self.persist_app_state(app_name).await;

            let release_path = release_app_path(&self.runtime.data_dir, &config);
            let runtime_bin_path =
                resolve_release_runtime_bin(&release_path, &self.runtime.data_dir)
                    .await
                    .ok()
                    .flatten();
            self.sync_app_workflows(app_name, &release_path, runtime_bin_path.as_deref())
                .await;

            if !app.get_instances().is_empty() {
                let previous_state = app.state();
                app.set_state(AppState::Deploying);
                let rolling_config = RollingUpdateConfig::default();
                let updater = RollingUpdater::new(self.app_manager.spawner(), rolling_config);
                let target =
                    target_new_instances_for_build(config.min_instances, app.get_instances().len());
                match updater.update(&app, config, target).await {
                    Ok(result) if result.success => {
                        app.set_state(AppState::Running);
                        return Response::ok(serde_json::json!({
                            "status": "updated",
                            "app": app_name,
                            "restarted": true
                        }));
                    }
                    Ok(result) => {
                        app.set_state(previous_state);
                        return Response::error(format!(
                            "Rolling restart failed: {:?}",
                            result.error
                        ));
                    }
                    Err(e) => {
                        app.set_state(AppState::Error);
                        return Response::error(format!("Rolling restart failed: {}", e));
                    }
                }
            }
        }

        Response::ok(serde_json::json!({
            "status": "updated",
            "app": app_name,
            "restarted": false
        }))
    }
}

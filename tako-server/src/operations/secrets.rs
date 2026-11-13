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

        let lock = self.get_deploy_lock(app_name).await;
        let _guard = match self.lock_deploy_for_operator(app_name, &lock).await {
            Ok(guard) => guard,
            Err(message) => return Response::error(message),
        };
        if let Some(response) = self.refuse_unresolved_restore(app_name) {
            return response;
        }

        if let Some(app) = self.app_manager.get_app(app_name) {
            let mut config = app.config.read().clone();
            if config.compose.is_some() && !app.get_instances().is_empty() {
                config.secrets = new_secrets;
                let credentials = (|| -> Result<_, crate::state_store::StateStoreError> {
                    Ok((
                        self.state_store.get_runtime_credentials(app_name)?,
                        self.state_store.get_ssl(app_name)?,
                        self.state_store.get_backup(app_name)?,
                    ))
                })();
                let (runtime_credentials, ssl, backup) = match credentials {
                    Ok(bindings) => bindings,
                    Err(error) => {
                        return Response::error(format!(
                            "Could not load deployment credentials: {error}"
                        ));
                    }
                };
                let routes = self.routes.read().routes_for_app(app_name);
                let response = self
                    .deploy_compose(super::compose::ComposeDeployment {
                        config,
                        routes,
                        runtime_credentials,
                        ssl,
                        backup,
                    })
                    .await;
                return match response {
                    Response::Ok { mut data } => {
                        data["status"] = serde_json::json!("updated");
                        data["restarted"] = serde_json::json!(
                            data["updates"].as_array().is_some_and(|v| !v.is_empty())
                        );
                        Response::Ok { data }
                    }
                    error => error,
                };
            }
        }

        if let Err(e) = self.state_store.set_secrets(app_name, &new_secrets) {
            return Response::error(format!("Failed to store secrets: {}", e));
        }

        if let Some(app) = self.app_manager.get_app(app_name) {
            let previous_config = app.config.read().clone();
            let mut config = previous_config.clone();
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
                        app.reset_recovery();
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

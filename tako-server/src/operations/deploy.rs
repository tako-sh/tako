use crate::app_command::env_vars_from_release_dir;
use crate::instances::{
    App, AppConfig, RollingUpdateConfig, RollingUpdater, target_new_instances_for_build,
};
use crate::release::{
    app_release_root, apply_release_runtime_to_config, ensure_app_runtime_data_dirs,
    inject_app_data_dir_env, requested_deployment_identity, resolve_release_runtime_bin,
    validate_app_name, validate_deploy_routes, validate_release_path_for_app,
    validate_release_version,
};
use crate::socket::{AppState, Response};
use std::collections::HashMap;
use std::sync::Arc;

impl crate::ServerState {
    pub(crate) async fn deploy_app(
        &self,
        app_name: &str,
        version: &str,
        path: &str,
        routes: Vec<String>,
        secrets: Option<HashMap<String, String>>,
    ) -> Response {
        tracing::info!(app = app_name, version = version, "Deploying app");

        if let Err(msg) = validate_app_name(app_name) {
            return Response::error(msg);
        }
        if let Err(msg) = validate_release_version(version) {
            return Response::error(msg);
        }
        if let Err(msg) = validate_deploy_routes(&routes) {
            return Response::error(msg);
        }
        let release_path =
            match validate_release_path_for_app(&self.runtime.data_dir, app_name, path) {
                Ok(value) => value,
                Err(msg) => return Response::error(msg),
            };

        let lock = self.get_deploy_lock(app_name).await;
        let _guard = match lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                tracing::warn!(
                    app = app_name,
                    "Deploy rejected: another deploy in progress"
                );
                return Response::error(format!(
                    "Deploy already in progress for app '{}'. Please wait and try again.",
                    app_name
                ));
            }
        };

        let env_vars = match env_vars_from_release_dir(&release_path) {
            Ok(vars) => vars,
            Err(error) => return Response::error(format!("Invalid app release: {}", error)),
        };
        let data_paths = match ensure_app_runtime_data_dirs(&self.runtime.data_dir, app_name) {
            Ok(paths) => paths,
            Err(error) => {
                return Response::error(format!("Failed to create app data dirs: {error}"));
            }
        };

        let secrets = if let Some(new_secrets) = secrets {
            if let Err(e) = self.state_store.set_secrets(app_name, &new_secrets) {
                return Response::error(format!("Failed to store secrets: {}", e));
            }
            new_secrets
        } else {
            self.state_store.get_secrets(app_name).unwrap_or_default()
        };
        let mut release_env = env_vars.clone();
        inject_app_data_dir_env(&mut release_env, &data_paths);
        release_env.extend(secrets.clone());

        let runtime_bin_path =
            match resolve_release_runtime_bin(&release_path, &self.runtime.data_dir).await {
                Ok(bin) => bin,
                Err(error) => return Response::error(format!("Invalid app release: {}", error)),
            };

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&release_path, std::fs::Permissions::from_mode(0o750));
        }

        let existing_app = self.app_manager.get_app(app_name);
        let rollback_snapshot = if let Some(existing) = existing_app.as_ref() {
            let previous_config = existing.config.read().clone();
            let previous_routes = {
                let route_table = self.routes.read().await;
                route_table.routes_for_app(app_name)
            };
            let previous_state = existing.state();
            Some((previous_config, previous_routes, previous_state))
        } else {
            None
        };

        let (app, deploy_config, is_new_app) = if let Some(existing) = existing_app {
            let mut config = existing.config.read().clone();
            config.version = version.to_string();
            config.secrets = secrets;
            if let Err(error) = apply_release_runtime_to_config(
                &mut config,
                release_path.clone(),
                runtime_bin_path.as_deref(),
            ) {
                return Response::error(format!("Invalid app release: {}", error));
            }
            inject_app_data_dir_env(&mut config.env_vars, &data_paths);
            existing.update_config(config.clone());
            (existing, config, false)
        } else {
            let (name, environment) = requested_deployment_identity(app_name);
            let config = AppConfig {
                name,
                environment,
                version: version.to_string(),
                secrets,
                min_instances: 1,
                max_instances: 4,
                ..Default::default()
            };
            let mut config = config;
            if let Err(error) = apply_release_runtime_to_config(
                &mut config,
                release_path.clone(),
                runtime_bin_path.as_deref(),
            ) {
                return Response::error(format!("Invalid app release: {}", error));
            }
            inject_app_data_dir_env(&mut config.env_vars, &data_paths);

            let deploy_config = config.clone();
            let app = self.app_manager.register_app(config);
            self.load_balancer.register_app(app.clone());
            (app, deploy_config, true)
        };

        {
            let mut route_table = self.routes.write().await;
            route_table.set_app_routes(app_name.to_string(), routes.clone());
        }

        app.clear_last_error();

        for route in &routes {
            let domain = route.split('/').next().unwrap_or(route);
            self.ensure_route_certificate(app_name, domain).await;
        }

        // Reconcile the workflow engine against the active release. Scale to
        // zero by default — no worker process spawns until the first enqueue
        // or cron tick.
        self.sync_app_workflows(app_name, &release_path, runtime_bin_path.as_deref())
            .await;

        if app.get_instances().is_empty() {
            if deploy_config.min_instances == 0 {
                match self.start_on_demand_warm_instance(&app).await {
                    Ok(()) => {
                        app.set_state(AppState::Running);
                        self.cold_start.reset(app_name);
                        self.persist_app_state(app_name).await;
                        Response::ok(serde_json::json!({
                            "status": "deployed",
                            "app": app_name,
                            "version": version,
                            "new_app": is_new_app,
                            "on_demand": true,
                            "startup_validated": true,
                            "warm_instance": true
                        }))
                    }
                    Err(e) => {
                        let error = e.to_string();
                        if let Some((previous_config, previous_routes, previous_state)) =
                            rollback_snapshot
                        {
                            self.restore_failed_rollout_snapshot(
                                app_name,
                                &app,
                                previous_config,
                                previous_routes,
                                previous_state,
                                error.clone(),
                            )
                            .await;
                        } else {
                            app.set_state(AppState::Error);
                        }
                        Response::error(format!("Deploy failed: {}", e))
                    }
                }
            } else {
                match self.app_manager.start_app(app_name).await {
                    Ok(()) => {
                        app.set_state(AppState::Running);
                        self.persist_app_state(app_name).await;
                        Response::ok(serde_json::json!({
                            "status": "deployed",
                            "app": app_name,
                            "version": version,
                            "new_app": is_new_app,
                            "on_demand": false
                        }))
                    }
                    Err(e) => {
                        let error = e.to_string();
                        if let Some((previous_config, previous_routes, previous_state)) =
                            rollback_snapshot
                        {
                            self.restore_failed_rollout_snapshot(
                                app_name,
                                &app,
                                previous_config,
                                previous_routes,
                                previous_state,
                                error.clone(),
                            )
                            .await;
                        } else {
                            app.set_state(AppState::Error);
                        }
                        Response::error(format!("Deploy failed: {}", e))
                    }
                }
            }
        } else {
            let previous_state = app.state();
            app.set_state(AppState::Deploying);

            let rolling_config = RollingUpdateConfig::default();
            let updater = RollingUpdater::new(self.app_manager.spawner().clone(), rolling_config);
            let target_new_instances = target_new_instances_for_build(
                deploy_config.min_instances,
                app.get_instances().len(),
            );

            match updater
                .update(&app, deploy_config.clone(), target_new_instances)
                .await
            {
                Ok(result) => {
                    if result.success {
                        if deploy_config.min_instances == 0 {
                            app.set_state(AppState::Running);
                            self.cold_start.reset(app_name);
                            self.persist_app_state(app_name).await;
                            Response::ok(serde_json::json!({
                                "status": "deployed",
                                "app": app_name,
                                "version": version,
                                "new_instances": result.new_instances,
                                "old_instances": result.old_instances,
                                "rolled_back": false,
                                "on_demand": true,
                                "startup_validated": true,
                                "warm_instance": true
                            }))
                        } else {
                            app.set_state(AppState::Running);
                            self.persist_app_state(app_name).await;
                            Response::ok(serde_json::json!({
                                "status": "deployed",
                                "app": app_name,
                                "version": version,
                                "new_instances": result.new_instances,
                                "old_instances": result.old_instances,
                                "rolled_back": false
                            }))
                        }
                    } else {
                        if let Some((previous_config, previous_routes, previous_state)) =
                            rollback_snapshot
                        {
                            self.restore_failed_rollout_snapshot(
                                app_name,
                                &app,
                                previous_config,
                                previous_routes,
                                previous_state,
                                result
                                    .error
                                    .clone()
                                    .unwrap_or_else(|| "Rolling update failed".to_string()),
                            )
                            .await;
                        } else {
                            app.set_state(previous_state);
                        }
                        Response::error(
                            serde_json::json!({
                                "status": "rollback",
                                "app": app_name,
                                "error": result.error,
                                "rolled_back": true
                            })
                            .to_string(),
                        )
                    }
                }
                Err(e) => {
                    if let Some((previous_config, previous_routes, previous_state)) =
                        rollback_snapshot
                    {
                        self.restore_failed_rollout_snapshot(
                            app_name,
                            &app,
                            previous_config,
                            previous_routes,
                            previous_state,
                            e.to_string(),
                        )
                        .await;
                    } else {
                        app.set_state(AppState::Error);
                    }
                    Response::error(format!("Rolling update failed: {}", e))
                }
            }
        }
    }

    pub(crate) async fn start_on_demand_warm_instance(&self, app: &Arc<App>) -> Result<(), String> {
        let instance = app.allocate_instance();
        let spawner = self.app_manager.spawner();

        match spawner.spawn(app, instance.clone()).await {
            Ok(()) => Ok(()),
            Err(e) => {
                app.remove_instance(&instance.id);
                Err(format!("Warm instance startup failed: {}", e))
            }
        }
    }

    async fn restore_failed_rollout_snapshot(
        &self,
        app_name: &str,
        app: &Arc<App>,
        previous_config: AppConfig,
        previous_routes: Vec<String>,
        previous_state: AppState,
        error: String,
    ) {
        let previous_release_path =
            app_release_root(&self.runtime.data_dir, app_name, &previous_config.version);
        let previous_secrets = previous_config.secrets.clone();
        app.update_config(previous_config);
        app.set_state(previous_state);
        app.set_last_error(format!("Rolling update failed: {error}"));
        if let Err(e) = self.state_store.set_secrets(app_name, &previous_secrets) {
            tracing::warn!(app = app_name, "Failed to restore previous secrets: {}", e);
        }
        {
            let mut route_table = self.routes.write().await;
            route_table.set_app_routes(app_name.to_string(), previous_routes);
        }
        self.persist_app_state(app_name).await;
        let runtime_bin_path =
            resolve_release_runtime_bin(&previous_release_path, &self.runtime.data_dir)
                .await
                .ok()
                .flatten();
        self.sync_app_workflows(
            app_name,
            &previous_release_path,
            runtime_bin_path.as_deref(),
        )
        .await;
    }
}

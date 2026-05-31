use crate::app_command::env_vars_from_release_dir;
use crate::instances::{
    App, AppConfig, RollingUpdateConfig, RollingUpdater, default_max_instances_for_host,
    target_new_instances_for_build, validate_requested_instances,
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
        source_ip: tako_core::SourceIpMode,
        secrets: Option<HashMap<String, String>>,
        storages: Option<HashMap<String, tako_core::StorageBinding>>,
        ssl: tako_core::SslBinding,
        backup: Option<tako_core::BackupBinding>,
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
        let ssl = match self
            .resolve_deploy_ssl_binding(app_name, &release_path, &routes, ssl)
            .await
        {
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
            new_secrets
        } else {
            self.state_store.get_secrets(app_name).unwrap_or_default()
        };
        let storages = if let Some(new_storages) = storages {
            new_storages
        } else {
            self.state_store.get_storages(app_name).unwrap_or_default()
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
        let previous_ssl = if existing_app.is_some() {
            match self.state_store.get_ssl(app_name) {
                Ok(value) => value,
                Err(error) => {
                    return Response::error(format!("Failed to read SSL credentials: {error}"));
                }
            }
        } else {
            None
        };
        let previous_backup = if existing_app.is_some() {
            match self.state_store.get_backup(app_name) {
                Ok(value) => value,
                Err(error) => {
                    return Response::error(format!("Failed to read backup config: {error}"));
                }
            }
        } else {
            None
        };
        let rollback_snapshot = if let Some(existing) = existing_app.as_ref() {
            let previous_config = existing.config.read().clone();
            let previous_routes = {
                let route_table = self.routes.read().await;
                route_table.routes_for_app(app_name)
            };
            let previous_state = existing.state();
            Some((
                previous_config,
                previous_routes,
                previous_state,
                previous_ssl,
                previous_backup,
            ))
        } else {
            None
        };

        let (app, deploy_config, is_new_app) = if let Some(existing) = existing_app {
            let mut config = existing.config.read().clone();
            config.version = version.to_string();
            config.source_ip = source_ip;
            if let Err(error) = apply_release_runtime_to_config(
                &mut config,
                release_path.clone(),
                runtime_bin_path.as_deref(),
            ) {
                return Response::error(format!("Invalid app release: {}", error));
            }
            if let Err(error) =
                validate_requested_instances(config.min_instances, config.max_instances)
            {
                return Response::error(format!("Deploy failed: {error}"));
            }
            inject_app_data_dir_env(&mut config.env_vars, &data_paths);
            if let Err(e) = self.persist_credentials(app_name, &secrets, &storages) {
                return Response::error(e);
            }
            if let Err(e) = self.persist_ssl_binding(app_name, &ssl) {
                return Response::error(e);
            }
            if let Err(e) = self.persist_backup_binding(app_name, backup.as_ref()) {
                return Response::error(e);
            }
            config.secrets = secrets;
            config.storages = storages;
            existing.update_config(config.clone());
            (existing, config, false)
        } else {
            let (name, environment) = requested_deployment_identity(app_name);
            let config = AppConfig {
                name,
                environment,
                version: version.to_string(),
                source_ip,
                min_instances: 1,
                max_instances: default_max_instances_for_host(),
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
            if let Err(error) =
                validate_requested_instances(config.min_instances, config.max_instances)
            {
                return Response::error(format!("Deploy failed: {error}"));
            }
            inject_app_data_dir_env(&mut config.env_vars, &data_paths);
            if let Err(e) = self.persist_credentials(app_name, &secrets, &storages) {
                return Response::error(e);
            }
            if let Err(e) = self.persist_ssl_binding(app_name, &ssl) {
                return Response::error(e);
            }
            if let Err(e) = self.persist_backup_binding(app_name, backup.as_ref()) {
                return Response::error(e);
            }
            config.secrets = secrets;
            config.storages = storages;

            let deploy_config = config.clone();
            let app = self.app_manager.register_app(config);
            self.load_balancer.register_app(app.clone());
            (app, deploy_config, true)
        };

        {
            let mut route_table = self.routes.write().await;
            route_table.set_app_routes_with_source_ip(
                app_name.to_string(),
                routes.clone(),
                source_ip,
            );
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
                        if let Some((
                            previous_config,
                            previous_routes,
                            previous_state,
                            previous_ssl,
                            previous_backup,
                        )) = rollback_snapshot
                        {
                            self.restore_failed_rollout_snapshot(
                                app_name,
                                &app,
                                previous_config,
                                previous_routes,
                                previous_state,
                                previous_ssl,
                                previous_backup,
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
                        if let Some((
                            previous_config,
                            previous_routes,
                            previous_state,
                            previous_ssl,
                            previous_backup,
                        )) = rollback_snapshot
                        {
                            self.restore_failed_rollout_snapshot(
                                app_name,
                                &app,
                                previous_config,
                                previous_routes,
                                previous_state,
                                previous_ssl,
                                previous_backup,
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
            let updater = RollingUpdater::new(self.app_manager.spawner(), rolling_config);
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
                        if let Some((
                            previous_config,
                            previous_routes,
                            previous_state,
                            previous_ssl,
                            previous_backup,
                        )) = rollback_snapshot
                        {
                            self.restore_failed_rollout_snapshot(
                                app_name,
                                &app,
                                previous_config,
                                previous_routes,
                                previous_state,
                                previous_ssl,
                                previous_backup,
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
                    if let Some((
                        previous_config,
                        previous_routes,
                        previous_state,
                        previous_ssl,
                        previous_backup,
                    )) = rollback_snapshot
                    {
                        self.restore_failed_rollout_snapshot(
                            app_name,
                            &app,
                            previous_config,
                            previous_routes,
                            previous_state,
                            previous_ssl,
                            previous_backup,
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

    fn persist_ssl_binding(
        &self,
        app_name: &str,
        ssl: &tako_core::SslBinding,
    ) -> Result<(), String> {
        if ssl.cloudflare_api_token.is_some() {
            self.state_store
                .set_ssl(app_name, ssl)
                .map_err(|e| format!("Failed to store SSL credentials: {e}"))
        } else {
            self.state_store
                .delete_ssl(app_name)
                .map_err(|e| format!("Failed to clear SSL credentials: {e}"))
        }
    }

    fn persist_credentials(
        &self,
        app_name: &str,
        secrets: &HashMap<String, String>,
        storages: &HashMap<String, tako_core::StorageBinding>,
    ) -> Result<(), String> {
        self.state_store
            .set_secrets(app_name, secrets)
            .map_err(|e| format!("Failed to store secrets: {e}"))?;
        self.state_store
            .set_storages(app_name, storages)
            .map_err(|e| format!("Failed to store storages: {e}"))?;
        Ok(())
    }

    fn persist_backup_binding(
        &self,
        app_name: &str,
        backup: Option<&tako_core::BackupBinding>,
    ) -> Result<(), String> {
        self.state_store
            .set_backup(app_name, backup)
            .map_err(|e| format!("Failed to store backup config: {e}"))
    }

    async fn restore_failed_rollout_snapshot(
        &self,
        app_name: &str,
        app: &Arc<App>,
        previous_config: AppConfig,
        previous_routes: Vec<String>,
        previous_state: AppState,
        previous_ssl: Option<tako_core::SslBinding>,
        previous_backup: Option<tako_core::BackupBinding>,
        error: String,
    ) {
        let previous_release_path =
            app_release_root(&self.runtime.data_dir, app_name, &previous_config.version);
        let previous_secrets = previous_config.secrets.clone();
        let previous_storages = previous_config.storages.clone();
        let previous_source_ip = previous_config.source_ip;
        app.update_config(previous_config);
        app.set_state(previous_state);
        app.set_last_error(format!("Rolling update failed: {error}"));
        if let Err(e) = self.state_store.set_secrets(app_name, &previous_secrets) {
            tracing::warn!(app = app_name, "Failed to restore previous secrets: {}", e);
        }
        if let Err(e) = self.state_store.set_storages(app_name, &previous_storages) {
            tracing::warn!(app = app_name, "Failed to restore previous storages: {}", e);
        }
        match previous_ssl {
            Some(ssl) => {
                if let Err(e) = self.state_store.set_ssl(app_name, &ssl) {
                    tracing::warn!(app = app_name, "Failed to restore previous SSL: {}", e);
                }
            }
            None => {
                if let Err(e) = self.state_store.delete_ssl(app_name) {
                    tracing::warn!(app = app_name, "Failed to clear restored SSL: {}", e);
                }
            }
        }
        if let Err(e) = self
            .state_store
            .set_backup(app_name, previous_backup.as_ref())
        {
            tracing::warn!(
                app = app_name,
                "Failed to restore previous backup config: {}",
                e
            );
        }
        {
            let mut route_table = self.routes.write().await;
            route_table.set_app_routes_with_source_ip(
                app_name.to_string(),
                previous_routes,
                previous_source_ip,
            );
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

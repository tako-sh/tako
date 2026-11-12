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

pub(super) enum BackupUpdate {
    Preserve,
    Replace(Option<Box<tako_core::BackupBinding>>),
}

pub(crate) struct DeployRequest<'a> {
    pub(crate) app_name: &'a str,
    pub(crate) version: &'a str,
    pub(crate) path: &'a str,
    pub(crate) routes: Vec<String>,
    pub(crate) source_ip: tako_core::SourceIpMode,
    pub(crate) secrets: Option<HashMap<String, String>>,
    pub(crate) runtime_credentials: Option<HashMap<String, String>>,
    pub(crate) storages: Option<HashMap<String, tako_core::StorageBinding>>,
    pub(crate) ssl: tako_core::SslBinding,
    pub(super) backup: BackupUpdate,
    pub(crate) force: bool,
}

struct RollbackSnapshot {
    config: AppConfig,
    routes: Vec<String>,
    state: AppState,
    ssl: Option<tako_core::SslBinding>,
    backup: Option<tako_core::BackupBinding>,
    runtime_credentials: HashMap<String, String>,
}

impl crate::ServerState {
    pub(crate) async fn deploy_app(&self, request: DeployRequest<'_>) -> Response {
        let DeployRequest {
            app_name,
            version,
            path,
            routes,
            source_ip,
            secrets,
            runtime_credentials,
            storages,
            ssl,
            backup,
            force,
        } = request;

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
        if let Err(error) =
            crate::protocol_compatibility::validate_release_protocol(&release_path, force)
        {
            return Response::error(error);
        }
        let ssl = match self
            .resolve_deploy_ssl_binding(app_name, &release_path, &routes, ssl)
            .await
        {
            Ok(value) => value,
            Err(msg) => return Response::error(msg),
        };

        let lock = self.get_deploy_lock(app_name).await;
        let _guard = match self.lock_deploy_for_operator(app_name, &lock).await {
            Ok(guard) => guard,
            Err(message) => {
                tracing::warn!(app = app_name, %message, "Deploy rejected");
                return Response::error(message);
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
        if let Err(error) = crate::isolation::prepare_app_filesystem_isolation(
            &self.runtime.data_dir,
            app_name,
            Some(&release_path),
            &data_paths,
        ) {
            return Response::error(format!("Failed to prepare app isolation: {error}"));
        }

        let secrets = if let Some(new_secrets) = secrets {
            new_secrets
        } else {
            match self.state_store.get_secrets(app_name) {
                Ok(secrets) => secrets,
                Err(error) => return Response::error(format!("Failed to read secrets: {error}")),
            }
        };
        let runtime_credentials = if let Some(new_runtime_credentials) = runtime_credentials {
            new_runtime_credentials
        } else {
            match self.state_store.get_runtime_credentials(app_name) {
                Ok(credentials) => credentials,
                Err(error) => {
                    return Response::error(format!("Failed to read runtime credentials: {error}"));
                }
            }
        };
        let storages = if let Some(new_storages) = storages {
            new_storages
        } else {
            match self.state_store.get_storages(app_name) {
                Ok(storages) => storages,
                Err(error) => return Response::error(format!("Failed to read storages: {error}")),
            }
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
        let backup = match backup {
            BackupUpdate::Preserve => previous_backup.clone(),
            BackupUpdate::Replace(binding) => binding.map(|binding| *binding),
        };
        let rollback_snapshot = if let Some(existing) = existing_app.as_ref() {
            let previous_config = existing.config.read().clone();
            let previous_routes = {
                let route_table = self.routes.read();
                route_table.routes_for_app(app_name)
            };
            let previous_state = existing.state();
            Some(RollbackSnapshot {
                config: previous_config,
                routes: previous_routes,
                state: previous_state,
                ssl: previous_ssl,
                backup: previous_backup,
                runtime_credentials: self
                    .state_store
                    .get_runtime_credentials(app_name)
                    .unwrap_or_default(),
            })
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
            if config.compose.is_none()
                && let Err(error) =
                    validate_requested_instances(config.min_instances, config.max_instances)
            {
                return Response::error(format!("Deploy failed: {error}"));
            }
            inject_app_data_dir_env(&mut config.env_vars, &data_paths);
            if config.compose.is_some() {
                config.secrets = secrets;
                config.storages = storages;
                return self
                    .deploy_compose(super::compose::ComposeDeployment {
                        config,
                        routes,
                        runtime_credentials,
                        ssl: ssl.cloudflare_api_token.is_some().then_some(ssl),
                        backup,
                    })
                    .await;
            }
            if let Err(e) =
                self.persist_credentials(app_name, &secrets, &runtime_credentials, &storages)
            {
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
            // Preserve ownership of the entire previous Compose project until
            // it has stopped, even if its HTTP instance has already disappeared.
            // After update_config the manager would only see the native runtime.
            if rollback_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.config.compose.is_some())
            {
                if let Err(error) = self.app_manager.stop_app(app_name).await {
                    if let Some(snapshot) = rollback_snapshot {
                        self.restore_failed_rollout_snapshot(
                            app_name,
                            &existing,
                            snapshot,
                            error.to_string(),
                        )
                        .await;
                    }
                    return Response::error(format!("Failed to stop the previous stack: {error}"));
                }
            }
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
            if config.compose.is_none()
                && let Err(error) =
                    validate_requested_instances(config.min_instances, config.max_instances)
            {
                return Response::error(format!("Deploy failed: {error}"));
            }
            inject_app_data_dir_env(&mut config.env_vars, &data_paths);
            if config.compose.is_some() {
                config.secrets = secrets;
                config.storages = storages;
                return self
                    .deploy_compose(super::compose::ComposeDeployment {
                        config,
                        routes,
                        runtime_credentials,
                        ssl: ssl.cloudflare_api_token.is_some().then_some(ssl),
                        backup,
                    })
                    .await;
            }
            if let Err(e) =
                self.persist_credentials(app_name, &secrets, &runtime_credentials, &storages)
            {
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
            let mut route_table = self.routes.write();
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

        let response = if app.get_instances().is_empty() {
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
                        if let Some(snapshot) = rollback_snapshot {
                            self.restore_failed_rollout_snapshot(
                                app_name,
                                &app,
                                snapshot,
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
                        if let Some(snapshot) = rollback_snapshot {
                            self.restore_failed_rollout_snapshot(
                                app_name,
                                &app,
                                snapshot,
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
                        if let Some(snapshot) = rollback_snapshot {
                            self.restore_failed_rollout_snapshot(
                                app_name,
                                &app,
                                snapshot,
                                result
                                    .error
                                    .clone()
                                    .unwrap_or_else(|| "Rolling update failed".to_string()),
                            )
                            .await;
                        } else {
                            app.set_state(previous_state);
                        }
                        Response::error(format!(
                            "Deploy failed; rolled back to previous release: {}",
                            result
                                .error
                                .unwrap_or_else(|| "Rolling update failed".to_string())
                        ))
                    }
                }
                Err(e) => {
                    if let Some(snapshot) = rollback_snapshot {
                        self.restore_failed_rollout_snapshot(
                            app_name,
                            &app,
                            snapshot,
                            e.to_string(),
                        )
                        .await;
                    } else {
                        app.set_state(AppState::Error);
                    }
                    Response::error(format!("Rolling update failed: {}", e))
                }
            }
        };
        if response.is_ok() {
            // A release that passed rollout starts with a clean crash history;
            // a stale backoff must not delay recovery of the new instances.
            app.reset_recovery();
        }
        response
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
        runtime_credentials: &HashMap<String, String>,
        storages: &HashMap<String, tako_core::StorageBinding>,
    ) -> Result<(), String> {
        self.state_store
            .set_secrets(app_name, secrets)
            .map_err(|e| format!("Failed to store secrets: {e}"))?;
        self.state_store
            .set_runtime_credentials(app_name, runtime_credentials)
            .map_err(|e| format!("Failed to store runtime credentials: {e}"))?;
        self.channel_credentials_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
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
        snapshot: RollbackSnapshot,
        error: String,
    ) {
        let recreate = snapshot.config.compose.is_some() || app.config.read().compose.is_some();
        let restart = recreate && matches!(snapshot.state, AppState::Running | AppState::Deploying);
        if recreate {
            if let Err(stop_error) = self.app_manager.stop_app(app_name).await {
                app.set_state(AppState::Error);
                app.set_last_error(format!(
                    "Deploy failed: {error}; could not stop the failed stack: {stop_error}"
                ));
                return;
            }
        }
        let previous_release_path =
            app_release_root(&self.runtime.data_dir, app_name, &snapshot.config.version);
        let previous_secrets = snapshot.config.secrets.clone();
        let previous_runtime_credentials = snapshot.runtime_credentials.clone();
        let previous_storages = snapshot.config.storages.clone();
        let previous_source_ip = snapshot.config.source_ip;
        app.update_config(snapshot.config);
        app.set_state(snapshot.state);
        app.set_last_error(format!("Rolling update failed: {error}"));
        if let Err(e) = self.state_store.set_secrets(app_name, &previous_secrets) {
            tracing::warn!(app = app_name, "Failed to restore previous secrets: {}", e);
        }
        if let Err(e) = self
            .state_store
            .set_runtime_credentials(app_name, &previous_runtime_credentials)
        {
            tracing::warn!(
                app = app_name,
                "Failed to restore previous runtime credentials: {}",
                e
            );
        }
        self.channel_credentials_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if let Err(e) = self.state_store.set_storages(app_name, &previous_storages) {
            tracing::warn!(app = app_name, "Failed to restore previous storages: {}", e);
        }
        match snapshot.ssl {
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
            .set_backup(app_name, snapshot.backup.as_ref())
        {
            tracing::warn!(
                app = app_name,
                "Failed to restore previous backup config: {}",
                e
            );
        }
        {
            let mut route_table = self.routes.write();
            route_table.set_app_routes_with_source_ip(
                app_name.to_string(),
                snapshot.routes,
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
        if restart {
            if let Err(restart_error) = self.app_manager.start_app(app_name).await {
                app.set_state(AppState::Error);
                app.set_last_error(format!(
                    "Deploy failed: {error}; previous release could not restart: {restart_error}"
                ));
            }
            self.persist_app_state(app_name).await;
        }
    }
}

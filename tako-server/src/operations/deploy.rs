use crate::instances::{
    AppConfig, RollingUpdateConfig, RollingUpdater, default_max_instances_for_host,
    target_new_instances_for_build, validate_requested_instances,
};
use crate::release::{
    apply_release_runtime_to_config, ensure_app_runtime_data_dirs, inject_app_data_dir_env,
    requested_deployment_identity, resolve_release_runtime_bin, validate_app_name,
    validate_deploy_routes, validate_release_path_for_app, validate_release_version,
};
use crate::socket::{AppState, Response};
use std::collections::HashMap;

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
    pub(crate) databases: Option<HashMap<String, tako_core::DatabaseBinding>>,
    pub(crate) ssl: tako_core::SslBinding,
    pub(super) backup: BackupUpdate,
    pub(crate) force: bool,
}

impl crate::ServerState {
    pub(crate) async fn deploy_app(&self, request: DeployRequest<'_>) -> Response {
        let lock = self.get_deploy_lock(request.app_name).await;
        let _guard = match self.lock_deploy_for_operator(request.app_name, &lock).await {
            Ok(guard) => guard,
            Err(message) => return Response::error(message),
        };
        self.deploy_app_locked(request).await
    }

    pub(super) async fn deploy_app_locked(&self, request: DeployRequest<'_>) -> Response {
        let DeployRequest {
            app_name,
            version,
            path,
            routes,
            source_ip,
            secrets,
            runtime_credentials,
            storages,
            databases,
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
        if !routes.is_empty()
            && let Err(msg) = validate_deploy_routes(&routes)
        {
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

        if let Some(response) = self.refuse_unresolved_restore(app_name) {
            return response;
        }

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
        let storages = if let Some(mut new_storages) = storages {
            match self.state_store.get_storages(app_name) {
                Ok(previous) => keep_local_signing_keys(&mut new_storages, &previous),
                Err(error) => return Response::error(format!("Failed to read storages: {error}")),
            }
            new_storages
        } else {
            match self.state_store.get_storages(app_name) {
                Ok(storages) => storages,
                Err(error) => return Response::error(format!("Failed to read storages: {error}")),
            }
        };
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

        let existing = self.app_manager.get_app(app_name);
        let backup = match backup {
            BackupUpdate::Preserve => match self.state_store.get_backup(app_name) {
                Ok(binding) => binding,
                Err(error) => {
                    return Response::error(format!("Failed to read backup config: {error}"));
                }
            },
            BackupUpdate::Replace(binding) => binding.map(|binding| *binding),
        };
        let mut config = existing.as_ref().map_or_else(
            || {
                let (name, environment) = requested_deployment_identity(app_name);
                AppConfig {
                    name,
                    environment,
                    min_instances: 1,
                    max_instances: default_max_instances_for_host(),
                    ..Default::default()
                }
            },
            |app| app.config.read().clone(),
        );
        config.version = version.to_string();
        config.source_ip = source_ip;
        if let Err(error) = apply_release_runtime_to_config(
            &mut config,
            release_path.clone(),
            runtime_bin_path.as_deref(),
        ) {
            return Response::error(format!("Invalid app release: {error}"));
        }
        if config.compose.is_none()
            && let Err(msg) = validate_deploy_routes(&routes)
        {
            return Response::error(msg);
        }
        inject_app_data_dir_env(&mut config.env_vars, &data_paths);
        config.secrets = secrets;
        config.storages = storages;
        // Without new bindings the existing ones stay, but they are checked
        // against this release again (a container release cannot use them).
        let previous_databases = config.databases.clone();
        let requested_databases = databases.unwrap_or_else(|| {
            previous_databases
                .iter()
                .map(|(name, binding)| (name.clone(), binding.binding.clone()))
                .collect()
        });
        // Held until this deploy ends, so a database it binds cannot be
        // deleted between being checked here and the release using it, nor
        // one the previous release binds while its instances drain.
        let _database_pins = match self.databases.pin(
            requested_databases
                .values()
                .map(|binding| binding.name.as_str())
                .chain(
                    previous_databases
                        .values()
                        .map(|binding| binding.binding.name.as_str()),
                ),
        ) {
            Ok(pins) => pins,
            Err(error) => return Response::error(error),
        };
        config.databases =
            match self
                .databases
                .prepare_bindings(&previous_databases, requested_databases, &config)
            {
                Ok(bindings) => bindings,
                Err(error) => return Response::error(error),
            };
        let ssl = ssl.cloudflare_api_token.is_some().then_some(ssl);
        if config.compose.is_some() {
            return self
                .deploy_compose(super::compose::ComposeDeployment {
                    config,
                    routes,
                    runtime_credentials,
                    ssl,
                    backup,
                })
                .await;
        }
        if let Err(error) = validate_requested_instances(config.min_instances, config.max_instances)
        {
            return Response::error(format!("Deploy failed: {error}"));
        }
        config.apply_internal_defaults();
        let database_connections = self.databases.connections(&config.databases);
        let kv_connections = crate::databases::kv_connections(&config.databases);
        let workflows = match self
            .prepare_app_workflows(
                app_name,
                &release_path,
                runtime_bin_path.as_deref(),
                crate::server_state::WorkflowBindings {
                    secrets: &config.secrets,
                    storages: &config.storages,
                    databases: &database_connections,
                    kv: &kv_connections,
                    postgres_url: runtime_credentials.get("postgres_url"),
                },
                tako_workflows::manager::ScheduleActivation::Deploy,
            )
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                return Response::error(format!("Deploy failed to prepare workflows: {error}"));
            }
        };

        if config.stopped {
            // A release may advance while paused, but must not start HTTP or
            // workflow processes. Start will use this newly committed release.
            // Stop with the previous configuration so a Compose -> native
            // transition also tears down untagged dependency containers.
            if existing.is_some()
                && let Err(error) = self.app_manager.stop_app(app_name).await
            {
                return Response::error(format!("Could not stop previous release: {error}"));
            }
            if let Err(error) = self.state_store.commit_deployment(
                &config,
                &routes,
                &runtime_credentials,
                ssl.as_ref(),
                backup.as_ref(),
                workflows
                    .as_ref()
                    .and_then(|runtime| runtime.schedule_revision()),
            ) {
                return Response::error(format!("Could not commit stopped deployment: {error}"));
            }
            self.databases
                .apply_bindings(app_name, &[&config.databases]);
            let app = existing.unwrap_or_else(|| self.app_manager.register_app(config.clone()));
            app.update_config(config.clone());
            self.load_balancer.register_app(app.clone());
            self.routes
                .write()
                .set_config_routes(&config, routes.clone());
            self.channel_credentials_generation
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            self.cold_start.mark_failed(app_name, "app_stopped");
            self.workflows
                .stop(app_name, std::time::Duration::from_secs(120))
                .await;
            app.set_state(AppState::Stopped);
            for route in &routes {
                self.ensure_route_certificate(app_name, route.split('/').next().unwrap_or(route))
                    .await;
            }
            return Response::ok(serde_json::json!({
                "status":"deployed", "app":app_name, "version":version, "stopped":true
            }));
        }

        // Prepare TLS with the incoming binding before publishing new hostnames.
        for route in &routes {
            self.ensure_route_certificate_with_ssl(
                app_name,
                route.split('/').next().unwrap_or(route),
                ssl.as_ref().unwrap_or(&tako_core::SslBinding::default()),
            )
            .await;
        }

        let first_deploy = existing.is_none();
        let app = existing.unwrap_or_else(|| self.app_manager.prepare_app(config.clone()));
        let previous_compose = app.config.read().compose.is_some();
        let previous_state = app.set_state(AppState::Deploying);
        if previous_compose {
            let _lifecycle = app.lifecycle_gate.write().await;
            if let Err(error) = self.app_manager.stop_app_locked(&app).await {
                app.set_state(AppState::Error);
                app.set_last_error(format!("Failed to stop the previous stack: {error}"));
                return Response::error(format!("Failed to stop the previous stack: {error}"));
            }
            app.set_state(AppState::Deploying);
        }
        // Old and new instances run side by side during the rollout.
        self.databases
            .apply_bindings(app_name, &[&previous_databases, &config.databases]);
        let old_count = app.get_instances().len();
        let target = target_new_instances_for_build(config.min_instances, old_count);
        let updater =
            RollingUpdater::new(self.app_manager.spawner(), RollingUpdateConfig::default());
        let rollout = updater
            .update(&app, &config, target, || {
                self.state_store
                    .commit_deployment(
                        &config,
                        &routes,
                        &runtime_credentials,
                        ssl.as_ref(),
                        backup.as_ref(),
                        workflows
                            .as_ref()
                            .and_then(|runtime| runtime.schedule_revision()),
                    )
                    .map_err(|error| format!("Could not commit deployment: {error}"))?;
                app.update_config(config.clone());
                if first_deploy {
                    self.app_manager.publish_app(&app);
                    self.load_balancer.register_app(app.clone());
                }
                self.routes.write().set_app_routes_with_source_ip(
                    app_name.to_string(),
                    routes,
                    source_ip,
                );
                self.channel_credentials_generation
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                self.install_app_workflows(app_name, workflows);
                app.clear_last_error();
                app.reset_recovery();
                if config.min_instances == 0 {
                    self.cold_start.mark_ready(app_name);
                }
                Ok(())
            })
            .await;
        let result = match rollout {
            Ok(result) if result.success => {
                self.databases
                    .apply_bindings(app_name, &[&config.databases]);
                result
            }
            failed => {
                self.databases
                    .apply_bindings(app_name, &[&previous_databases]);
                let error = match failed {
                    Ok(result) => result
                        .error
                        .unwrap_or_else(|| "Rolling update failed".into()),
                    Err(error) => error.to_string(),
                };
                let _lifecycle = app.lifecycle_gate.write().await;
                app.set_state(
                    if previous_state == AppState::Idle && app.healthy_instance_count() > 0 {
                        AppState::Running
                    } else {
                        previous_state
                    },
                );
                app.set_last_error(format!("Rolling update failed: {error}"));
                if previous_compose
                    && matches!(previous_state, AppState::Running | AppState::Deploying)
                    && let Err(restart_error) = self.app_manager.start_app_locked(&app).await
                {
                    app.set_state(AppState::Error);
                    let message = format!(
                        "Deploy failed: {error}; previous stack could not restart: {restart_error}"
                    );
                    app.set_last_error(message.clone());
                    return Response::error(message);
                }
                return if first_deploy {
                    Response::error(format!("Deploy failed: {error}"))
                } else {
                    Response::error(format!(
                        "Deploy failed; rolled back to previous release: {error}"
                    ))
                };
            }
        };

        app.set_state(AppState::Running);
        let mut response =
            serde_json::json!({ "status": "deployed", "app": app_name, "version": version });
        if old_count == 0 {
            response["new_app"] = first_deploy.into();
            response["on_demand"] = (config.min_instances == 0).into();
        } else {
            response["new_instances"] = result.new_instances.into();
            response["old_instances"] = result.old_instances.into();
            response["rolled_back"] = false.into();
        }
        if config.min_instances == 0 {
            response["on_demand"] = true.into();
            response["startup_validated"] = true.into();
            response["warm_instance"] = true.into();
        }
        Response::ok(response)
    }
}

/// Signed local-storage URLs must outlive a deploy, but every deploy brings a
/// fresh key. Keep the key a local binding already has while it serves the
/// same path; a new path starts with the new key.
fn keep_local_signing_keys(
    storages: &mut HashMap<String, tako_core::StorageBinding>,
    previous: &HashMap<String, tako_core::StorageBinding>,
) {
    for (name, binding) in storages.iter_mut() {
        let Some(old) = previous.get(name) else {
            continue;
        };
        if binding.provider == tako_core::StorageProvider::Local
            && old.provider == tako_core::StorageProvider::Local
            && binding.path == old.path
            && let Some(key) = old.signing_key.as_ref().filter(|key| !key.is_empty())
        {
            binding.signing_key = Some(key.clone());
        }
    }
}

#[cfg(test)]
mod signing_key_tests {
    use super::*;

    fn local(path: &str, key: &str) -> tako_core::StorageBinding {
        tako_core::StorageBinding {
            provider: tako_core::StorageProvider::Local,
            resource: None,
            bucket: None,
            endpoint: None,
            region: None,
            access_key_id: None,
            secret_access_key: None,
            force_path_style: false,
            public_base_url: None,
            path: Some(path.into()),
            signing_key: Some(key.into()),
        }
    }

    #[test]
    fn deploys_keep_a_local_bindings_signing_key_while_its_path_is_unchanged() {
        let previous = HashMap::from([
            ("files".to_string(), local("storage/files", "old")),
            ("moved".to_string(), local("storage/before", "old")),
        ]);
        let mut next = HashMap::from([
            ("files".to_string(), local("storage/files", "new")),
            ("moved".to_string(), local("storage/after", "new")),
            ("added".to_string(), local("storage/added", "new")),
        ]);
        keep_local_signing_keys(&mut next, &previous);
        let key = |name: &str| next[name].signing_key.as_deref().unwrap();
        assert_eq!(key("files"), "old", "issued URLs keep verifying");
        assert_eq!(key("moved"), "new");
        assert_eq!(key("added"), "new");
    }
}

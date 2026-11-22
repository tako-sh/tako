use super::*;

impl ServerState {
    pub async fn restore_from_state_store(&self) -> Result<(), StateStoreError> {
        if self.is_draining() {
            return Ok(());
        }
        self.restore_apps_from_state_store(false).await
    }

    pub(crate) async fn restore_apps_from_state_store(
        &self,
        strict: bool,
    ) -> Result<(), StateStoreError> {
        let mut errors = Vec::new();
        // Interrupted Compose deployments whose candidates could not be
        // removed. Their app is not started over containers left running.
        let mut pending_cleanup_errors = HashMap::new();
        let apps = self.state_store.load_apps()?;
        for (app, project) in self.state_store.pending_compose_deployments()? {
            self.restore_failures.write().remove(&app);
            let existing = apps
                .iter()
                .find(|persisted| persisted.config.deployment_id() == app);
            let was_compose = if let Some(persisted) = existing {
                let mut config = persisted.config.clone();
                let release_path = release_app_path(&self.runtime.data_dir, &config);
                if let Err(error) = apply_release_runtime_to_config(&mut config, release_path, None)
                {
                    // Missing/corrupt release metadata cannot establish who
                    // owns these containers. Leave them and the recovery marker
                    // intact so a running committed release is not destroyed.
                    tracing::error!(%app, %error, "Cannot determine interrupted Compose deployment ownership");
                    errors.push(format!("{app}: {error}"));
                    continue;
                }
                config.compose.is_some()
            } else {
                false
            };
            // Existing Compose apps reconcile the committed release below.
            // Interrupted first deployments (or native -> Compose transitions)
            // have no committed Compose owner and must leave no live candidates.
            if !was_compose {
                match tako_compose::cleanup_pending_project(&project).await {
                    Ok(()) => self.state_store.clear_compose_deployment(&app)?,
                    Err(error) => {
                        tracing::error!(%app, %error, "Could not clean interrupted Compose deployment");
                        let error =
                            format!("could not remove an interrupted Compose deployment: {error}");
                        if existing.is_none() {
                            // A first deployment: no app restores below to report it.
                            errors.push(format!("{app}: {error}"));
                            self.record_restore_failure(&app, "", true, &error);
                        } else {
                            pending_cleanup_errors.insert(app, error);
                        }
                    }
                }
            }
        }
        if apps.is_empty() {
            if strict && !errors.is_empty() {
                return Err(StateStoreError::InvalidData(errors.join("; ")));
            }
            return Ok(());
        }

        tracing::info!(apps = apps.len(), "Restoring apps from durable state");

        for persisted in apps {
            let mut config = persisted.config.clone();
            let app_name = config.deployment_id();
            let routes = persisted.routes.clone();
            let errors_before = errors.len();
            self.restore_failures.write().remove(&app_name);
            if let Some(error) = pending_cleanup_errors.remove(&app_name) {
                errors.push(format!("{app_name}: {error}"));
            }

            let release_path = release_app_path(&self.runtime.data_dir, &config);
            let runtime_bin_path =
                match resolve_release_runtime_bin(&release_path, &self.runtime.data_dir).await {
                    Ok(bin) => bin,
                    Err(error) => {
                        tracing::error!(app = %app_name, %error, "Failed to restore app runtime");
                        errors.push(format!("{app_name}: {error}"));
                        self.record_persisted_restore_failure(&persisted.config, &error);
                        continue;
                    }
                };
            if let Err(error) = apply_release_runtime_to_config(
                &mut config,
                release_path.clone(),
                runtime_bin_path.as_deref(),
            ) {
                tracing::error!(app = %app_name, "Failed to restore app config: {}", error);
                errors.push(format!("{app_name}: {error}"));
                self.record_persisted_restore_failure(&persisted.config, &error);
                continue;
            }
            if config.compose.is_none() {
                if self.runtime.standby && config.min_instances > 1 {
                    config.min_instances = 1;
                    config.max_instances = config.max_instances.max(1);
                }
                if let Some((requested_instances, max_instances)) =
                    clamp_instances_to_limit(&mut config)
                {
                    tracing::warn!(
                        app = %app_name,
                        requested_instances,
                        max_instances,
                        "Clamped restored app instance count to server maximum"
                    );
                }
            }
            let should_start = config.compose.is_some() || config.min_instances > 0;
            match ensure_app_runtime_data_dirs(&self.runtime.data_dir, &app_name) {
                Ok(paths) => inject_app_data_dir_env(&mut config.env_vars, &paths),
                Err(error) => {
                    tracing::error!(app = %app_name, "Failed to prepare app data dirs: {}", error);
                    errors.push(format!("{app_name}: {error}"));
                    self.record_persisted_restore_failure(&persisted.config, &error);
                    continue;
                }
            }
            config.secrets = self.state_store.get_secrets(&app_name).unwrap_or_else(|e| {
                tracing::warn!(app = %app_name, "Failed to read secrets: {}", e);
                errors.push(format!("{app_name}: {e}"));
                HashMap::new()
            });
            if matches!(config.launch, crate::instances::AppLaunch::Container { .. }) {
                if let Some(path) = config.env_vars.get(crate::release::TAKO_APP_DATA_DIR_ENV) {
                    if let Err(error) = crate::container_runtime::cleanup_app_containers(path).await
                    {
                        tracing::error!(app = %app_name, %error, "Failed to reconcile app containers");
                        errors.push(format!("{app_name}: {error}"));
                        self.record_persisted_restore_failure(&persisted.config, &error);
                        continue;
                    }
                }
            }
            config.storages = self
                .state_store
                .get_storages(&app_name)
                .unwrap_or_else(|e| {
                    tracing::warn!(app = %app_name, "Failed to read storages: {}", e);
                    errors.push(format!("{app_name}: {e}"));
                    HashMap::new()
                });
            config.databases = match self.state_store.get_app_databases(&app_name) {
                Ok(bindings) => bindings,
                Err(e) => {
                    tracing::error!(app = %app_name, "Failed to read database bindings: {}", e);
                    errors.push(format!("{app_name}: {e}"));
                    continue;
                }
            };
            // Before the app's processes start, so they can reach them.
            self.databases
                .apply_bindings(&config.deployment_id(), &[&config.databases]);
            if config.compose.is_some() {
                // Re-bind the group's socket before re-attaching its SDK
                // containers. They keep the stored token.
                config.compose_token = self
                    .state_store
                    .get_compose_token(&app_name)
                    .unwrap_or_else(|e| {
                        tracing::warn!(app = %app_name, "Failed to read the Compose SDK token: {}", e);
                        errors.push(format!("{app_name}: {e}"));
                        None
                    });
                if let Err(error) = self.prepare_compose_sdk(&mut config) {
                    tracing::error!(app = %app_name, %error, "Failed to prepare the Compose SDK runtime");
                    errors.push(format!("{app_name}: {error}"));
                }
            }

            // An app whose own state could not be read is not registered, so
            // neither a request nor an operator can start it without that state.
            if errors.len() > errors_before {
                self.databases.remove_bindings(&app_name);
                if config.compose.is_some() {
                    self.retire_compose_socket(&app_name).await;
                }
                let error = app_restore_error(&app_name, &errors[errors_before..]);
                self.record_persisted_restore_failure(&config, &error);
                if strict {
                    break;
                }
                continue;
            }

            let app = self.app_manager.register_app(config.clone());
            self.load_balancer.register_app(app.clone());

            {
                let mut route_table = self.routes.write();
                route_table.set_config_routes(&config, routes);
            }

            // Neither the app nor its workflows start on data a restore left
            // partly restored.
            if let Some(reason) = self.unresolved_restore_block(&app_name) {
                tracing::error!(app = %app_name, "{reason}");
                // Also stops Compose containers that outlived the server.
                if let Err(error) = self.app_manager.stop_app(&app_name).await {
                    tracing::error!(app = %app_name, "Could not stop the app: {error}");
                }
                app.set_last_error(reason);
                continue;
            }
            if config.stopped && config.compose.is_none() {
                app.set_state(AppState::Stopped);
                self.cold_start.mark_failed(&app_name, "app_stopped");
                if let Err(error) = self
                    .finalize_release_inner(&app_name, &config.version)
                    .await
                {
                    tracing::warn!(app = %app_name, %error, "Could not finalize restored release");
                }
                continue;
            }
            if !config.stopped
                && let Err(error) = self
                    .try_sync_app_workflows(&app_name, &release_path, runtime_bin_path.as_deref())
                    .await
            {
                errors.push(format!("{app_name}: {error}"));
            }
            if strict && !errors.is_empty() {
                break;
            }
            // Without strict mode the other apps still restore. This one is
            // unregistered again, as if its state had failed to read: nothing
            // has started it yet, and a cold start must not.
            if errors.len() > errors_before {
                self.app_manager.remove_app(&app_name);
                self.load_balancer.unregister_app(&app_name);
                self.routes.write().remove_app_routes(&app_name);
                self.databases.remove_bindings(&app_name);
                if config.compose.is_some() {
                    self.retire_compose_socket(&app_name).await;
                }
                let error = app_restore_error(&app_name, &errors[errors_before..]);
                self.record_persisted_restore_failure(&config, &error);
                continue;
            }

            if should_start {
                match self.app_manager.start_app(&app_name).await {
                    Ok(()) => {
                        app.set_state(if config.stopped {
                            AppState::Stopped
                        } else {
                            AppState::Running
                        });
                        if let Err(error) = self
                            .finalize_release_inner(&app_name, &config.version)
                            .await
                        {
                            tracing::warn!(app = %app_name, %error, "Could not finalize restored release");
                        }
                        if config.compose.is_some() {
                            self.state_store.clear_compose_deployment(&app_name)?;
                        }
                        tracing::info!(app = %app_name, "Restored and started app");
                    }
                    Err(e) => {
                        app.set_state(AppState::Error);
                        app.set_last_error(format!("Restore startup failed: {}", e));
                        tracing::error!(app = %app_name, "Failed to start restored app: {}", e);
                        errors.push(format!("{app_name}: {e}"));
                    }
                }
            } else {
                app.set_state(AppState::Idle);
                self.cold_start.reset(&app_name);
                if let Err(error) = self
                    .finalize_release_inner(&app_name, &config.version)
                    .await
                {
                    tracing::warn!(app = %app_name, %error, "Could not finalize restored release");
                }
                tracing::info!(app = %app_name, "Restored on-demand app in idle state");
            }
        }

        if strict && !errors.is_empty() {
            return Err(StateStoreError::InvalidData(errors.join("; ")));
        }
        Ok(())
    }

    /// An app a restore could not register keeps no routes or instances, so
    /// status reports it from here rather than from the app manager.
    fn record_restore_failure(
        &self,
        app_name: &str,
        version: &str,
        compose: bool,
        error: &impl std::fmt::Display,
    ) {
        self.restore_failures.write().insert(
            app_name.to_string(),
            super::RestoreFailure {
                version: version.to_string(),
                compose,
                error: format!("Restore failed: {error}"),
            },
        );
    }

    fn record_persisted_restore_failure(
        &self,
        config: &crate::instances::AppConfig,
        error: &impl std::fmt::Display,
    ) {
        self.record_restore_failure(
            &config.deployment_id(),
            &config.version,
            config.compose.is_some(),
            error,
        );
    }

    pub async fn persist_app_state(&self, app_name: &str) {
        let Some(app) = self.app_manager.get_app(app_name) else {
            return;
        };
        let config = app.config.read().clone();
        let routes = {
            let route_table = self.routes.read();
            route_table.routes_for_app(app_name)
        };
        if let Err(e) = self.state_store.upsert_app(&config, &routes) {
            tracing::warn!(app = app_name, "Failed to persist app state: {}", e);
        }
    }
}

/// This app's entries of the run-wide error list, without their app prefix.
fn app_restore_error(app_name: &str, errors: &[String]) -> String {
    let prefix = format!("{app_name}: ");
    errors
        .iter()
        .map(|error| error.strip_prefix(&prefix).unwrap_or(error))
        .collect::<Vec<_>>()
        .join("; ")
}

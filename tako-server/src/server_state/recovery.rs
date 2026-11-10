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
        let apps = self.state_store.load_apps()?;
        for (app, project) in self.state_store.pending_compose_deployments()? {
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
                        errors.push(format!("{app}: {error}"));
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

            let release_path = release_app_path(&self.runtime.data_dir, &config);
            let runtime_bin_path =
                match resolve_release_runtime_bin(&release_path, &self.runtime.data_dir).await {
                    Ok(bin) => bin,
                    Err(error) => {
                        tracing::error!(app = %app_name, %error, "Failed to restore app runtime");
                        errors.push(format!("{app_name}: {error}"));
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
            let should_start = config.min_instances > 0;
            match ensure_app_runtime_data_dirs(&self.runtime.data_dir, &app_name) {
                Ok(paths) => inject_app_data_dir_env(&mut config.env_vars, &paths),
                Err(error) => {
                    tracing::error!(app = %app_name, "Failed to prepare app data dirs: {}", error);
                    errors.push(format!("{app_name}: {error}"));
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

            let app = self.app_manager.register_app(config.clone());
            self.load_balancer.register_app(app.clone());

            {
                let mut route_table = self.routes.write();
                route_table.set_app_routes_with_source_ip(
                    app_name.clone(),
                    routes,
                    config.source_ip,
                );
            }

            if let Err(error) = self
                .try_sync_app_workflows(&app_name, &release_path, runtime_bin_path.as_deref())
                .await
            {
                errors.push(format!("{app_name}: {error}"));
            }
            if strict && !errors.is_empty() {
                break;
            }

            if should_start {
                match self.app_manager.start_app(&app_name).await {
                    Ok(()) => {
                        app.set_state(AppState::Running);
                        if config.compose.is_some() {
                            // The process or deploy client can disappear after
                            // the database commit but before CLI finalization.
                            // Repair the derived current link from that commit.
                            if let Err(error) = self
                                .finalize_release_inner(&app_name, &config.version)
                                .await
                            {
                                tracing::warn!(app = %app_name, %error, "Could not finalize restored Compose release");
                            }
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
                tracing::info!(app = %app_name, "Restored on-demand app in idle state");
            }
        }

        if strict && !errors.is_empty() {
            return Err(StateStoreError::InvalidData(errors.join("; ")));
        }
        Ok(())
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

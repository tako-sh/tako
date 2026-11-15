use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use crate::instances::{AppConfig, RollingUpdateConfig, RollingUpdater};
use crate::socket::{AppState, InstanceState, Response};

pub(super) struct ComposeDeployment {
    pub config: AppConfig,
    pub routes: Vec<String>,
    pub runtime_credentials: HashMap<String, String>,
    pub ssl: Option<tako_core::SslBinding>,
    pub backup: Option<tako_core::BackupBinding>,
}

impl crate::ServerState {
    /// Caller holds the per-app deployment lock. Preparation and application
    /// readiness precede the durable commit; cleanup follows traffic draining.
    pub(super) async fn deploy_compose(&self, request: ComposeDeployment) -> Response {
        let ComposeDeployment {
            mut config,
            routes,
            runtime_credentials,
            ssl,
            backup,
        } = request;
        let app_name = config.deployment_id();
        if config.compose.as_ref().map(|c| {
            c.routes(&config.environment)
                .into_iter()
                .collect::<BTreeSet<_>>()
        }) != Some(routes.iter().cloned().collect())
        {
            return Response::error("Compose routes must match the service routes in the release");
        }
        // Build the runtime with the previous stop intent intact: if this
        // rollout fails, predecessor recovery must still skip dependencies the
        // operator stopped, even ones the new release removes. Only the
        // configuration committed below drops services that no longer exist.
        let runtime = match crate::compose_runtime::for_config(&config) {
            Ok(runtime) => runtime,
            Err(error) => return Response::error(error),
        };
        config.compose_stopped.retain(|service| {
            config
                .compose
                .as_ref()
                .unwrap()
                .services
                .contains_key(service)
        });
        let mut rollout = match runtime.begin_rollout().await {
            Ok(rollout) => rollout,
            Err(error) => return Response::error(error),
        };
        let replicas = rollout.replicas();
        config.min_instances = runtime.ingress_count(&replicas);
        config
            .compose_replicas
            .retain(|service, _| replicas.contains_key(service));
        let updates: Vec<_> = rollout.changes().into_iter().map(|(service, order)| serde_json::json!({
            "service": service,
            "order": match order { tako_compose::UpdateOrder::StartFirst => "start-first", tako_compose::UpdateOrder::StopFirst => "stop-first" },
        })).collect();
        if let Err(error) = self
            .state_store
            .begin_compose_deployment(&app_name, &runtime.project)
        {
            return Response::error(format!(
                "Could not record pending Compose deployment: {error}"
            ));
        }
        let existing = self.app_manager.get_app(&app_name);
        let previous = existing
            .as_ref()
            .map(|app| (app.config.read().clone(), app.state()));
        let app = existing.unwrap_or_else(|| {
            let app = self.app_manager.register_app(config.clone());
            self.load_balancer.register_app(app.clone());
            app
        });
        let old_instances = app.get_instances();
        app.set_state(AppState::Deploying);
        app.update_config(config.clone());
        // App::update_config supplies the default health Host header. Use that
        // normalized config for readiness as the ordinary spawner does.
        let config = app.config.read().clone();
        let mut instances = Vec::new();
        let updater = Arc::new(RollingUpdater::new(
            self.app_manager.spawner(),
            RollingUpdateConfig::default(),
        ));
        let result: Result<(), String> = async {
            rollout.apply().await?;
            for web in rollout.ingresses() {
                let instance = app.allocate_instance();
                app.suppress_instance_routing(&instance.id);
                instances.push(instance.clone());
                self.app_manager
                    .spawner()
                    .attach_compose(&app, instance, &runtime, web)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            updater
                .wait_for_stable_health(&instances)
                .await
                .map_err(|e| e.to_string())?;
            self.state_store
                .commit_deployment(
                    &config,
                    &routes,
                    &runtime_credentials,
                    ssl.as_ref(),
                    backup.as_ref(),
                    None,
                )
                .map_err(|e| format!("Could not commit Compose deployment: {e}"))?;
            Ok(())
        }
        .await;

        if let Err(error) = result {
            // The transaction owns containers; killing this logical instance
            // only detaches its log client until ownership has been committed.
            for instance in &instances {
                let _ = instance.kill().await;
                app.remove_instance(&instance.id);
            }
            let mut recovery = rollout.rollback().await;
            if let Some((previous_config, previous_state)) = previous {
                app.update_config(previous_config.clone());
                if recovery.is_ok() && previous_config.compose.is_some() {
                    recovery = self
                        .app_manager
                        .spawner()
                        .reconcile_compose(&app)
                        .await
                        .map_err(|e| e.to_string());
                }
                app.set_state(if recovery.is_ok() {
                    previous_state
                } else {
                    AppState::Error
                });
            } else if recovery.is_ok() {
                self.app_manager.remove_app(&app_name);
                self.load_balancer.unregister_app(&app_name);
            } else {
                app.set_state(AppState::Error);
            }
            let message = match recovery {
                Ok(()) => {
                    if let Err(pending_error) = self.state_store.clear_compose_deployment(&app_name)
                    {
                        tracing::warn!(app = %app_name, %pending_error, "Could not clear recovered Compose deployment marker");
                    }
                    format!("Deploy failed; previous containers restored: {error}")
                }
                Err(recovery_error) => {
                    format!("Deploy failed: {error}; Compose recovery failed: {recovery_error}")
                }
            };
            app.set_last_error(message.clone());
            return Response::error(message);
        }

        // From here onward the new release is authoritative, including after
        // process termination. Cleanup failures must not roll its metadata back.
        {
            self.routes
                .write()
                .set_config_routes(&config, routes.clone());
        }
        for route in &routes {
            self.ensure_route_certificate(&app_name, route.split('/').next().unwrap_or(route))
                .await;
        }
        for instance in &instances {
            instance.own_compose_container(true);
            app.enable_instance_routing(instance);
        }
        let mut drains = tokio::task::JoinSet::new();
        for old in &old_instances {
            if instances
                .iter()
                .any(|instance| old.compose_container_id() == instance.compose_container_id())
            {
                // An unchanged HTTP container transfers ownership to the new
                // logical release. Retiring the old log client cannot stop it.
                old.own_compose_container(false);
            }
            // Stop admitting traffic to every predecessor before waiting for
            // any one request or Docker stop grace period to finish.
            app.set_instance_state(old, InstanceState::Draining);
            let app = app.clone();
            let old = old.clone();
            let updater = updater.clone();
            drains.spawn(async move { updater.drain_and_stop(&app, &old).await });
        }
        let mut cleanup_errors = Vec::new();
        if let Err(error) = app.sync_compose_logs(&runtime, rollout.containers()).await {
            cleanup_errors.push(error);
        }
        while let Some(result) = drains.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => cleanup_errors.push(error.to_string()),
                Err(error) => cleanup_errors.push(format!("Replica drain failed: {error}")),
            }
        }
        if let Err(error) = rollout.finish().await {
            cleanup_errors.push(error);
        }
        let cleanup_error = (!cleanup_errors.is_empty()).then(|| cleanup_errors.join("; "));
        app.set_state(if config.stopped {
            AppState::Stopped
        } else {
            AppState::Running
        });
        app.reset_recovery();
        if let Some(error) = &cleanup_error {
            app.set_last_error(format!(
                "Deployment activated; old container cleanup pending: {error}"
            ));
        } else {
            app.clear_last_error();
        }
        crate::runtime_events::update_instance_count_metric(&app_name, &app);
        let release_path =
            crate::release::app_release_root(&self.runtime.data_dir, &app_name, &config.version);
        if config.stopped {
            self.cold_start.mark_failed(&app_name, "app_stopped");
            self.workflows
                .stop(&app_name, std::time::Duration::from_secs(120))
                .await;
        } else {
            self.sync_app_workflows(&app_name, &release_path, None)
                .await;
        }
        Response::ok(serde_json::json!({
            "status":"deployed", "app":app_name, "version":config.version,
            "new_app":previous.is_none(), "updates":updates, "cleanup_error":cleanup_error,
            "replicas":replicas, "replica_overrides":config.compose_replicas,
        }))
    }

    /// Caller holds the per-app deployment lock. Scaling uses the same durable
    /// commit and rollback boundary as deployment, including when only a worker changes.
    pub(super) async fn scale_compose(
        &self,
        mut config: AppConfig,
        requested: Option<u8>,
        service: Option<&str>,
    ) -> Response {
        let service = match service {
            Some(service) => service.to_owned(),
            None => match config.compose.as_ref().unwrap().default_service() {
                Ok(service) => service.to_owned(),
                Err(error) => return Response::error(error),
            },
        };
        if let Some(count) = requested {
            config
                .compose_replicas
                .insert(service.clone(), u32::from(count));
        } else {
            config.compose_replicas.remove(&service);
        }
        let runtime = match crate::compose_runtime::for_config(&config) {
            Ok(runtime) => runtime,
            Err(error) => return Response::error(error),
        };
        let plan = match runtime.begin_rollout().await {
            Ok(plan) => plan,
            Err(error) => return Response::error(error),
        };
        let Some(count) = plan.configured_replicas().get(&service).copied() else {
            return Response::error(format!("Unknown Compose service '{service}'"));
        };
        let stopped = config.stopped || config.compose_stopped.contains(&service);
        match self.apply_compose_config(config).await {
            Response::Ok { mut data } => {
                data["status"] = serde_json::json!("scaled");
                data["service"] = serde_json::json!(service);
                data["instances"] = serde_json::json!(count);
                data["requested_instances"] = serde_json::json!(requested);
                data["standby_limited"] = serde_json::json!(false);
                data["stopped"] = serde_json::json!(stopped);
                Response::Ok { data }
            }
            error => error,
        }
    }

    pub(super) async fn set_compose_running(
        &self,
        mut config: AppConfig,
        service: Option<&str>,
        running: bool,
    ) -> Response {
        if let Some(service) = service {
            if !config
                .compose
                .as_ref()
                .unwrap()
                .services
                .contains_key(service)
            {
                return Response::error(format!(
                    "'{service}' is not a Tako app in this group; individual start/stop requires x-tako"
                ));
            }
            if running && config.stopped {
                return Response::error("The application group is stopped; start the group first");
            }
            if running {
                config.compose_stopped.remove(service);
            } else {
                config.compose_stopped.insert(service.to_owned());
            }
        } else {
            config.stopped = !running;
        }
        match self.apply_compose_config(config).await {
            Response::Ok { mut data } => {
                data["status"] = serde_json::json!(if running { "started" } else { "stopped" });
                data["service"] = serde_json::json!(service);
                Response::Ok { data }
            }
            error => error,
        }
    }

    pub(super) async fn apply_compose_config(&self, config: AppConfig) -> Response {
        let app = config.deployment_id();
        let bindings = (|| -> Result<_, crate::state_store::StateStoreError> {
            Ok((
                self.state_store.get_runtime_credentials(&app)?,
                self.state_store.get_ssl(&app)?,
                self.state_store.get_backup(&app)?,
            ))
        })();
        let (runtime_credentials, ssl, backup) = match bindings {
            Ok(bindings) => bindings,
            Err(error) => {
                return Response::error(format!("Could not load deployment credentials: {error}"));
            }
        };
        // Persisted route rows can have a different order after a restart.
        // The release's per-service metadata remains authoritative.
        let routes = config.compose.as_ref().unwrap().routes(&config.environment);
        self.deploy_compose(ComposeDeployment {
            config,
            routes,
            runtime_credentials,
            ssl,
            backup,
        })
        .await
    }
}

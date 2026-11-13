use std::collections::HashMap;
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
        let runtime = match crate::compose_runtime::for_config(&config) {
            Ok(runtime) => runtime,
            Err(error) => return Response::error(error),
        };
        let mut rollout = match runtime.begin_rollout().await {
            Ok(rollout) => rollout,
            Err(error) => return Response::error(error),
        };
        let replicas = rollout.replicas();
        config.min_instances = replicas[&runtime.config.service];
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
                    .attach_compose(&app, instance, &runtime, web, rollout.containers())
                    .await
                    .map_err(|e| e.to_string())?;
            }
            updater
                .wait_for_stable_health(&instances)
                .await
                .map_err(|e| e.to_string())?;
            self.state_store
                .commit_compose_deployment(
                    &config,
                    &routes,
                    &runtime_credentials,
                    ssl.as_ref(),
                    backup.as_ref(),
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
                if recovery.is_ok()
                    && previous_config.compose.is_some()
                    && !old_instances.is_empty()
                {
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
            self.routes.write().set_app_routes_with_source_ip(
                app_name.clone(),
                routes.clone(),
                config.source_ip,
            );
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
        app.set_state(AppState::Running);
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
        self.sync_app_workflows(&app_name, &release_path, None)
            .await;
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
        let app = config.deployment_id();
        let service = service
            .unwrap_or(&config.compose.as_ref().unwrap().service)
            .to_owned();
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
        let Some(count) = plan.replicas().get(&service).copied() else {
            return Response::error(format!("Unknown Compose service '{service}'"));
        };
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
        let routes = self.routes.read().routes_for_app(&app);
        match self
            .deploy_compose(ComposeDeployment {
                config,
                routes,
                runtime_credentials,
                ssl,
                backup,
            })
            .await
        {
            Response::Ok { mut data } => {
                data["status"] = serde_json::json!("scaled");
                data["service"] = serde_json::json!(service);
                data["instances"] = serde_json::json!(count);
                data["requested_instances"] = serde_json::json!(requested);
                data["standby_limited"] = serde_json::json!(false);
                Response::Ok { data }
            }
            error => error,
        }
    }
}

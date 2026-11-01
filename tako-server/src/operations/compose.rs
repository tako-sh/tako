use std::collections::HashMap;

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
            config,
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
        let instance = app.allocate_instance();
        app.suppress_instance_routing(&instance.id);
        let updater =
            RollingUpdater::new(self.app_manager.spawner(), RollingUpdateConfig::default());
        let result: Result<(), String> = async {
            rollout.apply().await?;
            self.app_manager
                .spawner()
                .attach_compose(
                    &app,
                    instance.clone(),
                    &config,
                    &runtime,
                    rollout.containers(),
                )
                .await
                .map_err(|e| e.to_string())?;
            updater
                .wait_for_stable_health(std::slice::from_ref(&instance))
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
            let _ = instance.kill().await;
            app.remove_instance(&instance.id);
            let mut recovery = rollout.rollback().await;
            if let Some((previous_config, previous_state)) = previous {
                app.update_config(previous_config.clone());
                if recovery.is_ok()
                    && previous_config.compose.is_some()
                    && !old_instances.is_empty()
                {
                    // Stop-first recovery can receive a different ephemeral
                    // published port. Re-adopt the restored containers and log
                    // client before returning their HTTP instances to routing.
                    recovery = async {
                        let previous_runtime =
                            crate::compose_runtime::for_config(&previous_config)?;
                        let mut restored = previous_runtime.begin_rollout().await?;
                        restored.apply().await?;
                        let ingress = restored.ingress()?;
                        for old in &old_instances {
                            // A failed start-first update leaves the predecessor
                            // serving. Reattaching it would briefly mark it
                            // Starting and unnecessarily remove it from routing.
                            if old.compose_container_id().as_deref() == Some(&ingress.id)
                                && old
                                    .endpoint()
                                    .is_some_and(|endpoint| Some(endpoint.port()) == ingress.port)
                                && old.state() == InstanceState::Healthy
                                && old.is_alive().await
                            {
                                continue;
                            }
                            old.own_compose_container(false);
                            self.app_manager
                                .spawner()
                                .attach_compose(
                                    &app,
                                    old.clone(),
                                    &previous_config,
                                    &previous_runtime,
                                    restored.containers(),
                                )
                                .await
                                .map_err(|e| e.to_string())?;
                            old.own_compose_container(true);
                        }
                        restored.finish().await
                    }
                    .await;
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
        instance.own_compose_container(true);
        app.enable_instance_routing(&instance);
        for old in &old_instances {
            if old.compose_container_id() == instance.compose_container_id() {
                // An unchanged HTTP container transfers ownership to the new
                // logical release. Retiring the old log client cannot stop it.
                old.own_compose_container(false);
            }
            let _ = updater.drain_and_stop(&app, old).await;
        }
        let cleanup_error = rollout.finish().await.err();
        app.set_state(AppState::Running);
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
        }))
    }
}

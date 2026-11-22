use crate::instances::{App, RollingUpdateConfig, validate_requested_instances};
use crate::metrics;
use crate::release::{app_root, requested_deployment_identity};
use crate::socket::{AppState, InstanceState, Response};
use std::sync::Arc;
use std::time::Duration;

impl crate::ServerState {
    pub(crate) async fn start_app(&self, app_name: &str, service: Option<&str>) -> Response {
        self.set_app_running(app_name, service, true).await
    }

    pub(crate) async fn stop_app(&self, app_name: &str, service: Option<&str>) -> Response {
        self.set_app_running(app_name, service, false).await
    }

    async fn set_app_running(
        &self,
        app_name: &str,
        service: Option<&str>,
        running: bool,
    ) -> Response {
        tracing::info!(app = app_name, service, running, "Setting app lifecycle");
        let lock = self.get_deploy_lock(app_name).await;
        let _guard = lock.lock().await;

        let Some(app) = self.app_manager.get_app(app_name) else {
            return Response::error(format!("App not found: {app_name}"));
        };
        if (running || service.is_some())
            && let Some(response) = self.refuse_unresolved_restore(app_name)
        {
            return response;
        }
        let previous = app.config.read().clone();
        if previous.compose.is_some() {
            return self.set_compose_running(previous, service, running).await;
        }
        if service.is_some() {
            return Response::error("--service requires a Compose application group");
        }
        let _lifecycle = app.lifecycle_gate.write().await;
        let routes = self.routes.read().routes_for_app(app_name);
        let mut config = previous.clone();
        config.stopped = !running;
        if running {
            if !previous.stopped && app.state() == AppState::Running {
                return Response::ok(serde_json::json!({"status":"started", "app":app_name}));
            }
            let previous_state = app.state();
            app.update_config(config.clone());
            self.cold_start.reset(app_name);
            let result = async {
                self.app_manager
                    .start_app_locked(&app)
                    .await
                    .map_err(|e| e.to_string())?;
                if config.min_instances == 0 && app.get_instances().is_empty() {
                    self.app_manager
                        .spawner()
                        .spawn(&app, app.allocate_instance())
                        .await
                        .map_err(|e| e.to_string())?;
                }
                let release = crate::release::app_release_root(
                    &self.runtime.data_dir,
                    app_name,
                    &config.version,
                );
                let runtime_bin =
                    crate::release::resolve_release_runtime_bin(&release, &self.runtime.data_dir)
                        .await?;
                self.try_sync_app_workflows(app_name, &release, runtime_bin.as_deref())
                    .await
                    .map_err(|e| e.to_string())?;
                self.state_store
                    .upsert_app(&config, &routes)
                    .map_err(|e| e.to_string())
            }
            .await;
            if let Err(error) = result {
                if previous.stopped {
                    self.workflows
                        .stop(app_name, Duration::from_secs(120))
                        .await;
                    let _ = self.app_manager.stop_app_locked(&app).await;
                } else {
                    // The operator never stopped this app: keep what was
                    // already serving and leave cold starts and recovery on.
                    app.set_state(previous_state);
                }
                app.update_config(previous);
                app.set_last_error(format!("Start failed: {error}"));
                return Response::error(format!("Start failed: {error}"));
            }
            app.reset_recovery();
            app.clear_last_error();
            app.set_state(AppState::Running);
            self.cold_start.mark_ready(app_name);
            return Response::ok(serde_json::json!({"status":"started", "app":app_name}));
        }
        // Persist intent before teardown: a crash cannot silently resume the app.
        if let Err(error) = self.state_store.upsert_app(&config, &routes) {
            return Response::error(format!("Could not persist stopped state: {error}"));
        }
        app.update_config(config);
        self.cold_start.mark_failed(app_name, "app_stopped");

        // Drain workflow worker first so in-flight tasks get a chance to
        // finish before HTTP instances are torn down. 120s hard cap.
        self.workflows
            .stop(app_name, Duration::from_secs(120))
            .await;

        match self.app_manager.stop_app_locked(&app).await {
            Ok(()) => Response::ok(serde_json::json!({
                "status": "stopped",
                "app": app_name
            })),
            Err(e) => Response::error(format!("Stop failed: {}", e)),
        }
    }

    pub(crate) async fn scale_app(
        &self,
        app_name: &str,
        requested_instances: Option<u8>,
        service: Option<&str>,
        reset: bool,
    ) -> Response {
        tracing::info!(
            app = app_name,
            ?requested_instances,
            service,
            reset,
            "Scaling app"
        );
        if reset == requested_instances.is_some() {
            return Response::error("Specify an instance count or reset, exclusively");
        }
        let lock = self.get_deploy_lock(app_name).await;
        let _guard = lock.lock().await;

        let app = match self.app_manager.get_app(app_name) {
            Some(app) => app,
            None => return Response::error(format!("App not found: {}", app_name)),
        };
        if let Some(response) = self.refuse_unresolved_restore(app_name) {
            return response;
        }

        let previous_config = app.config.read().clone();
        if previous_config.compose.is_some() {
            return self
                .scale_compose(previous_config, requested_instances, service)
                .await;
        }
        if service.is_some() || reset {
            return Response::error("--service and --reset require a Compose app");
        }
        let requested_instances = requested_instances.unwrap();
        let effective_instances = if self.runtime.standby {
            requested_instances.min(1)
        } else {
            requested_instances
        };

        let mut next_config = previous_config.clone();
        if let Err(message) = validate_requested_instances(
            u32::from(effective_instances),
            previous_config.max_instances,
        ) {
            return Response::error(message);
        }
        next_config.min_instances = effective_instances as u32;
        if next_config.stopped {
            let routes = self.routes.read().routes_for_app(app_name);
            if let Err(error) = self.state_store.upsert_app(&next_config, &routes) {
                return Response::error(format!("Could not persist replica count: {error}"));
            }
            app.update_config(next_config);
            return Response::ok(serde_json::json!({
                "status":"scaled", "app":app_name, "instances":effective_instances,
                "requested_instances":requested_instances, "stopped":true,
                "standby_limited":self.runtime.standby && effective_instances != requested_instances
            }));
        }
        app.update_config(next_config.clone());

        let running_before = app
            .get_instances()
            .into_iter()
            .filter(|instance| {
                matches!(
                    instance.state(),
                    InstanceState::Starting | InstanceState::Ready | InstanceState::Healthy
                )
            })
            .count();

        if effective_instances as usize > running_before {
            let to_add = effective_instances as usize - running_before;
            let mut started_instances = Vec::with_capacity(to_add);

            for _ in 0..to_add {
                let instance = app.allocate_instance();
                match self
                    .app_manager
                    .spawner()
                    .spawn(&app, instance.clone())
                    .await
                {
                    Ok(()) => started_instances.push(instance),
                    Err(error) => {
                        for started in started_instances {
                            let _ = started.kill().await;
                            app.remove_instance(&started.id);
                        }
                        app.update_config(previous_config);
                        return Response::error(format!("Scale failed: {}", error));
                    }
                }
            }
        } else if (effective_instances as usize) < running_before {
            let mut candidates: Vec<_> = app
                .get_instances()
                .into_iter()
                .filter(|instance| {
                    matches!(
                        instance.state(),
                        InstanceState::Starting | InstanceState::Ready | InstanceState::Healthy
                    )
                })
                .collect();
            candidates.sort_by_key(|instance| std::cmp::Reverse(instance.idle_time()));

            let to_remove = running_before - effective_instances as usize;
            for instance in candidates.into_iter().take(to_remove) {
                if let Err(error) = self.drain_and_stop_instance(&app, &instance).await {
                    return Response::error(format!("Scale failed: {}", error));
                }
            }
        }

        crate::runtime_events::update_instance_count_metric(app_name, &app);
        // The operator just set the instance count and saw it take effect;
        // earlier crash history and its backoff no longer apply.
        app.reset_recovery();
        if app.get_instances().is_empty() && effective_instances == 0 {
            app.set_state(AppState::Idle);
            self.cold_start.reset(app_name);
        } else {
            app.set_state(AppState::Running);
        }

        self.persist_app_state(app_name).await;

        Response::ok(serde_json::json!({
            "status": "scaled",
            "app": app_name,
            "instances": effective_instances,
            "requested_instances": requested_instances,
            "standby_limited": self.runtime.standby && effective_instances != requested_instances
        }))
    }

    pub(crate) async fn drain_and_stop_instance(
        &self,
        app: &Arc<App>,
        instance: &Arc<crate::instances::Instance>,
    ) -> Result<(), String> {
        app.set_instance_state(instance, InstanceState::Draining);
        if !crate::instances::wait_for_short_requests(
            instance,
            RollingUpdateConfig::default().drain_timeout,
        )
        .await
        {
            tracing::warn!(
                app = %app.name(),
                instance = %instance.id,
                in_flight = instance.in_flight(),
                "Scale drain timeout exceeded, forcing stop"
            );
        }

        instance
            .kill()
            .await
            .map_err(|error| format!("failed to stop instance '{}': {}", instance.id, error))?;
        app.remove_instance(&instance.id);
        metrics::remove_instance_metrics(&app.name(), &instance.id);
        Ok(())
    }

    pub(crate) async fn delete_app(&self, app_name: &str) -> Response {
        if let Some(message) = self.compose_child_delete_error(app_name) {
            return Response::error(message);
        }
        // A bare name denotes production. Its filesystem path alone is the
        // shared parent of every environment, so qualify it before any cleanup.
        let (name, environment) = requested_deployment_identity(app_name);
        let deployment = tako_core::deployment_app_id(&name, &environment);
        let app_name = deployment.as_str();
        tracing::info!(app = app_name, "Deleting app");

        let lock = self.get_deploy_lock(app_name).await;
        let _guard = lock.lock().await;

        // Drain workflow resources (worker, cron, enqueue socket) + remove
        // workflow state BEFORE we nuke app_root — the manager owns those files.
        self.workflows
            .delete(app_name, Duration::from_secs(120))
            .await;

        let app_root = app_root(&self.runtime.data_dir, app_name);
        let owns_compose = app_root.join("compose").is_dir()
            || self
                .app_manager
                .get_app(app_name)
                .is_some_and(|app| app.config.read().compose.is_some());
        let mut existed = false;
        if self.app_manager.get_app(app_name).is_some() {
            existed = true;
            if let Err(e) = self.app_manager.stop_app(app_name).await {
                return Response::error(format!("Delete failed: {}", e));
            }
        }
        if owns_compose {
            if let Err(error) =
                tako_compose::remove_app_containers(&app_root.join("data/app")).await
            {
                return Response::error(format!("Delete failed: {error}"));
            }
            self.retire_compose_socket(app_name).await;
            let project = tako_compose::production_project(&app_root.join("data/app"));
            let _ = std::fs::remove_dir_all(
                crate::compose_runtime::socket_root(&self.runtime.data_dir).join(project),
            );
        }
        self.app_manager.remove_app(app_name);
        self.databases.remove_bindings(app_name);
        if let Ok(scope) = crate::native_channel_client::app_scope(app_name) {
            self.direct_channel_stores.remove(&scope);
        }

        self.load_balancer.unregister_app(app_name);
        self.cold_start.reset(app_name);
        self.manual_backup_states.write().remove(app_name);
        self.restore_failures.write().remove(app_name);

        {
            let mut route_table = self.routes.write();
            route_table.remove_app_routes(app_name);
        }

        if let Err(e) = self.state_store.delete_app(&name, &environment) {
            tracing::warn!(
                app = app_name,
                "Failed to delete persisted app state: {}",
                e
            );
        }
        self.channel_credentials_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if let Err(e) = std::fs::remove_dir_all(&app_root)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                app = app_name,
                path = %app_root.display(),
                "Failed to remove app root: {}",
                e
            );
            return Response::error(format!(
                "Delete partially completed, but failed to remove app files '{}': {}",
                app_root.display(),
                e
            ));
        }

        {
            let mut locks = self.deploy_locks.write().await;
            locks.remove(app_name);
        }

        Response::ok(serde_json::json!({
            "status": "deleted",
            "app": app_name,
            "existed": existed
        }))
    }

    pub(crate) fn compose_child_delete_error(&self, app_name: &str) -> Option<String> {
        let (group, service) = app_name.rsplit_once('/')?;
        let app = self.app_manager.get_app(group)?;
        let config = app.config.read();
        config.compose.as_ref()?;
        Some(format!(
            "Cannot delete app '{service}' individually: it belongs to application group '{group}'. Remove the service from the Compose file and redeploy, or delete the whole group."
        ))
    }
}

use crate::instances::{App, RollingUpdateConfig, validate_requested_instances};
use crate::metrics;
use crate::release::{app_root, requested_deployment_identity};
use crate::socket::{AppState, InstanceState, Response};
use std::sync::Arc;
use std::time::Duration;

impl crate::ServerState {
    pub(crate) async fn stop_app(&self, app_name: &str) -> Response {
        tracing::info!(app = app_name, "Stopping app");

        // Drain workflow worker first so in-flight tasks get a chance to
        // finish before HTTP instances are torn down. 120s hard cap.
        self.workflows
            .stop(app_name, Duration::from_secs(120))
            .await;

        match self.app_manager.stop_app(app_name).await {
            Ok(()) => Response::ok(serde_json::json!({
                "status": "stopped",
                "app": app_name
            })),
            Err(e) => Response::error(format!("Stop failed: {}", e)),
        }
    }

    pub(crate) async fn scale_app(&self, app_name: &str, requested_instances: u8) -> Response {
        tracing::info!(app = app_name, requested_instances, "Scaling app");

        let app = match self.app_manager.get_app(app_name) {
            Some(app) => app,
            None => return Response::error(format!("App not found: {}", app_name)),
        };

        let previous_config = app.config.read().clone();
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
        let deadline = tokio::time::Instant::now() + RollingUpdateConfig::default().drain_timeout;
        while instance.in_flight() > 0 {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    app = %app.name(),
                    instance = %instance.id,
                    in_flight = instance.in_flight(),
                    "Scale drain timeout exceeded, forcing stop"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
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
        tracing::info!(app = app_name, "Deleting app");

        // Drain workflow resources (worker, cron, enqueue socket) + remove
        // workflow state BEFORE we nuke app_root — the manager owns those files.
        self.workflows
            .delete(app_name, Duration::from_secs(120))
            .await;

        let mut existed = false;
        if self.app_manager.get_app(app_name).is_some() {
            existed = true;
            if let Err(e) = self.app_manager.stop_app(app_name).await {
                return Response::error(format!("Delete failed: {}", e));
            }
            self.app_manager.remove_app(app_name);
        }

        self.load_balancer.unregister_app(app_name);
        self.cold_start.reset(app_name);

        {
            let mut route_table = self.routes.write();
            route_table.remove_app_routes(app_name);
        }

        {
            let mut locks = self.deploy_locks.write().await;
            locks.remove(app_name);
        }

        let (name, environment) = requested_deployment_identity(app_name);
        if let Err(e) = self.state_store.delete_app(&name, &environment) {
            tracing::warn!(
                app = app_name,
                "Failed to delete persisted app state: {}",
                e
            );
        }
        let app_root = app_root(&self.runtime.data_dir, app_name);
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

        Response::ok(serde_json::json!({
            "status": "deleted",
            "app": app_name,
            "existed": existed
        }))
    }
}

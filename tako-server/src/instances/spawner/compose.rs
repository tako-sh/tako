use super::*;

impl Spawner {
    /// Reconcile the committed stack once, adopting every HTTP replica. Used
    /// at startup, after health failures, and to restore a failed deployment.
    pub(crate) async fn reconcile_compose(&self, app: &App) -> Result<(), InstanceError> {
        let mut config = app.config.read().clone();
        let runtime = crate::compose_runtime::for_config(&config)
            .map_err(InstanceError::HealthCheckFailed)?;
        let mut rollout = runtime
            .begin_rollout()
            .await
            .map_err(InstanceError::HealthCheckFailed)?;
        config.min_instances = rollout.replicas()[&runtime.config.service];
        app.update_config(config.clone());
        let old = app.get_instances();
        let mut attached = Vec::new();
        let mut adopted_ids = std::collections::BTreeSet::new();
        let result: Result<(), InstanceError> = async {
            rollout
                .apply()
                .await
                .map_err(InstanceError::HealthCheckFailed)?;
            for web in rollout.ingresses() {
                let mut adopted = false;
                for instance in &old {
                    if instance.compose_container_id().as_deref() == Some(web.id.as_str())
                        && instance
                            .endpoint()
                            .is_some_and(|e| Some(e.port()) == web.port)
                        && instance.state() == InstanceState::Healthy
                        && instance.is_alive().await
                    {
                        adopted_ids.insert(instance.id.clone());
                        adopted = true;
                        break;
                    }
                }
                if adopted {
                    continue;
                }
                let instance = app.allocate_instance();
                attached.push(instance.clone());
                self.attach_compose(app, instance, &runtime, web, rollout.containers())
                    .await?;
            }
            // Any stale logical observers must relinquish container ownership
            // before cleanup. A restored container may now have a new port.
            for instance in &old {
                if !adopted_ids.contains(&instance.id) {
                    instance.own_compose_container(false);
                    let _ = instance.kill().await;
                    app.remove_instance(&instance.id);
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            for instance in attached {
                let _ = instance.kill().await;
                app.remove_instance(&instance.id);
            }
            return Err(InstanceError::HealthCheckFailed(
                match rollout.rollback().await {
                    Ok(()) => error.to_string(),
                    Err(recovery) => format!("{error}; Compose recovery failed: {recovery}"),
                },
            ));
        }
        for instance in attached {
            instance.own_compose_container(true);
        }
        // The desired configuration is already durable. Cleanup failure must
        // not reintroduce containers from an abandoned generation.
        rollout
            .finish()
            .await
            .map_err(InstanceError::HealthCheckFailed)
    }

    pub(crate) async fn attach_compose(
        &self,
        app: &App,
        instance: Arc<Instance>,
        runtime: &crate::compose_runtime::ComposeRuntime,
        web: &tako_compose::ServiceContainer,
        containers: &[tako_compose::ServiceContainer],
    ) -> Result<(), InstanceError> {
        let config = app.config.read().clone();
        let port = web.port.ok_or_else(|| {
            InstanceError::HealthCheckFailed("Compose HTTP ingress has no loopback port".into())
        })?;
        instance.set_compose(runtime.clone(), web.id.clone());
        instance.set_health_probe(
            config.health_check_host.clone(),
            config.health_check_path.clone(),
        );
        instance.set_sdk_health(false);
        app.set_instance_state(&instance, InstanceState::Starting);
        // Only slot zero follows shared dependency logs; each replica follows
        // its own HTTP container to avoid duplicate app log entries.
        let logs: Vec<_> = containers
            .iter()
            .filter(|c| c.id == web.id || (web.slot == 0 && c.service != runtime.config.service))
            .cloned()
            .collect();
        let child = runtime
            .logs(&logs)
            .await
            .map_err(InstanceError::HealthCheckFailed)?;
        instance.set_process(child);
        instance.set_port(port);
        instance.drain_pipes();
        match timeout(
            config.startup_timeout,
            wait_for_container_health(app, instance.clone(), &config),
        )
        .await
        {
            Ok(Ok(())) => {
                app.set_instance_state(&instance, InstanceState::Healthy);
                let _ = app
                    .instance_tx
                    .send(InstanceEvent::Ready {
                        app: config.deployment_id(),
                        instance_id: instance.id.clone(),
                    })
                    .await;
                Ok(())
            }
            result => {
                let _ = instance.kill().await;
                app.set_instance_state(&instance, InstanceState::Unhealthy);
                Err(InstanceError::HealthCheckFailed(match result {
                    Ok(Err(error)) => error.to_string(),
                    _ => "Compose HTTP service did not become healthy before the startup deadline"
                        .into(),
                }))
            }
        }
    }
}

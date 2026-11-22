use crate::release::collect_running_build_statuses;
use crate::socket::{AppStatus, InstanceStatus, Response};

impl crate::ServerState {
    pub(crate) async fn get_status(&self, app_name: &str) -> Response {
        let app = match self.app_manager.get_app(app_name) {
            Some(app) => app,
            None => {
                if let Some(failure) = self.restore_failures.read().get(app_name) {
                    return Response::ok(restore_failure_status(app_name, failure));
                }
                return Response::error(format!("App not found: {}", app_name));
            }
        };

        let instances: Vec<InstanceStatus> =
            app.get_instances().iter().map(|i| i.status()).collect();
        let builds = collect_running_build_statuses(&app);
        let config = app.config.read().clone();
        let kind = if config.compose.is_some() {
            tako_core::ApplicationKind::ApplicationGroup
        } else {
            tako_core::ApplicationKind::Application
        };
        let (services, services_error) = if let Some(compose) = config.compose.clone() {
            let mut env = config.env_vars.clone();
            env.extend(config.secrets.clone());
            if let Some(data) = config
                .env_vars
                .get(crate::release::TAKO_SHARED_DATA_DIR_ENV)
            {
                env.insert(
                    crate::release::TAKO_SHARED_DATA_DIR_ENV.into(),
                    data.clone(),
                );
            }
            if let Some(data) = config.env_vars.get(crate::release::TAKO_APP_DATA_DIR_ENV) {
                env.insert(crate::release::TAKO_APP_DATA_DIR_ENV.into(), data.clone());
            }
            let result = match crate::compose_runtime::ComposeRuntime::new(
                config.path.clone(),
                compose,
                env,
            ) {
                Ok(runtime) => runtime.status().await,
                Err(error) => Err(error),
            };
            match result {
                Ok(services) => (services, None),
                Err(error) => (Vec::new(), Some(error)),
            }
        } else {
            (Vec::new(), None)
        };
        let applications = config
            .compose
            .as_ref()
            .map(|compose| {
                compose
                    .services
                    .iter()
                    .map(|(service, settings)| {
                        let kind = settings.kind();
                        // HTTP apps are ready through their instances. Workers
                        // have no backend pool; their containers report state.
                        let running = match kind {
                            tako_core::compose::ComposeServiceKind::Http => {
                                app.get_instances().iter().any(|i| {
                                    i.compose_service().as_ref() == Some(service)
                                        && i.state() == tako_core::InstanceState::Healthy
                                })
                            }
                            tako_core::compose::ComposeServiceKind::Worker => worker_running(
                                services.iter().filter(|s| &s.service == service),
                                app.compose_desired_replicas(service)
                                    .or_else(|| config.compose_replicas.get(service).copied()),
                            ),
                        };
                        tako_core::ComposeApplicationStatus {
                            name: format!("{}/{}", app.name(), service),
                            service: service.clone(),
                            kind,
                            sdk: settings.sdk,
                            state: if config.stopped || config.compose_stopped.contains(service) {
                                tako_core::AppState::Stopped
                            } else if running {
                                tako_core::AppState::Running
                            } else {
                                match app.state() {
                                    tako_core::AppState::Running => tako_core::AppState::Error,
                                    state => state,
                                }
                            },
                            routes: settings
                                .envs
                                .get(&config.environment)
                                .map(|e| e.routes())
                                .unwrap_or_default(),
                            instances: app
                                .get_instances()
                                .iter()
                                .filter(|i| i.compose_service().as_ref() == Some(service))
                                .map(|i| i.status())
                                .collect(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let status = AppStatus {
            kind,
            applications,
            replica_overrides: config.compose_replicas.clone(),
            services,
            services_error,
            name: app.name(),
            version: app.version(),
            instances,
            builds,
            state: app.state(),
            last_error: app.last_error(),
            recovery: app.recovery_status(),
        };

        Response::ok(status)
    }

    pub(crate) async fn list_apps(&self) -> Response {
        let mut apps: Vec<serde_json::Value> = self
            .app_manager
            .list_apps()
            .iter()
            .filter_map(|name| {
                self.app_manager.get_app(name).map(|app| {
                    serde_json::json!({
                        "name": app.name(),
                        "kind": if app.config.read().compose.is_some() { "application_group" } else { "application" },
                        "version": app.version(),
                        "state": app.state(),
                        "instances": app.get_instances().len()
                    })
                })
            })
            .collect();
        // Apps a restore left unregistered, so they are not missing from status.
        let mut failed: Vec<_> = self
            .restore_failures
            .read()
            .iter()
            .filter(|(name, _)| self.app_manager.get_app(name).is_none())
            .map(|(name, failure)| (name.clone(), failure.clone()))
            .collect();
        failed.sort_by(|a, b| a.0.cmp(&b.0));
        apps.extend(failed.into_iter().map(|(name, failure)| {
            serde_json::json!({
                "name": name,
                "kind": if failure.compose { "application_group" } else { "application" },
                "version": failure.version,
                "state": tako_core::AppState::Error,
                "instances": 0
            })
        }));

        Response::ok(serde_json::json!({ "apps": apps }))
    }

    pub(crate) async fn list_routes(&self) -> Response {
        let route_table = self.routes.read();
        let routes: Vec<serde_json::Value> = self
            .app_manager
            .list_apps()
            .iter()
            .map(|app| {
                let patterns = route_table.routes_for_app(app);
                serde_json::json!({ "app": app, "routes": patterns })
            })
            .collect();
        Response::ok(serde_json::json!({ "routes": routes }))
    }
}

/// The status of an app a restore could not register: it has no instances,
/// routes or builds, only the error that kept it from starting.
fn restore_failure_status(
    app_name: &str,
    failure: &crate::server_state::RestoreFailure,
) -> AppStatus {
    AppStatus {
        kind: if failure.compose {
            tako_core::ApplicationKind::ApplicationGroup
        } else {
            tako_core::ApplicationKind::Application
        },
        applications: Vec::new(),
        replica_overrides: Default::default(),
        services: Vec::new(),
        services_error: None,
        name: app_name.to_string(),
        version: failure.version.clone(),
        instances: Vec::new(),
        builds: Vec::new(),
        state: tako_core::AppState::Error,
        last_error: Some(failure.error.clone()),
        recovery: None,
    }
}

/// A worker has no backend pool: it runs when every container is running and
/// healthy, or finished successfully. A worker with no containers runs as
/// desired only when the rollout planned zero replicas for it.
fn worker_running<'a>(
    mut containers: impl Iterator<Item = &'a tako_core::ComposeServiceStatus> + Clone,
    desired_replicas: Option<u32>,
) -> bool {
    (containers.clone().next().is_some() || desired_replicas == Some(0))
        && containers.all(|s| {
            (s.state == "running" && (s.health.is_empty() || s.health == "healthy"))
                || (s.state == "exited" && s.exit_code == 0)
        })
}

#[cfg(test)]
mod tests {
    use super::worker_running;
    use tako_core::ComposeServiceStatus;

    fn container(state: &str, health: &str, exit_code: i64) -> ComposeServiceStatus {
        ComposeServiceStatus {
            service: "worker".into(),
            state: state.into(),
            health: health.into(),
            exit_code,
        }
    }

    #[test]
    fn a_worker_planned_with_zero_replicas_runs_without_containers() {
        assert!(worker_running(std::iter::empty(), Some(0)));
        assert!(!worker_running(std::iter::empty(), Some(2)));
        assert!(!worker_running(std::iter::empty(), None));
    }

    #[test]
    fn a_worker_runs_only_while_every_container_is_healthy_or_completed() {
        let healthy = [container("running", "", 0), container("exited", "", 0)];
        assert!(worker_running(healthy.iter(), Some(2)));
        let failed = [
            container("running", "healthy", 0),
            container("exited", "", 1),
        ];
        assert!(!worker_running(failed.iter(), Some(2)));
        let starting = [container("running", "starting", 0)];
        assert!(!worker_running(starting.iter(), Some(1)));
    }
}

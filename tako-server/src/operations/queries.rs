use crate::release::collect_running_build_statuses;
use crate::socket::{AppStatus, InstanceStatus, Response};

impl crate::ServerState {
    pub(crate) async fn get_status(&self, app_name: &str) -> Response {
        let app = match self.app_manager.get_app(app_name) {
            Some(app) => app,
            None => return Response::error(format!("App not found: {}", app_name)),
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
        let applications = config
            .compose
            .as_ref()
            .map(|compose| {
                compose
                    .services
                    .iter()
                    .map(|(service, settings)| tako_core::ComposeApplicationStatus {
                        name: format!("{}/{}", app.name(), service),
                        service: service.clone(),
                        state: if config.stopped || config.compose_stopped.contains(service) {
                            tako_core::AppState::Stopped
                        } else if app.get_instances().iter().any(|i| {
                            i.compose_service().as_ref() == Some(service)
                                && i.state() == tako_core::InstanceState::Healthy
                        }) {
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
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (services, services_error) = if let Some(compose) = config.compose {
            let mut env = config.env_vars.clone();
            env.extend(config.secrets);
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
            let result =
                match crate::compose_runtime::ComposeRuntime::new(config.path, compose, env) {
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
        let apps: Vec<serde_json::Value> = self
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

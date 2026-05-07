use super::time::format_duration_human;
use super::{GlobalAppStatusResult, GlobalServerStatusResult, ServerStatusResult};
use crate::management_http::{self, ManagementClient};
use crate::output;
use tako_core::{AppStatus, Command, ListReleasesResponse, ReleaseInfo, Response, UpgradeMode};
use time::OffsetDateTime;

#[cfg(test)]
use crate::config::TakoToml;

pub(super) async fn query_global_server_status(
    _server_name: &str,
    host: &str,
    _port: u16,
) -> GlobalServerStatusResult {
    let _t = output::timed("Query status");
    let probe = match management_http::probe(host).await {
        Ok(probe) => probe,
        Err(error) => {
            return status_error(format!("Remote management failed: {error}"));
        }
    };
    let mut client = match ManagementClient::new(host).await {
        Ok(client) => client,
        Err(error) => {
            return status_error(format!("Remote management auth failed: {error}"));
        }
    };
    let info = match client.send(&Command::ServerInfo).await {
        Ok(response) => {
            match management_http::parse_ok_data::<tako_core::ServerRuntimeInfo>(
                response,
                "server_info",
            ) {
                Ok(info) => info,
                Err(error) => {
                    return status_error(format!("Remote management failed: {error}"));
                }
            }
        }
        Err(error) => {
            return status_error(format!("Remote management failed: {error}"));
        }
    };

    let service_status = match info.mode {
        UpgradeMode::Normal => "active".to_string(),
        UpgradeMode::Upgrading => "upgrading".to_string(),
    };
    let server_version = Some(normalize_server_version(probe.hello.server_version));
    let process_uptime = info
        .process_started_at_unix_secs
        .and_then(process_uptime_since);
    let routes = fetch_routes(&mut client).await;

    let mut effective_service_status = service_status.clone();
    let mut apps = Vec::new();
    let mut error = None;

    if service_status == "active" || service_status == "unknown" {
        match client.send(&Command::List).await {
            Ok(response) => match parse_list_apps_response(response) {
                Ok(app_names) => {
                    tracing::debug!("Found {} app(s)", app_names.len());
                    if service_status == "unknown" {
                        effective_service_status = "active".to_string();
                    }

                    for remote_app_name in app_names {
                        let (display_app_name, env_from_name) =
                            parse_remote_app_name(&remote_app_name);
                        let status = query_connected_app_status(
                            &mut client,
                            &effective_service_status,
                            server_version.clone(),
                            &remote_app_name,
                        )
                        .await;
                        for mut build_status in expand_status_by_running_builds(status) {
                            let app_version = build_status
                                .app_status
                                .as_ref()
                                .map(|app| app.version.clone());

                            let env_name = if let Some(app_version) = app_version {
                                build_status.deployed_at_unix_secs = fetch_app_deployed_at(
                                    &mut client,
                                    &remote_app_name,
                                    &app_version,
                                )
                                .await;
                                env_from_name
                                    .clone()
                                    .unwrap_or_else(|| "unknown".to_string())
                            } else {
                                env_from_name
                                    .clone()
                                    .unwrap_or_else(|| "unknown".to_string())
                            };

                            apps.push(GlobalAppStatusResult {
                                app_name: display_app_name.clone(),
                                env_name,
                                status: build_status,
                            });
                        }
                    }
                }
                Err(e) => {
                    error = Some(e);
                }
            },
            Err(e) => {
                error = Some(format!("Remote management query failed: {}", e));
            }
        }
    }

    GlobalServerStatusResult {
        service_status: effective_service_status,
        server_version,
        server_uptime: None,
        process_uptime,
        routes,
        apps,
        error,
    }
}

fn status_error(message: String) -> GlobalServerStatusResult {
    GlobalServerStatusResult {
        service_status: "unknown".to_string(),
        server_version: None,
        server_uptime: None,
        process_uptime: None,
        routes: Vec::new(),
        apps: Vec::new(),
        error: Some(message),
    }
}

fn process_uptime_since(epoch: i64) -> Option<String> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let secs = (now - epoch).max(0) as u64;
    Some(format_duration_human(secs))
}

async fn fetch_routes(client: &mut ManagementClient) -> Vec<(String, String)> {
    let Ok(response) = client.send(&Command::Routes).await else {
        return Vec::new();
    };

    match response {
        Response::Ok { data } => {
            let mut result = Vec::new();
            if let Some(routes) = data.get("routes").and_then(|v| v.as_array()) {
                for entry in routes {
                    let app = entry
                        .get("app")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let app = format_remote_app_label(app);
                    if let Some(patterns) = entry.get("routes").and_then(|v| v.as_array()) {
                        for pattern in patterns {
                            if let Some(p) = pattern.as_str() {
                                result.push((app.clone(), p.to_string()));
                            }
                        }
                    }
                }
            }
            result
        }
        Response::Error { .. } => Vec::new(),
    }
}

pub(super) fn sort_global_apps(apps: &mut [GlobalAppStatusResult]) {
    apps.sort_by(|a, b| {
        let a_version = a
            .status
            .app_status
            .as_ref()
            .map(|s| s.version.as_str())
            .unwrap_or_default();
        let b_version = b
            .status
            .app_status
            .as_ref()
            .map(|s| s.version.as_str())
            .unwrap_or_default();
        (&a.app_name, &a.env_name, a_version).cmp(&(&b.app_name, &b.env_name, b_version))
    });
}

pub(super) fn parse_remote_app_name(app_name: &str) -> (String, Option<String>) {
    match tako_core::split_deployment_app_id(app_name) {
        Some((base_app_name, env_name)) => (base_app_name.to_string(), Some(env_name.to_string())),
        None => (app_name.to_string(), None),
    }
}

pub(super) fn format_remote_app_label(app_name: &str) -> String {
    let (base_app_name, env_name) = parse_remote_app_name(app_name);
    match env_name {
        Some(env_name) => format!("{base_app_name} ({env_name})"),
        None => base_app_name,
    }
}

async fn query_connected_app_status(
    client: &mut ManagementClient,
    service_status: &str,
    server_version: Option<String>,
    app_name: &str,
) -> ServerStatusResult {
    let mut app_status = None;
    let deployed_at_unix_secs = None;
    let mut error = None;

    if service_status == "active" {
        match client
            .send(&Command::Status {
                app: app_name.to_string(),
            })
            .await
        {
            Ok(Response::Ok { data }) => match serde_json::from_value::<AppStatus>(data) {
                Ok(status) => {
                    app_status = Some(status);
                }
                Err(e) => {
                    error = Some(format!("Failed to parse app status: {}", e));
                }
            },
            Ok(Response::Error { message }) => {
                if !message.contains("not found") {
                    error = Some(message);
                }
            }
            Err(e) => {
                error = Some(format!("Remote management query failed: {}", e));
            }
        }
    }

    ServerStatusResult {
        service_status: service_status.to_string(),
        server_version,
        app_status,
        deployed_at_unix_secs,
        error,
    }
}

async fn fetch_app_deployed_at(
    client: &mut ManagementClient,
    app_name: &str,
    version: &str,
) -> Option<i64> {
    let response = client
        .send(&Command::ListReleases {
            app: app_name.to_string(),
        })
        .await
        .ok()?;
    let releases = parse_list_releases_response(response).ok()?;
    releases
        .into_iter()
        .find(|release| release.version == version)
        .and_then(|release| release.deployed_at_unix_secs)
}

pub(super) fn expand_status_by_running_builds(
    status: ServerStatusResult,
) -> Vec<ServerStatusResult> {
    let Some(app_status) = status.app_status.as_ref() else {
        return vec![status];
    };

    if app_status.builds.is_empty() {
        return vec![status];
    }

    let mut per_build = Vec::new();
    for build in &app_status.builds {
        if build.instances.is_empty() {
            continue;
        }

        per_build.push(ServerStatusResult {
            service_status: status.service_status.clone(),
            server_version: status.server_version.clone(),
            app_status: Some(AppStatus {
                name: app_status.name.clone(),
                version: build.version.clone(),
                instances: build.instances.clone(),
                builds: Vec::new(),
                state: build.state,
                last_error: app_status.last_error.clone(),
            }),
            deployed_at_unix_secs: status.deployed_at_unix_secs,
            error: status.error.clone(),
        });
    }

    if per_build.is_empty() {
        vec![status]
    } else {
        per_build
    }
}

pub(super) fn parse_list_apps_response(response: Response) -> Result<Vec<String>, String> {
    match response {
        Response::Ok { data } => {
            let mut names = data
                .get("apps")
                .and_then(|value| value.as_array())
                .map(|apps| {
                    apps.iter()
                        .filter_map(|app| {
                            app.get("name")
                                .and_then(|name| name.as_str())
                                .map(|name| name.to_string())
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            names.sort();
            names.dedup();
            Ok(names)
        }
        Response::Error { message } => Err(format!("tako-server error (list): {}", message)),
    }
}

fn parse_list_releases_response(response: Response) -> Result<Vec<ReleaseInfo>, String> {
    match response {
        Response::Ok { data } => {
            let parsed: ListReleasesResponse = serde_json::from_value(data)
                .map_err(|e| format!("invalid list_releases response: {}", e))?;
            Ok(parsed.releases)
        }
        Response::Error { message } => Err(format!("tako-server error (list_releases): {message}")),
    }
}

#[cfg(test)]
pub(super) fn parse_server_env_from_tako_toml(content: &str, server_name: &str) -> Option<String> {
    let config = TakoToml::parse(content).ok()?;

    let mut matching_envs = Vec::new();
    let mut configured_envs = Vec::new();
    for (env_name, env_config) in &config.envs {
        if env_name == "development" {
            continue;
        }

        if env_config.servers.iter().any(|name| name == server_name) {
            matching_envs.push(env_name.clone());
        }
        if !env_config.servers.is_empty() {
            configured_envs.push(env_name.clone());
        }
    }
    matching_envs.sort();
    matching_envs.dedup();
    if matching_envs.len() == 1 {
        return matching_envs.into_iter().next();
    }

    configured_envs.sort();
    configured_envs.dedup();
    if configured_envs.len() == 1 {
        configured_envs.into_iter().next()
    } else {
        None
    }
}

pub(super) fn normalize_server_version(raw: String) -> String {
    raw.trim()
        .strip_prefix("tako-server ")
        .unwrap_or(raw.trim())
        .to_string()
}

pub(super) fn display_server_version(version: &str) -> String {
    if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{}", version)
    }
}

use std::collections::BTreeSet;
use std::path::Path;

use crate::app::require_app_name_from_config_path;
use crate::commands::project_context;
use crate::config::{ServerEntry, ServersToml, TakoToml};
use crate::management_http::ManagementClient;
use crate::output;
use tako_core::{Command, Response};
use tracing::Instrument;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RemoteDeployment {
    remote_app_id: String,
    app: String,
    env: String,
    server_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeleteSelectionOptions {
    title: String,
    description: String,
    choices: Vec<(String, RemoteDeployment)>,
}

pub fn run(
    env: Option<&str>,
    server: Option<&str>,
    assume_yes: bool,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(env, server, assume_yes, config_path))
}

async fn run_async(
    requested_env: Option<&str>,
    requested_server: Option<&str>,
    assume_yes: bool,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let interactive = output::is_interactive();

    let project_context = project_context::resolve_optional(config_path)?;
    let project_tako = if let Some(context) = project_context.as_ref() {
        Some(TakoToml::load_from_file(&context.config_path)?)
    } else {
        None
    };
    let project_app = if let Some(context) = project_context.as_ref() {
        Some(
            require_app_name_from_config_path(&context.config_path).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
            })?,
        )
    } else {
        None
    };

    let servers = ServersToml::load()?;

    validate_confirmation_mode(assume_yes, interactive)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    validate_target_flags_for_mode(requested_env, requested_server, interactive)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    if let Some(env) = requested_env {
        if let Some(tako_config) = project_tako.as_ref() {
            validate_project_delete_env(env, tako_config)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        } else {
            validate_non_project_delete_env(env)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        }
    }

    if let Some(server_name) = requested_server {
        validate_delete_server(server_name, &servers)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    }

    let target = if let (Some(app_name), Some(env), Some(server_name)) =
        (project_app.as_deref(), requested_env, requested_server)
    {
        RemoteDeployment {
            remote_app_id: tako_core::deployment_app_id(app_name, env),
            app: app_name.to_string(),
            env: env.to_string(),
            server_name: server_name.to_string(),
        }
    } else {
        let deployments = discover_remote_deployments_with_progress(&servers).await?;
        if deployments.is_empty() {
            return Err("No deployed apps found on configured servers.".into());
        }

        resolve_delete_target_from_candidates(
            &deployments,
            project_app.as_deref(),
            requested_env,
            requested_server,
            interactive,
        )
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
    };

    if should_confirm_delete(assume_yes, interactive) {
        let prompt = format_delete_confirm_prompt(&target.app, &target.env, &target.server_name);
        let description = format_delete_confirm_hint(&target.app, &target.server_name);
        let confirmed = output::confirm_with_description(&prompt, Some(&description), false)?;
        if !confirmed {
            return Err(output::operation_cancelled_error().into());
        }
    }

    let server = servers
        .get(&target.server_name)
        .ok_or_else(|| format_server_not_found_error(&target.server_name))?;

    output::section("Delete");

    if output::is_dry_run() {
        output::dry_run_skip(&format!(
            "Delete {} from {} on {}",
            output::strong(&target.app),
            output::strong(&target.env),
            output::strong(&target.server_name)
        ));
        return Ok(());
    }

    output::info(&format!(
        "Deleting {} from {} on {}",
        target.app, target.env, target.server_name
    ));

    let span = output::scope(&target.server_name);
    let result: Result<(), String> = if interactive {
        output::with_spinner_async(
            &format!("Deleting from {}", target.server_name),
            &format!("Deleted from {}", target.server_name),
            delete_from_server(server, &target.remote_app_id).instrument(span),
        )
        .await
        .map_err(|e| e.to_string())
    } else {
        delete_from_server(server, &target.remote_app_id)
            .await
            .map_err(|e| e.to_string())
    };

    if let Err(error) = result {
        output::error("Delete");
        output::section("Summary");
        output::error(&format!("{} delete failed: {}", target.server_name, error));
        return Err(format!("Delete failed on {}: {}", target.server_name, error).into());
    }

    if interactive {
        output::success("Delete");
    }
    output::section("Summary");
    output::info(&format!(
        "Deleted {} from {} on {}",
        output::strong(&target.app),
        output::strong(&target.env),
        output::strong(&target.server_name)
    ));
    Ok(())
}

async fn discover_remote_deployments(
    servers: &ServersToml,
) -> Result<Vec<RemoteDeployment>, Box<dyn std::error::Error>> {
    if servers.is_empty() {
        return Err("No servers have been added. Run 'tako servers add <host>' first.".into());
    }

    let mut names: Vec<String> = servers.names().into_iter().map(str::to_string).collect();
    names.sort();

    let mut handles = Vec::new();
    for server_name in names {
        let Some(server) = servers.get(&server_name) else {
            continue;
        };

        let server_name_for_task = server_name.clone();
        let server = server.clone();
        let span = output::scope(&server_name_for_task);
        handles.push(tokio::spawn(
            async move {
                let result = discover_server_deployments(&server_name_for_task, &server).await;
                (server_name_for_task, result)
            }
            .instrument(span),
        ));
    }

    let mut all = Vec::new();
    for handle in handles {
        match handle.await {
            Ok((_server_name, Ok(mut deployments))) => {
                all.append(&mut deployments);
            }
            Ok((server_name, Err(e))) => {
                return Err(
                    format!("Failed to query deployed apps on '{}': {}", server_name, e).into(),
                );
            }
            Err(e) => {
                return Err(format!("Deployment discovery task panic: {}", e).into());
            }
        }
    }

    all.sort();
    all.dedup();
    Ok(all)
}

async fn discover_remote_deployments_with_progress(
    servers: &ServersToml,
) -> Result<Vec<RemoteDeployment>, Box<dyn std::error::Error>> {
    if output::is_interactive() {
        let deployments = output::with_spinner_async(
            "Getting deployment information",
            "Deployment information loaded",
            async {
                discover_remote_deployments(servers)
                    .await
                    .map_err(|e| e.to_string())
            },
        )
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        return Ok(deployments);
    }
    discover_remote_deployments(servers).await
}

async fn discover_server_deployments(
    server_name: &str,
    server: &ServerEntry,
) -> Result<Vec<RemoteDeployment>, Box<dyn std::error::Error + Send + Sync>> {
    let _t = output::timed("Discover deployments");
    let mut client = ManagementClient::new(&server.host).await?;
    let app_names = parse_list_apps_response(client.send(&Command::List).await?)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
    tracing::debug!("Found {} app(s)", app_names.len());

    let mut deployments = Vec::new();
    for app_name in app_names {
        let (app, env) = if let Some((app, env)) = tako_core::split_deployment_app_id(&app_name) {
            (app.to_string(), env.to_string())
        } else {
            (app_name.clone(), "unknown".to_string())
        };
        deployments.push(RemoteDeployment {
            remote_app_id: app_name,
            app,
            env,
            server_name: server_name.to_string(),
        });
    }

    Ok(deployments)
}

fn parse_list_apps_response(response: Response) -> Result<Vec<String>, String> {
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

fn delete_targets(
    deployments: &[RemoteDeployment],
    app_filter: Option<&str>,
    requested_env: Option<&str>,
    requested_server: Option<&str>,
) -> Vec<RemoteDeployment> {
    let mut targets = deployments
        .iter()
        .filter(|deployment| match app_filter {
            Some(app_name) => deployment.app == app_name,
            None => true,
        })
        .filter(|deployment| match requested_env {
            Some(env) => deployment.env == env,
            None => true,
        })
        .filter(|deployment| match requested_server {
            Some(server_name) => deployment.server_name == server_name,
            None => true,
        })
        .cloned()
        .collect::<Vec<_>>();
    targets.sort();
    targets.dedup();
    targets
}

fn delete_target_selection_options(
    targets: &[RemoteDeployment],
    app_known: bool,
    requested_env: Option<&str>,
    requested_server: Option<&str>,
) -> DeleteSelectionOptions {
    let include_app = !app_known && unique_values(targets, |target| target.app.as_str()).len() > 1;

    match (requested_env, requested_server) {
        (None, None) => DeleteSelectionOptions {
            title: "Select deployment to delete".to_string(),
            description: "Choose a deployment target and press Enter.".to_string(),
            choices: targets
                .iter()
                .cloned()
                .map(|target| {
                    let label = if include_app {
                        format!("{} {} from {}", target.app, target.env, target.server_name)
                    } else {
                        format!("{} from {}", target.env, target.server_name)
                    };
                    (label, target)
                })
                .collect(),
        },
        (Some(_), None) => DeleteSelectionOptions {
            title: if include_app {
                "Select deployment to delete".to_string()
            } else {
                "Select server to delete from".to_string()
            },
            description: "Choose a server and press Enter.".to_string(),
            choices: targets
                .iter()
                .cloned()
                .map(|target| {
                    let label = if include_app {
                        format!("{} from {}", target.app, target.server_name)
                    } else {
                        target.server_name.clone()
                    };
                    (label, target)
                })
                .collect(),
        },
        (None, Some(_)) => DeleteSelectionOptions {
            title: if include_app {
                "Select deployment to delete".to_string()
            } else {
                "Select environment to delete".to_string()
            },
            description: "Choose an environment and press Enter.".to_string(),
            choices: targets
                .iter()
                .cloned()
                .map(|target| {
                    let label = if include_app {
                        format!("{} in {}", target.app, target.env)
                    } else {
                        target.env.clone()
                    };
                    (label, target)
                })
                .collect(),
        },
        (Some(_), Some(_)) => DeleteSelectionOptions {
            title: "Select app to delete".to_string(),
            description: "Choose an app and press Enter.".to_string(),
            choices: targets
                .iter()
                .cloned()
                .map(|target| (target.app.clone(), target))
                .collect(),
        },
    }
}

fn resolve_delete_target_from_candidates(
    deployments: &[RemoteDeployment],
    app_filter: Option<&str>,
    requested_env: Option<&str>,
    requested_server: Option<&str>,
    interactive: bool,
) -> Result<RemoteDeployment, String> {
    let targets = delete_targets(deployments, app_filter, requested_env, requested_server);

    match targets.as_slice() {
        [] => Err(format_no_delete_targets_error(
            app_filter,
            requested_env,
            requested_server,
        )),
        [target] => Ok(target.clone()),
        _ if !interactive => Err(format_ambiguous_delete_targets_error(
            app_filter,
            requested_env,
            requested_server,
        )),
        _ => {
            let selection = delete_target_selection_options(
                &targets,
                app_filter.is_some(),
                requested_env,
                requested_server,
            );
            output::select(
                &selection.title,
                Some(selection.description.as_str()),
                selection.choices,
            )
            .map_err(|e| format!("Failed to read selection: {e}"))
        }
    }
}

fn validate_project_delete_env(env: &str, tako_config: &TakoToml) -> Result<String, String> {
    validate_non_project_delete_env(env)?;
    if !tako_config.envs.contains_key(env) {
        let available = available_environment_names(tako_config);
        let available_text = if available.is_empty() {
            "(none)".to_string()
        } else {
            available.join(", ")
        };
        return Err(format!(
            "Environment '{}' not found. Available: {}",
            env, available_text
        ));
    }
    Ok(env.to_string())
}

fn validate_non_project_delete_env(env: &str) -> Result<(), String> {
    if env == "development" {
        return Err(
            "Environment 'development' is reserved for local development and cannot be deleted."
                .to_string(),
        );
    }
    Ok(())
}

fn available_environment_names(tako_config: &TakoToml) -> Vec<String> {
    let mut names: Vec<String> = tako_config.envs.keys().cloned().collect();
    names.sort();
    names
}

fn unique_values<'a, F>(targets: &'a [RemoteDeployment], select: F) -> BTreeSet<&'a str>
where
    F: Fn(&'a RemoteDeployment) -> &'a str,
{
    targets.iter().map(select).collect()
}

fn format_no_delete_targets_error(
    app_filter: Option<&str>,
    requested_env: Option<&str>,
    requested_server: Option<&str>,
) -> String {
    match (app_filter, requested_env, requested_server) {
        (Some(app_name), Some(env), Some(server_name)) => format!(
            "App '{}' is not deployed to environment '{}' on server '{}'.",
            app_name, env, server_name
        ),
        (Some(app_name), Some(env), None) => format!(
            "App '{}' is not deployed to environment '{}' on configured servers.",
            app_name, env
        ),
        (Some(app_name), None, Some(server_name)) => format!(
            "App '{}' is not deployed on server '{}'.",
            app_name, server_name
        ),
        (Some(app_name), None, None) => format!(
            "No deployed environments found for app '{}' on configured servers.",
            app_name
        ),
        (None, Some(env), Some(server_name)) => format!(
            "No deployed apps found for environment '{}' on server '{}'.",
            env, server_name
        ),
        (None, Some(env), None) => format!(
            "No deployed apps found for environment '{}' on configured servers.",
            env
        ),
        (None, None, Some(server_name)) => {
            format!("No deployed apps found on server '{}'.", server_name)
        }
        (None, None, None) => "No deployed apps found on configured servers.".to_string(),
    }
}

fn format_ambiguous_delete_targets_error(
    app_filter: Option<&str>,
    requested_env: Option<&str>,
    requested_server: Option<&str>,
) -> String {
    match (app_filter, requested_env, requested_server) {
        (Some(app_name), Some(env), None) => format!(
            "Multiple deployments match app '{}' in environment '{}'. Re-run interactively or pass --server.",
            app_name, env
        ),
        (Some(app_name), None, Some(server_name)) => format!(
            "Multiple deployments match app '{}' on server '{}'. Re-run interactively or pass --env.",
            app_name, server_name
        ),
        (Some(app_name), None, None) => format!(
            "Multiple deployments match app '{}'. Re-run interactively or pass --env/--server.",
            app_name
        ),
        (None, Some(env), Some(server_name)) => format!(
            "Multiple deployments match environment '{}' on server '{}'. Re-run interactively from the app directory to choose one.",
            env, server_name
        ),
        (None, Some(env), None) => format!(
            "Multiple deployments match environment '{}'. Re-run interactively or run from the app directory to choose one.",
            env
        ),
        (None, None, Some(server_name)) => format!(
            "Multiple deployments match server '{}'. Re-run interactively or run from the app directory to choose one.",
            server_name
        ),
        (None, None, None) => {
            "Multiple deployments match. Re-run interactively to choose a target.".to_string()
        }
        (Some(app_name), Some(env), Some(server_name)) => format!(
            "Multiple deployments match app '{}' in environment '{}' on server '{}'. Re-run interactively to choose one.",
            app_name, env, server_name
        ),
    }
}

fn validate_delete_server(server_name: &str, servers: &ServersToml) -> Result<(), String> {
    if servers.get(server_name).is_none() {
        return Err(format_server_not_found_error(server_name));
    }
    Ok(())
}

fn validate_confirmation_mode(assume_yes: bool, interactive: bool) -> Result<(), String> {
    if !assume_yes && !interactive {
        return Err(
            "Delete requires --yes in non-interactive mode to avoid accidental removal."
                .to_string(),
        );
    }
    Ok(())
}

fn validate_target_flags_for_mode(
    requested_env: Option<&str>,
    requested_server: Option<&str>,
    interactive: bool,
) -> Result<(), String> {
    if interactive {
        return Ok(());
    }

    if requested_env.is_none() || requested_server.is_none() {
        return Err("Delete requires both --env and --server in non-interactive mode.".to_string());
    }

    Ok(())
}

fn should_confirm_delete(assume_yes: bool, interactive: bool) -> bool {
    !assume_yes && interactive
}

async fn delete_from_server(
    server: &ServerEntry,
    remote_app_name: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _t = output::timed(&format!("Delete app {remote_app_name}"));
    let mut client = ManagementClient::new(&server.host).await?;
    parse_delete_response(
        client
            .send(&Command::Delete {
                app: remote_app_name.to_string(),
            })
            .await?,
    )
    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
    tracing::debug!("Delete command succeeded for {remote_app_name}");
    Ok(())
}

fn parse_delete_response(response: Response) -> Result<(), String> {
    match response {
        Response::Ok { .. } => Ok(()),
        Response::Error { message } => Err(format!("tako-server error (delete): {}", message)),
    }
}

fn format_delete_confirm_prompt(app_name: &str, env: &str, server_name: &str) -> String {
    format!(
        "Please confirm you want to remove application {} from {} on {}.",
        output::strong(app_name),
        output::strong(env),
        output::strong(server_name)
    )
}

fn format_delete_confirm_hint(app_name: &str, server_name: &str) -> String {
    format!(
        "This removes application {} from {}.",
        output::strong(app_name),
        output::strong(server_name)
    )
}

fn format_server_not_found_error(server_name: &str) -> String {
    format!(
        "Server '{}' not found in config.toml [[servers]]. Run 'tako servers add --name {} <host>'.",
        server_name, server_name
    )
}

#[cfg(test)]
mod tests;

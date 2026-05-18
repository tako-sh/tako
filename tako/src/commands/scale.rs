use std::path::Path;

use crate::app::require_app_name_from_config_path;
use crate::commands::helpers::{resolve_servers_for_env, validate_server_names};
use crate::commands::project_context;
use crate::config::{ServerEntry, ServersToml, TakoToml};
use crate::management_http::ManagementClient;
use crate::output;
use tako_core::{Command, Response};
use tracing::Instrument;

#[derive(Debug, Clone)]
struct ScaleTarget {
    display_app_name: String,
    remote_app_name: String,
    env_name: Option<String>,
}

pub fn run(
    instances: u8,
    env: Option<&str>,
    server: Option<&str>,
    app: Option<&str>,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(instances, env, server, app, config_path))
}

async fn run_async(
    instances: u8,
    env: Option<&str>,
    server: Option<&str>,
    app: Option<&str>,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let project_context = project_context::resolve_optional(config_path)?;
    let project_tako = if let Some(context) = project_context.as_ref() {
        Some(TakoToml::load_from_file(&context.config_path)?)
    } else {
        None
    };
    let config_path = project_context
        .as_ref()
        .map(|context| context.config_path.as_path());

    let target = resolve_scale_target(project_tako.as_ref(), config_path, app, env, server)?;
    let servers = ServersToml::load()?;
    let server_names = resolve_scale_server_names(
        project_tako.as_ref(),
        &servers,
        target.env_name.as_deref(),
        server,
    )?;

    output::section("Scale");
    if let Some(env_name) = target.env_name.as_deref() {
        output::info(&format!(
            "{} ({}) -> {instances} instance(s)",
            target.display_app_name, env_name
        ));
    } else {
        output::info(&format!(
            "{} -> {instances} instance(s)",
            target.display_app_name
        ));
    }

    let mut tasks = Vec::new();
    for server_name in &server_names {
        let Some(entry) = servers.get(server_name) else {
            return Err(format!("Server '{}' not found in config.toml", server_name).into());
        };
        let server_name = server_name.clone();
        let entry = entry.clone();
        let app_name = target.remote_app_name.clone();
        let span = output::scope(&server_name);
        tasks.push(tokio::spawn(
            async move {
                let result = scale_server(&app_name, &entry, instances).await;
                (server_name, result)
            }
            .instrument(span),
        ));
    }

    let results = if output::is_interactive() && tasks.len() > 1 {
        output::with_spinner_async_simple(&format!("Scaling {} server(s)", tasks.len()), async {
            let mut results = Vec::new();
            for task in tasks {
                results.push(task.await);
            }
            results
        })
        .await
    } else {
        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await);
        }
        results
    };

    let mut failures = Vec::new();
    for result in results {
        match result {
            Ok((server_name, Ok(scale_result))) => {
                output::bullet(&format!(
                    "{server_name}: {} instance(s)",
                    scale_result.instances
                ));
                if scale_result.standby_limited {
                    output::warning(&format!(
                        "{server_name}: standby mode limited scale to {} instance(s)",
                        scale_result.instances
                    ));
                }
            }
            Ok((server_name, Err(error))) => {
                output::error(&format!("{server_name}: {error}"));
                failures.push(server_name);
            }
            Err(error) => {
                output::error(&format!("Scale task failed: {}", error));
                failures.push("<task>".to_string());
            }
        }
    }

    if failures.is_empty() {
        output::success("Scale");
        Ok(())
    } else {
        Err(format!("Failed to scale {} server(s)", failures.len()).into())
    }
}

fn resolve_scale_target(
    project_tako: Option<&TakoToml>,
    config_path: Option<&std::path::Path>,
    explicit_app: Option<&str>,
    env: Option<&str>,
    server: Option<&str>,
) -> Result<ScaleTarget, Box<dyn std::error::Error>> {
    if let Some(tako_config) = project_tako {
        let config_path = config_path.expect("config path must exist when config is loaded");
        let resolved = require_app_name_from_config_path(config_path).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
        })?;
        if let Some(app_name) = explicit_app
            && app_name != resolved
        {
            return Err(format!(
                "--app '{}' does not match project app '{}'",
                app_name, resolved
            )
            .into());
        }
        let env_name = match (env, server) {
            (Some(env_name), _) => Some(env_name.to_string()),
            (None, Some(_)) => Some(super::helpers::resolve_env(None)),
            (None, None) => None,
        };
        if let Some(env_name) = env_name.as_deref()
            && !tako_config.envs.contains_key(env_name)
        {
            return Err(format!("Environment '{}' not found in tako.toml.", env_name).into());
        }
        let remote_app_name = env_name
            .as_deref()
            .map(|env_name| tako_core::deployment_app_id(&resolved, env_name))
            .unwrap_or_else(|| resolved.clone());
        return Ok(ScaleTarget {
            display_app_name: resolved,
            remote_app_name,
            env_name,
        });
    }

    let explicit_app = explicit_app
        .ok_or_else(|| "Run `tako scale` from a project directory or pass --app.".to_string())?;
    let remote_app_name = match env {
        Some(env_name) if tako_core::split_deployment_app_id(explicit_app).is_none() => {
            tako_core::deployment_app_id(explicit_app, env_name)
        }
        _ => explicit_app.to_string(),
    };
    let (display_app_name, env_name) = match tako_core::split_deployment_app_id(&remote_app_name) {
        Some((app_name, env_name)) => (app_name.to_string(), Some(env_name.to_string())),
        None => (explicit_app.to_string(), env.map(str::to_string)),
    };
    Ok(ScaleTarget {
        display_app_name,
        remote_app_name,
        env_name,
    })
}

fn resolve_scale_server_names(
    project_tako: Option<&TakoToml>,
    servers: &ServersToml,
    env: Option<&str>,
    server: Option<&str>,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if let Some(server_name) = server {
        if !servers.contains(server_name) {
            return Err(format!("Server '{}' not found in config.toml", server_name).into());
        }
        if let Some(env_name) = env
            && let Some(tako_config) = project_tako
        {
            if !tako_config.envs.contains_key(env_name) {
                return Err(format!("Environment '{}' not found in tako.toml.", env_name).into());
            }
            if !tako_config
                .get_servers_for_env(env_name)
                .contains(&server_name)
            {
                return Err(format!(
                    "Server '{}' is not configured for environment '{}'.",
                    server_name, env_name
                )
                .into());
            }
        }
        return Ok(vec![server_name.to_string()]);
    }

    let env_name = env.ok_or_else(|| "Pass --env when --server is omitted.".to_string())?;
    let tako_config = project_tako.ok_or_else(|| {
        "Scaling by environment requires project context because environment mappings live in tako.toml."
            .to_string()
    })?;
    if !tako_config.envs.contains_key(env_name) {
        return Err(format!("Environment '{}' not found in tako.toml.", env_name).into());
    }

    let mut names = resolve_servers_for_env(tako_config, servers, env_name)?;
    names.sort();
    names.dedup();
    validate_server_names(&names, servers)?;
    Ok(names)
}

#[derive(Debug)]
struct ScaleResult {
    instances: u8,
    standby_limited: bool,
}

async fn scale_server(
    app_name: &str,
    server: &ServerEntry,
    instances: u8,
) -> Result<ScaleResult, Box<dyn std::error::Error + Send + Sync>> {
    let _t = output::timed(&format!("Scale {app_name} to {instances} instance(s)"));
    let mut client = ManagementClient::new(&server.host).await?;
    let response = client
        .send(&Command::Scale {
            app: app_name.to_string(),
            instances,
        })
        .await?;

    match response {
        Response::Ok { data } => {
            let result = ScaleResult {
                instances: data
                    .get("instances")
                    .and_then(|value| value.as_u64())
                    .and_then(|value| u8::try_from(value).ok())
                    .unwrap_or(instances),
                standby_limited: data
                    .get("standby_limited")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
            };
            tracing::debug!(
                "Scale response: {} instance(s), standby_limited={}",
                result.instances,
                result.standby_limited
            );
            Ok(result)
        }
        Response::Error { message } => Err(message.into()),
    }
}

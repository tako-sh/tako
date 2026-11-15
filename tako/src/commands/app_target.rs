use crate::app::require_app_name_from_config_path;
use crate::commands::helpers::{resolve_servers_for_env, validate_server_names};
use crate::config::{ServerInventory, TakoToml};

#[derive(Debug, Clone)]
pub(super) struct AppTarget {
    pub display_app_name: String,
    pub remote_app_name: String,
    pub env_name: Option<String>,
}

pub(super) fn resolve(
    project_tako: Option<&TakoToml>,
    config_path: Option<&std::path::Path>,
    explicit_app: Option<&str>,
    env: Option<&str>,
    server: Option<&str>,
) -> Result<AppTarget, Box<dyn std::error::Error>> {
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
        return Ok(AppTarget {
            display_app_name: resolved,
            remote_app_name,
            env_name,
        });
    }

    let explicit_app =
        explicit_app.ok_or_else(|| "Run from a project directory or pass --app.".to_string())?;
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
    Ok(AppTarget {
        display_app_name,
        remote_app_name,
        env_name,
    })
}

pub(super) fn server_names(
    project_tako: Option<&TakoToml>,
    servers: &ServerInventory,
    env: Option<&str>,
    server: Option<&str>,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if let Some(server_name) = server {
        if !servers.contains(server_name) {
            return Err(format!(
                "Server '{}' not found in enrolled network membership",
                server_name
            )
            .into());
        }
        if let Some(env_name) = env
            && let Some(tako_config) = project_tako
        {
            if !tako_config.envs.contains_key(env_name) {
                return Err(format!("Environment '{}' not found in tako.toml.", env_name).into());
            }
            if !resolve_servers_for_env(tako_config, servers, env_name)?
                .iter()
                .any(|name| name == server_name)
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
        "Selecting servers by environment requires project context because environment mappings live in tako.toml."
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

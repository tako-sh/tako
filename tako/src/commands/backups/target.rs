use crate::commands::helpers;
use crate::config::{ServersToml, TakoToml};

#[derive(Debug, Clone)]
pub(super) struct BackupTarget {
    pub(super) app_name: String,
    pub(super) env: String,
    pub(super) remote_app_name: String,
    pub(super) server_names: Vec<String>,
}

pub(super) fn resolve_backup_target(
    app_name: &str,
    requested_env: Option<&str>,
    requested_server: Option<&str>,
    tako_config: &TakoToml,
    servers: &ServersToml,
) -> Result<BackupTarget, Box<dyn std::error::Error>> {
    let env = helpers::resolve_env(requested_env);
    if !tako_config.envs.contains_key(&env) {
        return Err(format!("Environment '{}' not found in tako.toml.", env).into());
    }

    let mut server_names = match requested_server {
        Some(server_name) => {
            if !servers.contains(server_name) {
                return Err(format!("Server '{}' not found in config.toml", server_name).into());
            }
            let mapped = tako_config.get_servers_for_env(&env);
            if !mapped.is_empty() && !mapped.contains(&server_name) {
                return Err(format!(
                    "Server '{}' is not configured for environment '{}'.",
                    server_name, env
                )
                .into());
            }
            vec![server_name.to_string()]
        }
        None => helpers::resolve_servers_for_env(tako_config, servers, &env)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?,
    };
    server_names.sort();
    server_names.dedup();
    helpers::validate_server_names(&server_names, servers)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    Ok(BackupTarget {
        app_name: app_name.to_string(),
        env: env.clone(),
        remote_app_name: tako_core::deployment_app_id(app_name, &env),
        server_names,
    })
}

pub(super) fn resolve_single_server(
    server_names: &[String],
    message: &str,
) -> Result<String, String> {
    match server_names {
        [server] => Ok(server.clone()),
        [] => Err("No target servers found.".to_string()),
        _ => Err(message.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_single_server_accepts_only_target() {
        assert_eq!(
            resolve_single_server(&["prod".to_string()], "choose").unwrap(),
            "prod"
        );
    }

    #[test]
    fn resolve_single_server_rejects_multiple_targets() {
        let err = resolve_single_server(&["a".to_string(), "b".to_string()], "choose one")
            .expect_err("should reject");
        assert_eq!(err, "choose one");
    }
}

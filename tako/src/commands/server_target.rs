//! The servers an app environment runs on, and typed management requests to
//! them.

use serde::de::DeserializeOwned;
use tako_core::{Command, Response};

use crate::commands::helpers;
use crate::config::{ServerEntry, ServerInventory, TakoToml};
use crate::management_http::ManagementClient;

#[derive(Debug, Clone)]
pub(crate) struct EnvTarget {
    pub(crate) app_name: String,
    pub(crate) env: String,
    pub(crate) remote_app_name: String,
    pub(crate) server_names: Vec<String>,
}

/// The servers of `app_name`'s environment, or the one server requested.
pub(crate) fn resolve_env_target(
    app_name: &str,
    requested_env: Option<&str>,
    requested_server: Option<&str>,
    tako_config: &TakoToml,
    servers: &ServerInventory,
) -> Result<EnvTarget, Box<dyn std::error::Error>> {
    let env = helpers::resolve_env(requested_env);
    if !tako_config.envs.contains_key(&env) {
        return Err(format!("Environment '{}' not found in tako.toml.", env).into());
    }

    let mut server_names = match requested_server {
        Some(server_name) => {
            if !servers.contains(server_name) {
                return Err(format!(
                    "Server '{}' not found in enrolled network membership",
                    server_name
                )
                .into());
            }
            let mapped = helpers::resolve_servers_for_env(tako_config, servers, &env)?;
            if !mapped.is_empty() && !mapped.iter().any(|name| name == server_name) {
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

    Ok(EnvTarget {
        app_name: app_name.to_string(),
        env: env.clone(),
        remote_app_name: tako_core::deployment_app_id(app_name, &env),
        server_names,
    })
}

pub(crate) async fn send_typed_to_server<T>(
    server: &ServerEntry,
    command: Command,
    response_name: &'static str,
) -> Result<T, String>
where
    T: DeserializeOwned,
{
    let mut client = ManagementClient::for_server(server)
        .await
        .map_err(|e| e.to_string())?;
    parse_typed_response(
        client.send(&command).await.map_err(|e| e.to_string())?,
        response_name,
    )
}

fn parse_typed_response<T>(response: Response, response_name: &str) -> Result<T, String>
where
    T: DeserializeOwned,
{
    match response {
        Response::Ok { data } => serde_json::from_value(data)
            .map_err(|e| format!("invalid {response_name} response: {e}")),
        Response::Error { message, .. } => Err(message),
    }
}

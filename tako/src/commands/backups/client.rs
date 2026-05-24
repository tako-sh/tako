use serde::de::DeserializeOwned;
use tako_core::{Command, Response};
use tracing::Instrument;

use crate::config::{ServerEntry, ServersToml};
use crate::management_http::ManagementClient;
use crate::output;

pub(super) async fn send_typed_to_servers<T>(
    server_names: &[String],
    servers: &ServersToml,
    command: Command,
    response_name: &'static str,
    progress: &str,
) -> Result<Vec<(String, Result<T, String>)>, Box<dyn std::error::Error>>
where
    T: DeserializeOwned + Send + 'static,
{
    let mut tasks = Vec::new();
    for server_name in server_names {
        let Some(server) = servers.get(server_name) else {
            continue;
        };
        let server_name = server_name.clone();
        let server = server.clone();
        let command = command.clone();
        let span = output::scope(&server_name);
        tasks.push(tokio::spawn(
            async move {
                let result = send_typed_to_server::<T>(&server, command, response_name).await;
                (server_name, result)
            }
            .instrument(span),
        ));
    }

    let results = if output::is_interactive() && tasks.len() > 1 {
        output::with_spinner_async_simple(progress, async {
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

    let mut out = Vec::new();
    for result in results {
        match result {
            Ok(value) => out.push(value),
            Err(error) => out.push(("<task>".to_string(), Err(error.to_string()))),
        }
    }
    Ok(out)
}

pub(super) async fn send_typed_to_server<T>(
    server: &ServerEntry,
    command: Command,
    response_name: &'static str,
) -> Result<T, String>
where
    T: DeserializeOwned,
{
    let mut client = ManagementClient::new(&server.host)
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
        Response::Error { message } => Err(message),
    }
}

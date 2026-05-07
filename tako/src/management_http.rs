use std::net::IpAddr;
use std::time::Duration;

use tako_core::{Command, HelloResponse, Response, ServerRuntimeInfo};

pub(crate) const MANAGEMENT_PORT: u16 = 9844;
const MANAGEMENT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagementError {
    #[error("{0}")]
    Message(String),
}

#[derive(Debug, Clone)]
pub(crate) struct ManagementProbe {
    pub(crate) hello: HelloResponse,
    pub(crate) info: ServerRuntimeInfo,
}

pub(crate) fn rpc_url(host: &str) -> String {
    let trimmed = host.trim();
    let literal = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(trimmed);

    if literal
        .parse::<IpAddr>()
        .is_ok_and(|ip| matches!(ip, IpAddr::V6(_)))
    {
        format!("http://[{literal}]:{MANAGEMENT_PORT}/rpc")
    } else {
        format!("http://{trimmed}:{MANAGEMENT_PORT}/rpc")
    }
}

pub(crate) async fn send_command(
    host: &str,
    command: &Command,
) -> Result<Response, ManagementError> {
    let client = reqwest::Client::builder()
        .timeout(MANAGEMENT_TIMEOUT)
        .build()
        .map_err(|error| ManagementError::Message(error.to_string()))?;

    let response = client
        .post(rpc_url(host))
        .json(command)
        .send()
        .await
        .map_err(|error| ManagementError::Message(error.to_string()))?;

    let body = response
        .bytes()
        .await
        .map_err(|error| ManagementError::Message(error.to_string()))?;

    serde_json::from_slice::<Response>(&body).map_err(|error| {
        ManagementError::Message(format!("Remote management returned invalid JSON: {error}"))
    })
}

pub(crate) async fn probe(host: &str) -> Result<ManagementProbe, ManagementError> {
    let hello = send_command(
        host,
        &Command::Hello {
            protocol_version: tako_core::PROTOCOL_VERSION,
        },
    )
    .await?;
    let hello = parse_ok_data::<HelloResponse>(hello, "hello")?;

    let info = send_command(host, &Command::ServerInfo).await?;
    let info = parse_ok_data::<ServerRuntimeInfo>(info, "server_info")?;

    Ok(ManagementProbe { hello, info })
}

fn parse_ok_data<T>(response: Response, context: &str) -> Result<T, ManagementError>
where
    T: serde::de::DeserializeOwned,
{
    match response {
        Response::Ok { data } => serde_json::from_value(data).map_err(|error| {
            ManagementError::Message(format!(
                "Invalid remote management {context} response: {error}"
            ))
        }),
        Response::Error { message } => Err(ManagementError::Message(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_url_brackets_ipv6_literals() {
        assert_eq!(
            rpc_url("prod.tailnet.ts.net"),
            "http://prod.tailnet.ts.net:9844/rpc"
        );
        assert_eq!(rpc_url("100.64.0.10"), "http://100.64.0.10:9844/rpc");
        assert_eq!(
            rpc_url("fd7a:115c:a1e0::1"),
            "http://[fd7a:115c:a1e0::1]:9844/rpc"
        );
    }

    #[test]
    fn parse_ok_data_extracts_typed_data() {
        let response = Response::ok(HelloResponse {
            protocol_version: tako_core::PROTOCOL_VERSION,
            server_version: "0.0.0".to_string(),
            capabilities: vec!["server_runtime_info".to_string()],
            server_identity: Some("SHA256:test".to_string()),
        });

        let parsed: HelloResponse = parse_ok_data(response, "hello").unwrap();

        assert_eq!(parsed.server_identity.as_deref(), Some("SHA256:test"));
    }

    #[test]
    fn parse_ok_data_surfaces_management_errors() {
        let err =
            parse_ok_data::<HelloResponse>(Response::error("management auth required"), "hello")
                .unwrap_err();

        assert!(err.to_string().contains("management auth required"));
    }
}

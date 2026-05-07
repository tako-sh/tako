use std::net::IpAddr;
use std::time::Duration;

use tako_core::{Command, HelloResponse, Response, ServerRuntimeInfo};

mod auth;

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

pub(crate) struct ManagementClient {
    host: String,
    http: reqwest::Client,
    signer: auth::ManagementSigner,
}

impl ManagementClient {
    pub(crate) async fn new(host: &str) -> Result<Self, ManagementError> {
        Ok(Self {
            host: host.to_string(),
            http: http_client()?,
            signer: auth::ManagementSigner::load().await?,
        })
    }

    pub(crate) async fn send(&mut self, command: &Command) -> Result<Response, ManagementError> {
        let body = serde_json::to_vec(command)
            .map_err(|error| ManagementError::Message(error.to_string()))?;
        let signed_headers = self.signer.sign_headers(&body).await?;
        let mut last_auth_error = None;

        for headers in signed_headers {
            let response = self
                .http
                .post(rpc_url(&self.host))
                .header(auth::HEADER_KEY_FINGERPRINT, headers.key_fingerprint)
                .header(auth::HEADER_TIMESTAMP, headers.timestamp)
                .header(auth::HEADER_NONCE, headers.nonce)
                .header(auth::HEADER_SIGNATURE, headers.signature)
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
                .await
                .map_err(|error| ManagementError::Message(error.to_string()))?;

            let parsed = parse_response(response).await?;
            if is_auth_error(&parsed) {
                last_auth_error = parsed.error_message().map(str::to_string);
                continue;
            }
            return Ok(parsed);
        }

        Err(ManagementError::Message(
            last_auth_error.unwrap_or_else(|| "management auth failed".to_string()),
        ))
    }
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
    let client = http_client()?;

    let response = client
        .post(rpc_url(host))
        .json(command)
        .send()
        .await
        .map_err(|error| ManagementError::Message(error.to_string()))?;

    parse_response(response).await
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

fn http_client() -> Result<reqwest::Client, ManagementError> {
    reqwest::Client::builder()
        .timeout(MANAGEMENT_TIMEOUT)
        .build()
        .map_err(|error| ManagementError::Message(error.to_string()))
}

async fn parse_response(response: reqwest::Response) -> Result<Response, ManagementError> {
    let body = response
        .bytes()
        .await
        .map_err(|error| ManagementError::Message(error.to_string()))?;

    serde_json::from_slice::<Response>(&body).map_err(|error| {
        ManagementError::Message(format!("Remote management returned invalid JSON: {error}"))
    })
}

fn is_auth_error(response: &Response) -> bool {
    matches!(
        response.error_message(),
        Some("management auth required" | "management auth failed")
    )
}

pub(crate) fn parse_ok_data<T>(response: Response, context: &str) -> Result<T, ManagementError>
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

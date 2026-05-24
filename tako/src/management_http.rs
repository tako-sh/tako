use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tako_core::{Command, HelloResponse, Response, ServerRuntimeInfo};
use tokio::io::AsyncReadExt;

mod auth;

pub(crate) const MANAGEMENT_PORT: u16 = 9844;
const MANAGEMENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MANAGEMENT_RPC_TIMEOUT: Duration = Duration::from_secs(5);
const MANAGEMENT_DEPLOY_RPC_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MANAGEMENT_UPLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const HEADER_UPLOAD_APP: &str = "x-tako-app";
const HEADER_UPLOAD_VERSION: &str = "x-tako-version";
const HEADER_UPLOAD_SIZE: &str = "x-tako-artifact-size";
const HEADER_UPLOAD_SHA256: &str = "x-tako-artifact-sha256";
const HEADER_LOG_APP: &str = "x-tako-app";
const HEADER_LOG_PREVIOUS_OFFSET: &str = "x-tako-log-previous-offset";
const HEADER_LOG_CURRENT_OFFSET: &str = "x-tako-log-current-offset";
const HEADER_LOG_SINCE_UNIX_SECS: &str = "x-tako-log-since-unix-secs";
const HEADER_LOG_MAX_BYTES: &str = "x-tako-log-max-bytes";
const HEADER_LOG_PREVIOUS_LEN: &str = "x-tako-log-previous-len";
const HEADER_LOG_CURRENT_LEN: &str = "x-tako-log-current-len";
const HEADER_LOG_TRUNCATED: &str = "x-tako-log-truncated";

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
    deploy_http: reqwest::Client,
    upload_http: reqwest::Client,
    signer: auth::ManagementSigner,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LogCursor {
    pub(crate) previous: u64,
    pub(crate) current: u64,
}

#[derive(Debug)]
pub(crate) struct LogFetch {
    pub(crate) bytes: bytes::Bytes,
    pub(crate) cursor: LogCursor,
    pub(crate) previous_len: u64,
    pub(crate) current_len: u64,
    pub(crate) truncated: bool,
}

impl ManagementClient {
    pub(crate) async fn new(host: &str) -> Result<Self, ManagementError> {
        Ok(Self {
            host: host.to_string(),
            http: http_client(MANAGEMENT_RPC_TIMEOUT)?,
            deploy_http: http_client(MANAGEMENT_DEPLOY_RPC_TIMEOUT)?,
            upload_http: http_client(MANAGEMENT_UPLOAD_TIMEOUT)?,
            signer: auth::ManagementSigner::load().await?,
        })
    }

    pub(crate) async fn send(&mut self, command: &Command) -> Result<Response, ManagementError> {
        let body = serde_json::to_vec(command)
            .map_err(|error| ManagementError::Message(error.to_string()))?;
        let signed_headers = self.signer.sign_headers(&body).await?;
        let mut last_auth_error = None;
        let timeout = management_rpc_timeout_for_command(command);
        let http = if timeout == MANAGEMENT_DEPLOY_RPC_TIMEOUT {
            &self.deploy_http
        } else {
            &self.http
        };

        for headers in signed_headers {
            let response = http
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

    pub(crate) async fn upload_release_artifact(
        &mut self,
        app: &str,
        version: &str,
        artifact_path: &Path,
    ) -> Result<Response, ManagementError> {
        let metadata = tokio::fs::metadata(artifact_path)
            .await
            .map_err(|error| ManagementError::Message(error.to_string()))?;
        let size = metadata.len();
        let sha256 = sha256_file_hex(artifact_path).await?;
        let auth_body = tako_core::release_artifact_upload_auth_body(app, version, size, &sha256);
        let signed_headers = self.signer.sign_headers(&auth_body).await?;
        let mut last_auth_error = None;

        for headers in signed_headers {
            let file = tokio::fs::File::open(artifact_path)
                .await
                .map_err(|error| ManagementError::Message(error.to_string()))?;
            let response = self
                .upload_http
                .post(release_artifact_url(&self.host))
                .header(auth::HEADER_KEY_FINGERPRINT, headers.key_fingerprint)
                .header(auth::HEADER_TIMESTAMP, headers.timestamp)
                .header(auth::HEADER_NONCE, headers.nonce)
                .header(auth::HEADER_SIGNATURE, headers.signature)
                .header(HEADER_UPLOAD_APP, app)
                .header(HEADER_UPLOAD_VERSION, version)
                .header(HEADER_UPLOAD_SIZE, size.to_string())
                .header(HEADER_UPLOAD_SHA256, sha256.as_str())
                .header("content-type", "application/zstd")
                .timeout(MANAGEMENT_UPLOAD_TIMEOUT)
                .body(reqwest::Body::from(file))
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

    pub(crate) async fn fetch_log_bytes(
        &mut self,
        app: &str,
        cursor: LogCursor,
        since_unix_secs: Option<i64>,
        max_bytes: usize,
    ) -> Result<LogFetch, ManagementError> {
        let auth_body = tako_core::logs_request_auth_body(
            app,
            cursor.previous,
            cursor.current,
            since_unix_secs,
            max_bytes,
        );
        let signed_headers = self.signer.sign_headers(&auth_body).await?;
        let mut last_auth_error = None;

        for headers in signed_headers {
            let mut request = self
                .http
                .post(logs_url(&self.host))
                .header(auth::HEADER_KEY_FINGERPRINT, headers.key_fingerprint)
                .header(auth::HEADER_TIMESTAMP, headers.timestamp)
                .header(auth::HEADER_NONCE, headers.nonce)
                .header(auth::HEADER_SIGNATURE, headers.signature)
                .header(HEADER_LOG_APP, app)
                .header(HEADER_LOG_PREVIOUS_OFFSET, cursor.previous.to_string())
                .header(HEADER_LOG_CURRENT_OFFSET, cursor.current.to_string())
                .header(HEADER_LOG_MAX_BYTES, max_bytes.to_string());
            if let Some(since_unix_secs) = since_unix_secs {
                request = request.header(HEADER_LOG_SINCE_UNIX_SECS, since_unix_secs.to_string());
            }

            let response = request
                .send()
                .await
                .map_err(|error| ManagementError::Message(error.to_string()))?;
            match parse_log_fetch(response).await {
                Ok(fetch) => return Ok(fetch),
                Err(LogFetchError::Auth(message)) => {
                    last_auth_error = Some(message);
                    continue;
                }
                Err(LogFetchError::Other(error)) => return Err(error),
            }
        }

        Err(ManagementError::Message(
            last_auth_error.unwrap_or_else(|| "management auth failed".to_string()),
        ))
    }
}

pub(crate) fn rpc_url(host: &str) -> String {
    management_url(host, "rpc")
}

pub(crate) fn release_artifact_url(host: &str) -> String {
    management_url(host, "release-artifact")
}

pub(crate) fn logs_url(host: &str) -> String {
    management_url(host, "logs")
}

fn management_url(host: &str, path: &str) -> String {
    let trimmed = host.trim();
    let literal = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(trimmed);

    if literal
        .parse::<IpAddr>()
        .is_ok_and(|ip| matches!(ip, IpAddr::V6(_)))
    {
        format!("http://[{literal}]:{MANAGEMENT_PORT}/{path}")
    } else {
        format!("http://{trimmed}:{MANAGEMENT_PORT}/{path}")
    }
}

pub(crate) async fn send_command(
    host: &str,
    command: &Command,
) -> Result<Response, ManagementError> {
    let client = http_client(MANAGEMENT_RPC_TIMEOUT)?;

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

fn management_rpc_timeout_for_command(command: &Command) -> Duration {
    if matches!(
        command,
        Command::PrepareRelease { .. }
            | Command::CleanupRelease { .. }
            | Command::FinalizeRelease { .. }
            | Command::RunRelease { .. }
            | Command::Deploy { .. }
            | Command::BackupNow { .. }
            | Command::RestoreBackup { .. }
            | Command::Delete { .. }
            | Command::Rollback { .. }
    ) {
        MANAGEMENT_DEPLOY_RPC_TIMEOUT
    } else {
        MANAGEMENT_RPC_TIMEOUT
    }
}

fn http_client(timeout: Duration) -> Result<reqwest::Client, ManagementError> {
    reqwest::Client::builder()
        .connect_timeout(MANAGEMENT_CONNECT_TIMEOUT)
        .timeout(timeout)
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

enum LogFetchError {
    Auth(String),
    Other(ManagementError),
}

async fn parse_log_fetch(response: reqwest::Response) -> Result<LogFetch, LogFetchError> {
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .map_err(|error| LogFetchError::Other(ManagementError::Message(error.to_string())))?;

    if !status.is_success() {
        let parsed = serde_json::from_slice::<Response>(&body).map_err(|error| {
            LogFetchError::Other(ManagementError::Message(format!(
                "Remote management returned invalid error JSON: {error}"
            )))
        })?;
        if is_auth_error(&parsed) {
            return Err(LogFetchError::Auth(
                parsed
                    .error_message()
                    .unwrap_or("management auth failed")
                    .to_string(),
            ));
        }
        return Err(LogFetchError::Other(ManagementError::Message(
            parsed
                .error_message()
                .unwrap_or("remote log request failed")
                .to_string(),
        )));
    }

    Ok(LogFetch {
        bytes: body,
        cursor: LogCursor {
            previous: response_u64_header(&headers, HEADER_LOG_PREVIOUS_OFFSET)?,
            current: response_u64_header(&headers, HEADER_LOG_CURRENT_OFFSET)?,
        },
        previous_len: response_u64_header(&headers, HEADER_LOG_PREVIOUS_LEN)?,
        current_len: response_u64_header(&headers, HEADER_LOG_CURRENT_LEN)?,
        truncated: response_bool_header(&headers, HEADER_LOG_TRUNCATED)?,
    })
}

fn response_u64_header(
    headers: &reqwest::header::HeaderMap,
    name: &'static str,
) -> Result<u64, LogFetchError> {
    let value = headers.get(name).ok_or_else(|| {
        LogFetchError::Other(ManagementError::Message(format!(
            "Remote log response missing {name}"
        )))
    })?;
    let value = value.to_str().map_err(|_| {
        LogFetchError::Other(ManagementError::Message(format!(
            "Remote log response has invalid {name}"
        )))
    })?;
    value.parse::<u64>().map_err(|_| {
        LogFetchError::Other(ManagementError::Message(format!(
            "Remote log response has invalid {name}"
        )))
    })
}

fn response_bool_header(
    headers: &reqwest::header::HeaderMap,
    name: &'static str,
) -> Result<bool, LogFetchError> {
    let value = headers.get(name).ok_or_else(|| {
        LogFetchError::Other(ManagementError::Message(format!(
            "Remote log response missing {name}"
        )))
    })?;
    let value = value.to_str().map_err(|_| {
        LogFetchError::Other(ManagementError::Message(format!(
            "Remote log response has invalid {name}"
        )))
    })?;
    value.parse::<bool>().map_err(|_| {
        LogFetchError::Other(ManagementError::Message(format!(
            "Remote log response has invalid {name}"
        )))
    })
}

fn is_auth_error(response: &Response) -> bool {
    matches!(
        response.error_message(),
        Some("management auth required" | "management auth failed")
    )
}

async fn sha256_file_hex(path: &Path) -> Result<String, ManagementError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| ManagementError::Message(error.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 128 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|error| ManagementError::Message(error.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
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
    fn release_artifact_url_uses_management_port() {
        assert_eq!(
            release_artifact_url("prod.tailnet.ts.net"),
            "http://prod.tailnet.ts.net:9844/release-artifact"
        );
    }

    #[test]
    fn release_artifact_upload_has_room_for_large_archives() {
        assert!(MANAGEMENT_UPLOAD_TIMEOUT > MANAGEMENT_RPC_TIMEOUT);
        assert!(MANAGEMENT_UPLOAD_TIMEOUT >= Duration::from_secs(10 * 60));
    }

    #[test]
    fn release_lifecycle_commands_use_long_rpc_timeout() {
        assert_eq!(
            management_rpc_timeout_for_command(&Command::PrepareRelease {
                app: "demo".to_string(),
                path: "/opt/tako/apps/demo/production/releases/v1".to_string(),
            }),
            MANAGEMENT_DEPLOY_RPC_TIMEOUT
        );
        assert_eq!(
            management_rpc_timeout_for_command(&Command::Deploy {
                app: "demo".to_string(),
                version: "v1".to_string(),
                path: "/opt/tako/apps/demo/production/releases/v1".to_string(),
                routes: vec!["demo.tako.sh".to_string()],
                source_ip: tako_core::SourceIpMode::Direct,
                secrets: None,
                storages: None,
                ssl: tako_core::SslBinding::default(),
                backup: None,
            }),
            MANAGEMENT_DEPLOY_RPC_TIMEOUT
        );
    }

    #[test]
    fn quick_management_commands_keep_short_rpc_timeout() {
        assert_eq!(
            management_rpc_timeout_for_command(&Command::Hello {
                protocol_version: tako_core::PROTOCOL_VERSION,
            }),
            MANAGEMENT_RPC_TIMEOUT
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

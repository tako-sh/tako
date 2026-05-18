use std::convert::Infallible;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::http::StatusCode;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use tako_core::Command;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use crate::ServerState;
use crate::management_auth::{ManagementAuthState, verify_signed_request};

pub(crate) const MANAGEMENT_PORT: u16 = 9844;
const MAX_RPC_BODY_BYTES: usize = 1024 * 1024;
const HEADER_UPLOAD_APP: &str = "x-tako-app";
const HEADER_UPLOAD_VERSION: &str = "x-tako-version";
const HEADER_UPLOAD_SIZE: &str = "x-tako-artifact-size";
const HEADER_UPLOAD_SHA256: &str = "x-tako-artifact-sha256";

type ResponseBody = Full<Bytes>;

pub(crate) async fn serve(
    host: String,
    state: Arc<ServerState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener =
        bind_listener_with_retry(host.as_str(), MANAGEMENT_PORT, Duration::from_millis(250))
            .await?;
    let local_addr = listener.local_addr()?;
    let auth_state = Arc::new(ManagementAuthState::default());
    tracing::info!(%local_addr, "Remote management HTTP listening");

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state = state.clone();
        let auth_state = auth_state.clone();

        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let state = state.clone();
                let auth_state = auth_state.clone();
                async move {
                    Ok::<_, Infallible>(handle_request(request, state, auth_state, peer_addr).await)
                }
            });

            if let Err(error) = http1::Builder::new().serve_connection(io, service).await {
                tracing::debug!(%peer_addr, %error, "Management HTTP connection ended");
            }
        });
    }
}

async fn bind_listener_with_retry(
    host: &str,
    port: u16,
    retry_delay: Duration,
) -> std::io::Result<TcpListener> {
    let mut logged_addr_in_use = false;

    loop {
        match TcpListener::bind((host, port)).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == ErrorKind::AddrInUse => {
                if !logged_addr_in_use {
                    tracing::warn!(
                        host = %host,
                        port,
                        "Remote management HTTP port is still in use; retrying"
                    );
                    logged_addr_in_use = true;
                }
                tokio::time::sleep(retry_delay).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn handle_request(
    request: Request<Incoming>,
    state: Arc<ServerState>,
    auth_state: Arc<ManagementAuthState>,
    peer_addr: SocketAddr,
) -> Response<ResponseBody> {
    if request.method() == Method::POST && request.uri().path() == "/release-artifact" {
        return handle_release_artifact_upload(request, state, auth_state, peer_addr).await;
    }

    if request.method() != Method::POST || request.uri().path() != "/rpc" {
        return json_response(
            StatusCode::NOT_FOUND,
            &serde_json::json!({ "status": "error", "message": "not found" }),
        );
    }

    let (parts, body) = request.into_parts();
    let collected = match Limited::new(body, MAX_RPC_BODY_BYTES).collect().await {
        Ok(collected) => collected,
        Err(error) if error.is::<LengthLimitError>() => {
            return json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &serde_json::json!({ "status": "error", "message": "request body too large" }),
            );
        }
        Err(error) => {
            tracing::debug!(%peer_addr, %error, "Failed to read management RPC body");
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({ "status": "error", "message": "invalid request body" }),
            );
        }
    };
    let body = collected.to_bytes();

    let command = match serde_json::from_slice::<Command>(&body) {
        Ok(command) => command,
        Err(error) => {
            tracing::debug!(%peer_addr, %error, "Invalid management RPC JSON");
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({ "status": "error", "message": "invalid command" }),
            );
        }
    };

    if !command_is_public_probe(&command)
        && let Err(error) = verify_signed_request(
            &state.runtime_config().data_dir,
            &auth_state,
            &parts.headers,
            &body,
        )
    {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &tako_core::Response::error(error.to_string()),
        );
    }

    let response = handle_rpc_command(state, command).await;
    let status = if response.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    json_response(status, &response)
}

async fn handle_release_artifact_upload(
    request: Request<Incoming>,
    state: Arc<ServerState>,
    auth_state: Arc<ManagementAuthState>,
    peer_addr: SocketAddr,
) -> Response<ResponseBody> {
    let (parts, mut body) = request.into_parts();
    let app = match required_header(&parts.headers, HEADER_UPLOAD_APP) {
        Ok(value) => value.to_string(),
        Err(response) => return response,
    };
    let version = match required_header(&parts.headers, HEADER_UPLOAD_VERSION) {
        Ok(value) => value.to_string(),
        Err(response) => return response,
    };
    let expected_size = match required_header(&parts.headers, HEADER_UPLOAD_SIZE)
        .and_then(|value| parse_u64_header(HEADER_UPLOAD_SIZE, value))
    {
        Ok(value) => value,
        Err(response) => return response,
    };
    let expected_sha256 = match required_header(&parts.headers, HEADER_UPLOAD_SHA256) {
        Ok(value) => value.to_string(),
        Err(response) => return response,
    };
    if expected_sha256.len() != 64 || !expected_sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return json_response(
            StatusCode::BAD_REQUEST,
            &tako_core::Response::error("invalid artifact digest"),
        );
    }

    let auth_body = tako_core::release_artifact_upload_auth_body(
        &app,
        &version,
        expected_size,
        &expected_sha256,
    );
    if let Err(error) = verify_signed_request(
        &state.runtime_config().data_dir,
        &auth_state,
        &parts.headers,
        &auth_body,
    ) {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &tako_core::Response::error(error.to_string()),
        );
    }

    let upload_dir = state.runtime_config().data_dir.join("tmp").join("uploads");
    if let Err(error) = tokio::fs::create_dir_all(&upload_dir).await {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &tako_core::Response::error(format!("create upload dir: {error}")),
        );
    }
    let temp_path = upload_dir.join(format!(
        "{}-{}.tar.zst",
        tako_core::deployment_app_id_filename(&app),
        nanoid::nanoid!(8)
    ));

    let upload_result = write_upload_body(&mut body, &temp_path, expected_size).await;
    let actual_sha256 = match upload_result {
        Ok(digest) => digest,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            tracing::debug!(%peer_addr, %error, "Failed to receive release artifact");
            return json_response(StatusCode::BAD_REQUEST, &tako_core::Response::error(error));
        }
    };

    if actual_sha256 != expected_sha256.to_ascii_lowercase() {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return json_response(
            StatusCode::BAD_REQUEST,
            &tako_core::Response::error("artifact digest mismatch"),
        );
    }

    let store_result = state.store_uploaded_release_artifact(&app, &version, &temp_path);
    let _ = tokio::fs::remove_file(&temp_path).await;
    match store_result {
        Ok(plan) => json_response(StatusCode::OK, &tako_core::Response::ok(plan)),
        Err(error) => json_response(StatusCode::BAD_REQUEST, &tako_core::Response::error(error)),
    }
}

async fn write_upload_body(
    body: &mut Incoming,
    temp_path: &PathBuf,
    expected_size: u64,
) -> Result<String, String> {
    let mut file = tokio::fs::File::create(temp_path)
        .await
        .map_err(|e| format!("create upload file {}: {e}", temp_path.display()))?;
    let mut hasher = Sha256::new();
    let mut received = 0_u64;

    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| format!("read upload body: {e}"))?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        received = received
            .checked_add(data.len() as u64)
            .ok_or_else(|| "artifact upload too large".to_string())?;
        if received > expected_size {
            return Err("artifact upload exceeded declared size".to_string());
        }
        hasher.update(&data);
        file.write_all(&data)
            .await
            .map_err(|e| format!("write upload file {}: {e}", temp_path.display()))?;
    }

    if received != expected_size {
        return Err(format!(
            "artifact upload size mismatch: expected {expected_size} bytes, received {received}"
        ));
    }
    file.shutdown()
        .await
        .map_err(|e| format!("flush upload file {}: {e}", temp_path.display()))?;
    Ok(hex::encode(hasher.finalize()))
}

fn required_header<'a>(
    headers: &'a hyper::HeaderMap,
    name: &'static str,
) -> Result<&'a str, Response<ResponseBody>> {
    let Some(value) = headers.get(name) else {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            &tako_core::Response::error(format!("missing {name} header")),
        ));
    };
    let Ok(value) = value.to_str() else {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            &tako_core::Response::error(format!("invalid {name} header")),
        ));
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            &tako_core::Response::error(format!("empty {name} header")),
        ));
    }
    Ok(value)
}

fn parse_u64_header(name: &'static str, value: &str) -> Result<u64, Response<ResponseBody>> {
    value.parse::<u64>().map_err(|_| {
        json_response(
            StatusCode::BAD_REQUEST,
            &tako_core::Response::error(format!("invalid {name} header")),
        )
    })
}

pub(crate) async fn handle_rpc_command(
    state: Arc<ServerState>,
    command: Command,
) -> tako_core::Response {
    state.handle_command(command).await
}

fn command_is_public_probe(command: &Command) -> bool {
    matches!(command, Command::Hello { .. } | Command::ServerInfo)
}

fn json_response(status: StatusCode, value: &impl serde::Serialize) -> Response<ResponseBody> {
    match serde_json::to_vec(value) {
        Ok(body) => Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .header("cache-control", "no-store")
            .body(Full::new(Bytes::from(body)))
            .expect("management HTTP response should build"),
        Err(error) => {
            tracing::error!(%error, "Failed to serialize management HTTP response");
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("content-type", "application/json")
                .header("cache-control", "no-store")
                .body(Full::new(Bytes::from_static(
                    br#"{"status":"error","message":"internal server error"}"#,
                )))
                .expect("management HTTP error response should build")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::{CertManager, CertManagerConfig};
    use parking_lot::RwLock;
    use std::collections::HashMap;
    use std::time::Duration;
    use tako_core::PROTOCOL_VERSION;
    use tempfile::TempDir;

    fn empty_challenge_tokens() -> crate::tls::ChallengeTokens {
        Arc::new(RwLock::new(HashMap::new()))
    }

    fn test_state() -> Arc<ServerState> {
        let temp = TempDir::new().expect("tempdir");
        let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
            cert_dir: temp.path().join("certs"),
            ..Default::default()
        }));
        let runtime = crate::ServerRuntimeConfig {
            server_identity: Some("SHA256:testidentity".to_string()),
            ..crate::ServerRuntimeConfig::for_defaults(temp.path().to_path_buf())
        };

        Arc::new(
            ServerState::new_with_runtime(
                temp.path().to_path_buf(),
                cert_manager,
                None,
                empty_challenge_tokens(),
                runtime,
            )
            .expect("server state"),
        )
    }

    #[tokio::test]
    async fn rpc_allows_hello_probe_without_management_auth() {
        let response = handle_rpc_command(
            test_state(),
            Command::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await;

        let tako_core::Response::Ok { data } = response else {
            panic!("expected hello ok response");
        };
        assert_eq!(
            data.get("server_identity")
                .and_then(serde_json::Value::as_str),
            Some("SHA256:testidentity")
        );
    }

    #[tokio::test]
    async fn rpc_allows_server_info_probe_without_management_auth() {
        let response = handle_rpc_command(test_state(), Command::ServerInfo).await;

        let tako_core::Response::Ok { data } = response else {
            panic!("expected server info ok response");
        };
        assert_eq!(
            data.get("server_identity")
                .and_then(serde_json::Value::as_str),
            Some("SHA256:testidentity")
        );
    }

    #[tokio::test]
    async fn bind_listener_retries_until_reload_port_is_released() {
        let occupied = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind occupied listener");
        let port = occupied.local_addr().expect("occupied addr").port();

        let bind_task = tokio::spawn(bind_listener_with_retry(
            "127.0.0.1",
            port,
            Duration::from_millis(10),
        ));

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !bind_task.is_finished(),
            "bind should wait while the old reload process still owns the port"
        );

        drop(occupied);

        let listener = tokio::time::timeout(Duration::from_secs(1), bind_task)
            .await
            .expect("bind retry should complete")
            .expect("bind task should not panic")
            .expect("bind retry should succeed");
        assert_eq!(listener.local_addr().expect("new addr").port(), port);
    }

    #[test]
    fn rpc_treats_non_probe_commands_as_private() {
        assert!(!command_is_public_probe(&Command::List));
    }

    #[tokio::test]
    async fn rpc_allows_hello_mismatch_to_return_protocol_error() {
        let response = handle_rpc_command(
            test_state(),
            Command::Hello {
                protocol_version: 999,
            },
        )
        .await;

        assert!(
            response
                .error_message()
                .is_some_and(|message| message.contains("Protocol version mismatch"))
        );
    }
}

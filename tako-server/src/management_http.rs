use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::http::StatusCode;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use tako_core::Command;
use tokio::net::TcpListener;

use crate::ServerState;
use crate::management_auth::{ManagementAuthState, verify_signed_request};

pub(crate) const MANAGEMENT_PORT: u16 = 9844;
const MAX_RPC_BODY_BYTES: usize = 1024 * 1024;

type ResponseBody = Full<Bytes>;

pub(crate) async fn serve(
    host: String,
    state: Arc<ServerState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind((host.as_str(), MANAGEMENT_PORT)).await?;
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

async fn handle_request(
    request: Request<Incoming>,
    state: Arc<ServerState>,
    auth_state: Arc<ManagementAuthState>,
    peer_addr: SocketAddr,
) -> Response<ResponseBody> {
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

//! Management transport invoked only after the native HTTPS host/TLS gate.
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Body;
use hyper::{Method, Request, Response, StatusCode};
use sha2::{Digest, Sha256};
use tako_core::Command;
use tako_network::api::{ManagementAction, RpcRequest, RpcResponse, SignedRequest};
use tako_network::{AppEnv, Permissions};

use crate::ServerState;
use crate::management_http::{json_response, log_bytes_response};
use crate::native_management_policy::{self as policy, PolicyContext, ProvisionedApp};

mod archive;
mod logs;
#[cfg(test)]
mod tests;
mod upload;

const AUTH_HEADER: &str = "x-tako-network-authorization";
const MAX_AUTH_HEADER: usize = 16 * 1024;
const MAX_RPC_BODY: usize = 1024 * 1024;
const MAX_LOG_BYTES: usize = 1024 * 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
type HttpResponse = Response<Full<Bytes>>;

pub(crate) async fn handle_request<B>(
    request: Request<B>,
    state: Arc<ServerState>,
    dispatch: crate::native_management_ownership::NativeDispatchGuard,
) -> HttpResponse
where
    B: Body<Data = Bytes> + Unpin + Send + 'static,
    B::Error: std::fmt::Display + Send + Sync + 'static,
{
    let data_dir = state.runtime_config().data_dir.clone();
    let recheck_dir = data_dir.clone();
    handle_with_authority(
        request,
        state,
        move |bytes| async move {
            agent_authority(
                crate::native_agent::relay(&data_dir, "POST", "/v1/network/authorize", &bytes)
                    .await,
            )
        },
        move |signed| async move {
            agent_authority(crate::native_agent::recheck_authority(&recheck_dir, &signed).await)
        },
        Some(dispatch),
    )
    .await
}

fn agent_authority(
    result: Result<crate::native_agent::AgentResponse, crate::native_agent::AgentError>,
) -> Result<RpcResponse, StatusCode> {
    let result = result.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    if !result.ready {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    if result.status != 200 {
        return Err(StatusCode::UNAUTHORIZED);
    }
    result.response.ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

fn permissions_for(
    signed: &SignedRequest,
    result: Result<RpcResponse, StatusCode>,
) -> Result<Permissions, HttpResponse> {
    let RpcRequest::Authorize { action } = &signed.request else {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "Invalid management authorization",
        ));
    };
    match result {
        Ok(RpcResponse::Authorized {
            signer,
            permissions,
            action: authorized,
        }) if signer == signed.signer && authorized == *action => Ok(permissions),
        Ok(_) => Err(error(
            StatusCode::UNAUTHORIZED,
            "Invalid management authorization",
        )),
        Err(status) => Err(error(
            status,
            "Management authorization unavailable or denied",
        )),
    }
}

async fn handle_with_authority<B, F, Fut, G, Check>(
    request: Request<B>,
    state: Arc<ServerState>,
    authorize: F,
    recheck: G,
    dispatch: Option<crate::native_management_ownership::NativeDispatchGuard>,
) -> HttpResponse
where
    B: Body<Data = Bytes> + Unpin + Send + 'static,
    B::Error: std::fmt::Display + Send + Sync + 'static,
    F: FnOnce(Vec<u8>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<RpcResponse, StatusCode>> + Send + 'static,
    G: FnOnce(SignedRequest) -> Check + Send + 'static,
    Check: Future<Output = Result<RpcResponse, StatusCode>> + Send + 'static,
{
    let (parts, body) = request.into_parts();
    if parts.method != Method::POST || parts.uri.query().is_some() {
        return error(StatusCode::NOT_FOUND, "Unknown management endpoint");
    }
    let path = parts.uri.path();
    if !matches!(
        path,
        "/v1/management/rpc" | "/v1/management/release-artifact" | "/v1/management/logs"
    ) {
        return error(StatusCode::NOT_FOUND, "Unknown management endpoint");
    }
    let signed = match authorization_header(&parts.headers) {
        Ok(signed) => signed,
        Err(()) => return error(StatusCode::UNAUTHORIZED, "Invalid management authorization"),
    };
    let RpcRequest::Authorize { action } = &signed.request else {
        return error(StatusCode::UNAUTHORIZED, "Invalid management authorization");
    };
    if action.validate().is_err() {
        return error(StatusCode::UNAUTHORIZED, "Invalid management authorization");
    }
    let operation = match (path, action) {
        ("/v1/management/rpc", ManagementAction::Rpc { .. }) => {
            let bytes = match read_body(body, MAX_RPC_BODY).await {
                Ok(bytes) => bytes,
                Err(status) => return error(status, "Invalid management body"),
            };
            if !matches_commitment(action, &bytes) {
                return error(StatusCode::UNAUTHORIZED, "Management commitment mismatch");
            }
            let command = match serde_json::from_slice::<Command>(&bytes) {
                Ok(command) => command,
                Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid management command"),
            };
            Operation::Rpc(command)
        }
        ("/v1/management/release-artifact", ManagementAction::ReleaseArtifact { .. }) => {
            let metadata = match upload::Metadata::parse(&parts.headers) {
                Ok(metadata) => metadata,
                Err(message) => return error(StatusCode::BAD_REQUEST, message),
            };
            if !matches_commitment(action, &metadata.auth_body()) {
                return error(StatusCode::UNAUTHORIZED, "Management commitment mismatch");
            }
            Operation::Upload(metadata, body)
        }
        ("/v1/management/logs", ManagementAction::Logs { .. }) => {
            let metadata = match Logs::parse(&parts.headers) {
                Ok(metadata) => metadata,
                Err(()) => return error(StatusCode::BAD_REQUEST, "Invalid log metadata"),
            };
            if !matches_commitment(action, &metadata.auth_body()) {
                return error(StatusCode::UNAUTHORIZED, "Management commitment mismatch");
            }
            if let Err(status) = read_body(body, 0).await {
                return error(status, "Log request body must be empty");
            }
            Operation::Logs(metadata)
        }
        _ => return error(StatusCode::UNAUTHORIZED, "Management action mismatch"),
    };
    let wire = match serde_json::to_vec(&signed) {
        Ok(wire) => wire,
        Err(_) => return error(StatusCode::UNAUTHORIZED, "Invalid management authorization"),
    };
    match operation {
        Operation::Rpc(command) => {
            let scope = action.scope().cloned();
            // Owned admission and mutation guards survive a disconnected waiter.
            match tokio::spawn(async move {
                let _dispatch = dispatch;
                if let Command::DrainServer { timeout_seconds } = &command {
                    let permissions = match permissions_for(&signed, authorize(wire).await) {
                        Ok(permissions) => permissions,
                        Err(response) => return response,
                    };
                    if let Err(message) =
                        authorize_command(&state, &permissions, scope.as_ref(), &command)
                    {
                        return error(StatusCode::FORBIDDEN, &message);
                    }
                    return rpc_response(
                        state.drain_server_before_management(*timeout_seconds).await,
                    );
                }
                let _guard = state.management_lock.lock().await;
                // Consume the signed nonce only when dispatch can proceed, using current authority.
                let permissions = match permissions_for(&signed, authorize(wire).await) {
                    Ok(permissions) => permissions,
                    Err(response) => return response,
                };
                if let Err(message) =
                    authorize_command(&state, &permissions, scope.as_ref(), &command)
                {
                    return error(StatusCode::FORBIDDEN, &message);
                }
                rpc_response(state.handle_command(command).await)
            })
            .await
            {
                Ok(response) => response,
                Err(_) => error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Management operation failed",
                ),
            }
        }
        Operation::Upload(metadata, body) => {
            let Some(scope) = action.scope().cloned() else {
                return error(StatusCode::FORBIDDEN, "Explicit scope required");
            };
            let permissions = match permissions_for(&signed, authorize(wire).await) {
                Ok(permissions) => permissions,
                Err(response) => return response,
            };
            if policy::authorize_app(&permissions, &scope, &metadata.app).is_err() {
                return error(StatusCode::FORBIDDEN, "Deployment permission denied");
            }
            upload::receive(state, scope, metadata, body, dispatch, async move {
                let current = recheck(signed.clone()).await;
                permissions_for(&signed, current)
            })
            .await
        }
        Operation::Logs(metadata) => {
            let Some(scope) = action.scope().cloned() else {
                return error(StatusCode::FORBIDDEN, "Explicit scope required");
            };
            let _dispatch = dispatch;
            let _guard = state.management_lock.lock().await;
            let permissions = match permissions_for(&signed, authorize(wire).await) {
                Ok(permissions) => permissions,
                Err(response) => return response,
            };
            let command = Command::Status {
                app: metadata.app.clone(),
            };
            if authorize_command(&state, &permissions, Some(&scope), &command).is_err()
                || upload::confined_logs(&state.runtime_config().data_dir, &metadata.app).is_err()
            {
                return error(StatusCode::FORBIDDEN, "Log access denied");
            }
            match logs::read(&state, &metadata) {
                Ok(logs) => log_bytes_response(StatusCode::OK, logs),
                Err(_) => error(StatusCode::BAD_REQUEST, "Log read failed"),
            }
        }
    }
}

enum Operation<B> {
    Rpc(Command),
    Upload(upload::Metadata, B),
    Logs(Logs),
}

fn authorization_header(headers: &hyper::HeaderMap) -> Result<SignedRequest, ()> {
    let value = header(headers, AUTH_HEADER).ok_or(())?;
    if value.len() > MAX_AUTH_HEADER {
        return Err(());
    }
    let bytes = STANDARD.decode(value).map_err(|_| ())?;
    SignedRequest::from_slice(&bytes).map_err(|_| ())
}

fn header<'a>(headers: &'a hyper::HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() || value.is_empty() || value.len() > MAX_AUTH_HEADER {
        return None;
    }
    Some(value)
}

fn matches_commitment(action: &ManagementAction, bytes: &[u8]) -> bool {
    hex::encode(Sha256::digest(bytes)) == action.sha256()
}

async fn read_body<B>(body: B, limit: usize) -> Result<Bytes, StatusCode>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display + Send + Sync + 'static,
{
    tokio::time::timeout(BODY_TIMEOUT, async {
        let mut body = body;
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
            let data = frame.into_data().map_err(|_| StatusCode::BAD_REQUEST)?;
            if data.len() > limit.saturating_sub(bytes.len()) {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            bytes.extend_from_slice(&data);
        }
        Ok(Bytes::from(bytes))
    })
    .await
    .map_err(|_| StatusCode::REQUEST_TIMEOUT)?
}

fn authorize_command(
    state: &ServerState,
    permissions: &Permissions,
    scope: Option<&AppEnv>,
    command: &Command,
) -> Result<(), String> {
    let data_dir = &state.runtime_config().data_dir;
    let fail = |_| "Cannot read provisioned management state".to_string();
    let stored = if let Some(scope) = scope {
        state
            .state_store
            .load_apps()
            .map_err(fail)?
            .into_iter()
            .find(|app| app.config.deployment_id() == scope.as_str())
    } else {
        None
    };
    let ssl = scope
        .map(|scope| state.state_store.get_ssl(scope.as_str()))
        .transpose()
        .map_err(fail)?
        .flatten();
    let credentials = scope
        .map(|scope| state.state_store.get_runtime_credentials(scope.as_str()))
        .transpose()
        .map_err(fail)?;
    let storages = scope
        .map(|scope| state.state_store.get_storages(scope.as_str()))
        .transpose()
        .map_err(fail)?;
    let backup = scope
        .map(|scope| state.state_store.get_backup(scope.as_str()))
        .transpose()
        .map_err(fail)?
        .flatten();
    let app = match (
        scope,
        stored.as_ref(),
        ssl.as_ref(),
        credentials.as_ref(),
        storages.as_ref(),
    ) {
        (Some(scope), Some(stored), Some(ssl), Some(runtime_credentials), Some(storages)) => {
            Some(ProvisionedApp {
                scope,
                routes: &stored.routes,
                ssl,
                runtime_credentials,
                storages,
                backup: backup.as_ref(),
            })
        }
        _ => None,
    };
    policy::authorize_command(
        permissions,
        scope,
        command,
        &PolicyContext { data_dir, app },
    )
    .map_err(|denial| denial.to_string())?;
    if let policy::CommandClass::Application { app } = policy::classify(command) {
        upload::confined_directory(&crate::release::app_root(data_dir, app).join("shared/logs"))
            .map_err(|_| "Managed shared log directory must not redirect".to_string())?;
    }
    Ok(())
}

fn rpc_response(response: tako_core::Response) -> HttpResponse {
    json_response(
        if response.is_ok() {
            StatusCode::OK
        } else {
            StatusCode::BAD_REQUEST
        },
        &response,
    )
}
fn error(status: StatusCode, message: &str) -> HttpResponse {
    json_response(status, &tako_core::Response::error(message))
}

struct Logs {
    app: String,
    previous: u64,
    current: u64,
    since: Option<i64>,
    max_bytes: usize,
}
impl Logs {
    fn parse(headers: &hyper::HeaderMap) -> Result<Self, ()> {
        let max_bytes = header(headers, "x-tako-log-max-bytes")
            .ok_or(())?
            .parse()
            .map_err(|_| ())?;
        if max_bytes == 0 || max_bytes > MAX_LOG_BYTES {
            return Err(());
        }
        let since = if headers.contains_key("x-tako-log-since-unix-secs") {
            Some(
                header(headers, "x-tako-log-since-unix-secs")
                    .ok_or(())?
                    .parse()
                    .map_err(|_| ())?,
            )
        } else {
            None
        };
        Ok(Self {
            app: header(headers, "x-tako-app").ok_or(())?.into(),
            previous: header(headers, "x-tako-log-previous-offset")
                .ok_or(())?
                .parse()
                .map_err(|_| ())?,
            current: header(headers, "x-tako-log-current-offset")
                .ok_or(())?
                .parse()
                .map_err(|_| ())?,
            since,
            max_bytes,
        })
    }
    fn auth_body(&self) -> Vec<u8> {
        tako_core::logs_request_auth_body(
            &self.app,
            self.previous,
            self.current,
            self.since,
            self.max_bytes,
        )
    }
}

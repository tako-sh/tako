use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::process::push_scoped_log;
use crate::protocol::{self, Response};

use crate::control::State;

const DEFAULT_TUNNEL_BASE_URL: &str = "https://tako.website/api";
const TUNNEL_API_TIMEOUT: Duration = Duration::from_secs(15);
const TUNNEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub(crate) struct TunnelRegistration {
    pub(crate) url: String,
    pub(crate) expires_at: u64,
    pub(crate) abort_handle: AbortHandle,
}

#[derive(Debug, Deserialize)]
struct CreatedTunnel {
    host: String,
    url: String,
    session: String,
    expires_at: u64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Request(TunnelRequestMessage),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Response(TunnelResponseMessage),
}

#[derive(Debug, Clone, Deserialize)]
struct TunnelRequestMessage {
    id: String,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body_base64: String,
}

#[derive(Debug, Serialize)]
struct TunnelResponseMessage {
    id: String,
    status: u16,
    headers: Vec<(String, String)>,
    body_base64: String,
}

pub(super) async fn handle_toggle_tunnel(
    state: &Arc<Mutex<State>>,
    config_path: String,
    enabled: bool,
) -> Response {
    if enabled {
        match enable_tunnel(state, &config_path).await {
            Ok((url, expires_at)) => Response::TunnelToggled {
                config_path,
                enabled: true,
                url: Some(url),
                expires_at: Some(expires_at),
            },
            Err(message) => Response::Error { message },
        }
    } else {
        let (url, expires_at) = disable_tunnel(state, &config_path, DisableReason::User);
        Response::TunnelToggled {
            config_path,
            enabled: false,
            url,
            expires_at,
        }
    }
}

async fn enable_tunnel(
    state: &Arc<Mutex<State>>,
    config_path: &str,
) -> Result<(String, u64), String> {
    let snapshot = {
        let s = state.lock().map_err(|_| "dev server state poisoned")?;
        let Some(app) = s.apps.get(config_path) else {
            return Err("app is not registered".to_string());
        };
        if let Some(tunnel) = &app.tunnel {
            return Ok((tunnel.url.clone(), tunnel.expires_at));
        }
        let Some(host) = primary_local_host(&app.hosts) else {
            return Err("app has no concrete local host".to_string());
        };
        TunnelSnapshot {
            config_path: config_path.to_string(),
            app_name: app.name.clone(),
            local_host: host,
            listen_addr: s.listen_addr.clone(),
            log_buffer: app.log_buffer.clone(),
        }
    };

    push_scoped_log(&snapshot.log_buffer, "Info", "tako", "Starting tunnel...");
    let base_url = tunnel_base_url();
    let created = create_tunnel(&base_url, &snapshot.app_name).await?;
    let connect_url = tunnel_connect_url(&base_url, &created.host, &created.session)?;

    let state_for_task = Arc::clone(state);
    let config_for_task = snapshot.config_path.clone();
    let url_for_task = created.url.clone();
    let expires_for_task = created.expires_at;
    let (start_tx, start_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        let result = run_tunnel_connection(connect_url, snapshot).await;
        if let Err(error) = result {
            tracing::debug!("Tunnel connection closed: {error}");
        }
        clear_tunnel_if_current(
            &state_for_task,
            &config_for_task,
            &url_for_task,
            DisableReason::Remote,
        );
    });
    let abort_handle = task.abort_handle();

    let (app_name, url, expires_at) = {
        let mut s = state.lock().map_err(|_| "dev server state poisoned")?;
        let Some(app) = s.apps.get_mut(config_path) else {
            abort_handle.abort();
            return Err("app is not registered".to_string());
        };
        let app_name = app.name.clone();
        app.tunnel = Some(TunnelRegistration {
            url: created.url.clone(),
            expires_at: created.expires_at,
            abort_handle,
        });
        push_scoped_log(
            &app.log_buffer,
            "Info",
            "tako",
            &format!("Tunnel on: {}", created.url),
        );
        (app_name, created.url.clone(), created.expires_at)
    };
    let _ = start_tx.send(());
    let s = state.lock().map_err(|_| "dev server state poisoned")?;
    s.events.broadcast(Response::Event {
        event: protocol::DevEvent::TunnelModeChanged {
            config_path: config_path.to_string(),
            app_name,
            enabled: true,
            url: Some(url),
            expires_at: Some(expires_at),
        },
    });

    Ok((created.url, expires_for_task))
}

enum DisableReason {
    User,
    Remote,
}

fn disable_tunnel(
    state: &Arc<Mutex<State>>,
    config_path: &str,
    reason: DisableReason,
) -> (Option<String>, Option<u64>) {
    let mut s = match state.lock() {
        Ok(s) => s,
        Err(_) => return (None, None),
    };
    let Some(app) = s.apps.get_mut(config_path) else {
        return (None, None);
    };
    let Some(tunnel) = app.tunnel.take() else {
        return (None, None);
    };
    if matches!(reason, DisableReason::User) {
        tunnel.abort_handle.abort();
    }
    let url = tunnel.url;
    let expires_at = tunnel.expires_at;
    let app_name = app.name.clone();
    push_scoped_log(&app.log_buffer, "Info", "tako", "Tunnel turned off");
    drop(s);
    let s = match state.lock() {
        Ok(s) => s,
        Err(_) => return (Some(url), Some(expires_at)),
    };
    s.events.broadcast(Response::Event {
        event: protocol::DevEvent::TunnelModeChanged {
            config_path: config_path.to_string(),
            app_name,
            enabled: false,
            url: None,
            expires_at: None,
        },
    });
    (Some(url), Some(expires_at))
}

fn clear_tunnel_if_current(
    state: &Arc<Mutex<State>>,
    config_path: &str,
    url: &str,
    reason: DisableReason,
) {
    let mut s = match state.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    let Some(app) = s.apps.get_mut(config_path) else {
        return;
    };
    let should_clear = app.tunnel.as_ref().is_some_and(|t| t.url == url);
    if !should_clear {
        return;
    }
    app.tunnel = None;
    let app_name = app.name.clone();
    if matches!(reason, DisableReason::Remote) {
        push_scoped_log(&app.log_buffer, "Info", "tako", "Tunnel turned off");
    }
    drop(s);
    let s = match state.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    s.events.broadcast(Response::Event {
        event: protocol::DevEvent::TunnelModeChanged {
            config_path: config_path.to_string(),
            app_name,
            enabled: false,
            url: None,
            expires_at: None,
        },
    });
}

#[derive(Clone)]
struct TunnelSnapshot {
    config_path: String,
    app_name: String,
    local_host: String,
    listen_addr: String,
    log_buffer: crate::state::LogBuffer,
}

async fn create_tunnel(base_url: &str, app_name: &str) -> Result<CreatedTunnel, String> {
    let url = format!("{}/v1/tunnels", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .connect_timeout(TUNNEL_API_TIMEOUT)
        .timeout(TUNNEL_API_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to build tunnel HTTP client: {error}"))?;
    let response = client
        .post(url)
        .json(&serde_json::json!({ "app": app_name }))
        .send()
        .await
        .map_err(|error| format!("failed to create tunnel: {error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("failed to create tunnel ({status}): {body}"));
    }
    response
        .json::<CreatedTunnel>()
        .await
        .map_err(|error| format!("invalid tunnel response: {error}"))
}

async fn run_tunnel_connection(
    connect_url: String,
    snapshot: TunnelSnapshot,
) -> Result<(), String> {
    let (socket, _) = tokio::time::timeout(
        TUNNEL_CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async(&connect_url),
    )
    .await
    .map_err(|_| "timed out connecting tunnel websocket".to_string())?
    .map_err(|error| format!("failed to connect tunnel websocket: {error}"))?;
    push_scoped_log(
        &snapshot.log_buffer,
        "Info",
        "tako",
        &format!("Tunnel connected for {}", snapshot.app_name),
    );
    let (mut writer, mut reader) = socket.split();
    let client = local_proxy_client()?;

    while let Some(message) = reader.next().await {
        let message = message.map_err(|error| format!("tunnel websocket error: {error}"))?;
        let Message::Text(text) = message else {
            if matches!(message, Message::Close(_)) {
                break;
            }
            continue;
        };
        let server_message = serde_json::from_str::<ServerMessage>(&text)
            .map_err(|error| format!("invalid tunnel message: {error}"))?;
        let ServerMessage::Request(request) = server_message;
        let response = forward_to_local_proxy(&client, &snapshot, request).await;
        let payload = serde_json::to_string(&ClientMessage::Response(response))
            .map_err(|error| format!("failed to encode tunnel response: {error}"))?;
        writer
            .send(Message::Text(payload.into()))
            .await
            .map_err(|error| format!("failed to send tunnel response: {error}"))?;
    }

    Ok(())
}

fn local_proxy_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("failed to build tunnel HTTP client: {error}"))
}

async fn forward_to_local_proxy(
    client: &reqwest::Client,
    snapshot: &TunnelSnapshot,
    request: TunnelRequestMessage,
) -> TunnelResponseMessage {
    match forward_to_local_proxy_inner(client, snapshot, &request).await {
        Ok(response) => response,
        Err(error) => TunnelResponseMessage {
            id: request.id,
            status: 502,
            headers: vec![(
                "content-type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            )],
            body_base64: STANDARD.encode(error),
        },
    }
}

async fn forward_to_local_proxy_inner(
    client: &reqwest::Client,
    snapshot: &TunnelSnapshot,
    request: &TunnelRequestMessage,
) -> Result<TunnelResponseMessage, String> {
    let method = request
        .method
        .parse::<reqwest::Method>()
        .map_err(|error| format!("invalid method: {error}"))?;
    let body = STANDARD
        .decode(request.body_base64.as_bytes())
        .map_err(|error| format!("invalid request body: {error}"))?;
    let url = local_proxy_url(&snapshot.listen_addr, &request.path);
    let mut builder = client
        .request(method, url)
        .header(reqwest::header::HOST, snapshot.local_host.as_str());
    for (name, value) in forwarded_request_headers(&request.headers) {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(&value) else {
            continue;
        };
        builder = builder.header(name, value);
    }
    let response = builder
        .body(body)
        .send()
        .await
        .map_err(|error| format!("local proxy request failed: {error}"))?;
    let status = response.status().as_u16();
    let headers = forwarded_response_headers(response.headers());
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("local proxy response failed: {error}"))?;
    Ok(TunnelResponseMessage {
        id: request.id.clone(),
        status,
        headers,
        body_base64: STANDARD.encode(body),
    })
}

fn tunnel_base_url() -> String {
    std::env::var("TAKO_TUNNEL_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_TUNNEL_BASE_URL.to_string())
}

fn tunnel_connect_url(base_url: &str, host: &str, session: &str) -> Result<String, String> {
    let trimmed = base_url.trim_end_matches('/');
    let ws_base = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return Err("tunnel URL must start with http:// or https://".to_string());
    };
    Ok(format!(
        "{ws_base}/v1/tunnels/connect?host={host}&session={session}"
    ))
}

fn local_proxy_url(listen_addr: &str, path: &str) -> String {
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    format!("https://{listen_addr}{path}")
}

fn primary_local_host(hosts: &[String]) -> Option<String> {
    hosts
        .iter()
        .map(|host| host.split('/').next().unwrap_or(host))
        .find(|host| !host.starts_with("*.") && !host.is_empty())
        .map(str::to_string)
}

fn forwarded_request_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| !is_hop_by_hop_or_host(name))
        .cloned()
        .collect()
}

fn forwarded_response_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            if is_hop_by_hop_or_host(name.as_str()) || name == reqwest::header::CONTENT_LENGTH {
                return None;
            }
            Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
        })
        .collect()
}

fn is_hop_by_hop_or_host(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tunnel_base_url_uses_apex_api_path() {
        assert_eq!(DEFAULT_TUNNEL_BASE_URL, "https://tako.website/api");
    }

    #[test]
    fn websocket_url_uses_ws_schemes_and_query() {
        let url = tunnel_connect_url(
            "https://tako.website/api",
            "app-a8f3k2zz.tako.website",
            "session-token",
        )
        .unwrap();
        assert_eq!(
            url,
            "wss://tako.website/api/v1/tunnels/connect?host=app-a8f3k2zz.tako.website&session=session-token"
        );
    }

    #[test]
    fn websocket_url_accepts_http_for_tests() {
        let url = tunnel_connect_url("http://127.0.0.1:3000/", "host.test", "s").unwrap();
        assert_eq!(
            url,
            "ws://127.0.0.1:3000/v1/tunnels/connect?host=host.test&session=s"
        );
    }

    #[test]
    fn forwarded_request_headers_skip_hop_by_hop_and_host() {
        let headers = vec![
            ("host".to_string(), "public.tako.website".to_string()),
            ("connection".to_string(), "keep-alive".to_string()),
            ("x-test".to_string(), "ok".to_string()),
            ("transfer-encoding".to_string(), "chunked".to_string()),
        ];
        assert_eq!(
            forwarded_request_headers(&headers),
            vec![("x-test".to_string(), "ok".to_string())]
        );
    }
}

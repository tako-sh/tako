use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

use crate::protocol::{self, Response};

use crate::control::State;
use crate::identity::TakoIdentity;

const DEFAULT_TUNNEL_BASE_URL: &str = "https://tako.website/api";
const TUNNEL_API_TIMEOUT: Duration = Duration::from_secs(15);
const TUNNEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TUNNEL_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(25);
const TUNNEL_RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);
const TUNNEL_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

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
struct TunnelChallenge {
    host: String,
    nonce: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Request(TunnelRequestMessage),
    WebSocketOpen(TunnelWebSocketOpenMessage),
    WebSocketFrame(TunnelWebSocketFrameMessage),
    WebSocketClose(TunnelWebSocketCloseMessage),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Response(TunnelResponseMessage),
    WebSocketFrame(TunnelWebSocketFrameMessage),
    WebSocketClose(TunnelWebSocketCloseMessage),
    Ping,
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

#[derive(Debug, Clone, Deserialize)]
struct TunnelWebSocketOpenMessage {
    id: String,
    path: String,
    headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TunnelWebSocketFrameMessage {
    id: String,
    kind: TunnelWebSocketFrameKind,
    data_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TunnelWebSocketCloseMessage {
    id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TunnelWebSocketFrameKind {
    Text,
    Binary,
    Ping,
    Pong,
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
        let (url, expires_at) = disable_tunnel(state, &config_path, TunnelCloseReason::User);
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
            upstream_port: app.upstream_port,
        }
    };

    let base_url = tunnel_base_url();
    let identity = TakoIdentity::load_or_create()?;
    let public_key = identity.public_key()?;
    let challenge = create_tunnel_challenge(&base_url, &snapshot.app_name, &public_key).await?;
    let signature = identity.sign_tunnel(
        &challenge.nonce,
        &snapshot.app_name,
        &challenge.host,
        &public_key,
    )?;
    let created = create_tunnel(
        &base_url,
        &snapshot.app_name,
        &public_key,
        &challenge.nonce,
        &signature,
    )
    .await?;
    let connect_url = tunnel_connect_url(&base_url, &created.host, &created.session)?;

    let state_for_task = Arc::clone(state);
    let config_for_task = snapshot.config_path.clone();
    let url_for_task = created.url.clone();
    let (start_tx, start_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        run_tunnel_session(
            connect_url,
            snapshot,
            state_for_task,
            config_for_task,
            url_for_task,
        )
        .await;
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
            close_reason: None,
        },
    });

    Ok((created.url, expires_at))
}

type TunnelCloseReason = protocol::TunnelCloseReason;

fn disable_tunnel(
    state: &Arc<Mutex<State>>,
    config_path: &str,
    reason: TunnelCloseReason,
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
    if matches!(
        reason,
        TunnelCloseReason::User | TunnelCloseReason::Shutdown
    ) {
        tunnel.abort_handle.abort();
    }
    let url = tunnel.url;
    let expires_at = tunnel.expires_at;
    let app_name = app.name.clone();
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
            close_reason: Some(reason),
        },
    });
    (Some(url), Some(expires_at))
}

fn tunnel_is_current(state: &Arc<Mutex<State>>, config_path: &str, url: &str) -> bool {
    let s = match state.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };
    s.apps
        .get(config_path)
        .and_then(|app| app.tunnel.as_ref())
        .is_some_and(|tunnel| tunnel.url == url)
}

fn broadcast_tunnel_connection(
    state: &Arc<Mutex<State>>,
    config_path: &str,
    app_name: &str,
    url: &str,
    connected: bool,
) {
    let s = match state.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    if s.apps
        .get(config_path)
        .and_then(|app| app.tunnel.as_ref())
        .is_none_or(|tunnel| tunnel.url != url)
    {
        return;
    }
    s.events.broadcast(Response::Event {
        event: protocol::DevEvent::TunnelConnectionChanged {
            config_path: config_path.to_string(),
            app_name: app_name.to_string(),
            connected,
            url: url.to_string(),
        },
    });
}

#[derive(Clone)]
struct TunnelSnapshot {
    config_path: String,
    app_name: String,
    local_host: String,
    listen_addr: String,
    upstream_port: u16,
}

async fn create_tunnel_challenge(
    base_url: &str,
    app_name: &str,
    public_key: &str,
) -> Result<TunnelChallenge, String> {
    let url = format!("{}/v1/tunnel-challenges", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .connect_timeout(TUNNEL_API_TIMEOUT)
        .timeout(TUNNEL_API_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to build tunnel HTTP client: {error}"))?;
    let response = client
        .post(url)
        .json(&serde_json::json!({
            "app": app_name,
            "public_key": public_key,
        }))
        .send()
        .await
        .map_err(|error| format!("failed to prepare tunnel: {error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("failed to prepare tunnel ({status}): {body}"));
    }
    response
        .json::<TunnelChallenge>()
        .await
        .map_err(|error| format!("invalid tunnel challenge: {error}"))
}

async fn create_tunnel(
    base_url: &str,
    app_name: &str,
    public_key: &str,
    nonce: &str,
    signature: &str,
) -> Result<CreatedTunnel, String> {
    let url = format!("{}/v1/tunnels", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .connect_timeout(TUNNEL_API_TIMEOUT)
        .timeout(TUNNEL_API_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to build tunnel HTTP client: {error}"))?;
    let response = client
        .post(url)
        .json(&serde_json::json!({
            "app": app_name,
            "public_key": public_key,
            "nonce": nonce,
            "signature": signature,
        }))
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

async fn run_tunnel_session(
    connect_url: String,
    snapshot: TunnelSnapshot,
    state: Arc<Mutex<State>>,
    config_path: String,
    url: String,
) {
    let mut delay = TUNNEL_RECONNECT_INITIAL_DELAY;
    let mut reported_reconnecting = false;

    loop {
        if !tunnel_is_current(&state, &config_path, &url) {
            return;
        }

        match connect_tunnel_socket(&connect_url).await {
            Ok(socket) => {
                if reported_reconnecting {
                    broadcast_tunnel_connection(
                        &state,
                        &config_path,
                        &snapshot.app_name,
                        &url,
                        true,
                    );
                    reported_reconnecting = false;
                }
                delay = TUNNEL_RECONNECT_INITIAL_DELAY;
                if let Err(error) = run_tunnel_connection(socket, snapshot.clone()).await {
                    tracing::debug!("Tunnel connection closed: {error}");
                }
            }
            Err(error) => {
                tracing::debug!("Tunnel connection failed: {error}");
            }
        }

        if !tunnel_is_current(&state, &config_path, &url) {
            return;
        }
        if !reported_reconnecting {
            broadcast_tunnel_connection(&state, &config_path, &snapshot.app_name, &url, false);
            reported_reconnecting = true;
        }

        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(TUNNEL_RECONNECT_MAX_DELAY);
    }
}

async fn connect_tunnel_socket(
    connect_url: &str,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String> {
    let (socket, _) = tokio::time::timeout(
        TUNNEL_CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async(connect_url),
    )
    .await
    .map_err(|_| "timed out connecting tunnel websocket".to_string())?
    .map_err(|error| format!("failed to connect tunnel websocket: {error}"))?;
    Ok(socket)
}

async fn run_tunnel_connection(
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    snapshot: TunnelSnapshot,
) -> Result<(), String> {
    let (mut writer, mut reader) = socket.split();
    let client = local_proxy_client(&snapshot.local_host, &snapshot.listen_addr)?;
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<String>(128);
    let writer_task = tokio::spawn(async move {
        while let Some(payload) = outbound_rx.recv().await {
            writer
                .send(Message::Text(payload.into()))
                .await
                .map_err(|error| format!("failed to send tunnel message: {error}"))?;
        }
        Ok::<(), String>(())
    });
    let mut web_sockets: BTreeMap<String, mpsc::Sender<Message>> = BTreeMap::new();
    let mut keepalive = tokio::time::interval(TUNNEL_KEEPALIVE_INTERVAL);

    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                send_client_message(&outbound_tx, ClientMessage::Ping).await?;
            }
            message = reader.next() => {
                let Some(message) = message else {
                    break;
                };
                let message = message.map_err(|error| format!("tunnel websocket error: {error}"))?;
                let Message::Text(text) = message else {
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                    continue;
                };
                let server_message = serde_json::from_str::<ServerMessage>(&text)
                    .map_err(|error| format!("invalid tunnel message: {error}"))?;
                match server_message {
                    ServerMessage::Request(request) => {
                        let response = forward_to_local_proxy(&client, &snapshot, request).await;
                        send_client_message(&outbound_tx, ClientMessage::Response(response)).await?;
                    }
                    ServerMessage::WebSocketOpen(open) => {
                        let id = open.id.clone();
                        match open_local_websocket(&snapshot, open, outbound_tx.clone()).await {
                            Ok(sender) => {
                                web_sockets.insert(id, sender);
                            }
                            Err(error) => {
                                tracing::debug!(error = %error, "failed to open local websocket");
                                send_client_message(
                                    &outbound_tx,
                                    ClientMessage::WebSocketClose(TunnelWebSocketCloseMessage { id }),
                                )
                                .await?;
                            }
                        }
                    }
                    ServerMessage::WebSocketFrame(frame) => {
                        let id = frame.id.clone();
                        if let Some(sender) = web_sockets.get(&id)
                            && let Ok(message) = tunnel_frame_to_tungstenite_message(frame)
                        {
                            let _ = sender.send(message).await;
                        }
                    }
                    ServerMessage::WebSocketClose(close) => {
                        if let Some(sender) = web_sockets.remove(&close.id) {
                            let _ = sender.send(Message::Close(None)).await;
                        }
                    }
                }
            }
        }
    }

    writer_task.abort();
    Ok(())
}

async fn send_client_message(
    outbound: &mpsc::Sender<String>,
    message: ClientMessage,
) -> Result<(), String> {
    let payload = serde_json::to_string(&message)
        .map_err(|error| format!("failed to encode tunnel message: {error}"))?;
    outbound
        .send(payload)
        .await
        .map_err(|_| "tunnel websocket writer closed".to_string())
}

fn local_proxy_client(local_host: &str, listen_addr: &str) -> Result<reqwest::Client, String> {
    let listen_addr = local_proxy_listen_addr(listen_addr)?;
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(local_host, &[listen_addr])
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
    let url = local_proxy_url(&snapshot.local_host, &snapshot.listen_addr, &request.path)?;
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

async fn open_local_websocket(
    snapshot: &TunnelSnapshot,
    open: TunnelWebSocketOpenMessage,
    outbound: mpsc::Sender<String>,
) -> Result<mpsc::Sender<Message>, String> {
    let id = open.id.clone();
    let mut request = local_websocket_url(snapshot.upstream_port, &open.path)?
        .into_client_request()
        .map_err(|error| format!("invalid local websocket request: {error}"))?;
    for (name, value) in forwarded_websocket_headers(&open.headers) {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(&value) else {
            continue;
        };
        request.headers_mut().insert(name, value);
    }
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| format!("local websocket request failed: {error}"))?;
    let (mut local_writer, mut local_reader) = socket.split();
    let (to_local, mut from_tunnel) = mpsc::channel::<Message>(128);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                message = from_tunnel.recv() => {
                    let Some(message) = message else {
                        break;
                    };
                    if local_writer.send(message).await.is_err() {
                        break;
                    }
                }
                message = local_reader.next() => {
                    let Some(Ok(message)) = message else {
                        break;
                    };
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                    let Some(frame) = tungstenite_message_to_tunnel_frame(&id, message) else {
                        continue;
                    };
                    if send_client_message(&outbound, ClientMessage::WebSocketFrame(frame)).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = send_client_message(
            &outbound,
            ClientMessage::WebSocketClose(TunnelWebSocketCloseMessage { id }),
        )
        .await;
    });
    Ok(to_local)
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
    let mut url = reqwest::Url::parse(&format!("{ws_base}/v1/tunnels/connect"))
        .map_err(|error| format!("invalid tunnel URL: {error}"))?;
    url.query_pairs_mut()
        .append_pair("host", host)
        .append_pair("session", session);
    Ok(url.to_string())
}

fn local_proxy_url(local_host: &str, listen_addr: &str, path: &str) -> Result<String, String> {
    let listen_addr = local_proxy_listen_addr(listen_addr)?;
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Ok(format!(
        "https://{}:{}{}",
        local_host,
        listen_addr.port(),
        path
    ))
}

fn local_websocket_url(upstream_port: u16, path: &str) -> Result<String, String> {
    if upstream_port == 0 {
        return Err("app has no active upstream port".to_string());
    }
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Ok(format!("ws://127.0.0.1:{upstream_port}{path}"))
}

fn local_proxy_listen_addr(listen_addr: &str) -> Result<SocketAddr, String> {
    listen_addr
        .parse()
        .map_err(|error| format!("invalid local proxy listen address: {error}"))
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

fn forwarded_websocket_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase();
            !is_hop_by_hop_or_host(&name) && !name.starts_with("sec-websocket-")
        })
        .cloned()
        .collect()
}

fn tungstenite_message_to_tunnel_frame(
    id: &str,
    message: Message,
) -> Option<TunnelWebSocketFrameMessage> {
    let (kind, data) = match message {
        Message::Text(text) => (
            TunnelWebSocketFrameKind::Text,
            text.to_string().into_bytes(),
        ),
        Message::Binary(bytes) => (TunnelWebSocketFrameKind::Binary, bytes.to_vec()),
        Message::Ping(bytes) => (TunnelWebSocketFrameKind::Ping, bytes.to_vec()),
        Message::Pong(bytes) => (TunnelWebSocketFrameKind::Pong, bytes.to_vec()),
        Message::Close(_) | Message::Frame(_) => return None,
    };
    Some(TunnelWebSocketFrameMessage {
        id: id.to_string(),
        kind,
        data_base64: STANDARD.encode(data),
    })
}

fn tunnel_frame_to_tungstenite_message(
    frame: TunnelWebSocketFrameMessage,
) -> Result<Message, String> {
    let data = STANDARD
        .decode(frame.data_base64.as_bytes())
        .map_err(|error| format!("invalid websocket frame: {error}"))?;
    match frame.kind {
        TunnelWebSocketFrameKind::Text => {
            let text = String::from_utf8(data)
                .map_err(|error| format!("invalid websocket text frame: {error}"))?;
            Ok(Message::Text(text.into()))
        }
        TunnelWebSocketFrameKind::Binary => Ok(Message::Binary(data.into())),
        TunnelWebSocketFrameKind::Ping => Ok(Message::Ping(data.into())),
        TunnelWebSocketFrameKind::Pong => Ok(Message::Pong(data.into())),
    }
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
            "a8f3k2zz.tako.website",
            "session-token",
        )
        .unwrap();
        assert_eq!(
            url,
            "wss://tako.website/api/v1/tunnels/connect?host=a8f3k2zz.tako.website&session=session-token"
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
    fn websocket_url_escapes_query_values() {
        let url =
            tunnel_connect_url("https://tako.website/api", "app.test", "a&b=c space").unwrap();
        assert_eq!(
            url,
            "wss://tako.website/api/v1/tunnels/connect?host=app.test&session=a%26b%3Dc+space"
        );
    }

    #[test]
    fn local_proxy_url_uses_app_host_for_tls_sni() {
        let url = local_proxy_url("app.test", "127.0.0.1:47831", "/api").unwrap();
        assert_eq!(url, "https://app.test:47831/api");
    }

    #[test]
    fn local_proxy_url_normalizes_paths() {
        let url = local_proxy_url("app.test", "127.0.0.1:47831", "api").unwrap();
        assert_eq!(url, "https://app.test:47831/api");
    }

    #[test]
    fn local_websocket_url_uses_upstream_port() {
        let url = local_websocket_url(5173, "/@vite/client").unwrap();
        assert_eq!(url, "ws://127.0.0.1:5173/@vite/client");
    }

    #[test]
    fn local_proxy_url_rejects_invalid_listen_addr() {
        let error = local_proxy_url("app.test", "localhost:47831", "/").unwrap_err();
        assert!(error.contains("invalid local proxy listen address"));
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

    #[test]
    fn forwarded_websocket_headers_skip_handshake_headers() {
        let headers = vec![
            ("host".to_string(), "public.tako.website".to_string()),
            ("sec-websocket-key".to_string(), "key".to_string()),
            (
                "origin".to_string(),
                "https://public.tako.website".to_string(),
            ),
        ];
        assert_eq!(
            forwarded_websocket_headers(&headers),
            vec![(
                "origin".to_string(),
                "https://public.tako.website".to_string()
            )]
        );
    }

    #[test]
    fn websocket_text_frames_roundtrip_through_tunnel_encoding() {
        let frame = tungstenite_message_to_tunnel_frame("ws-1", Message::Text("hello".into()))
            .expect("frame");

        assert_eq!(frame.id, "ws-1");
        assert_eq!(frame.kind, TunnelWebSocketFrameKind::Text);

        let message = tunnel_frame_to_tungstenite_message(frame).expect("message");
        assert_eq!(message, Message::Text("hello".into()));
    }
}

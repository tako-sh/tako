use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::tunnel_wire::*;
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{
        Error as WebSocketError, Message, client::IntoClientRequest, http::StatusCode,
        protocol::CloseFrame,
    },
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
const TUNNEL_CLOSE_ACTIVE_LIMIT: u16 = 4409;
const TUNNEL_CLOSE_REPLACED: u16 = 4410;

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

#[derive(Debug, PartialEq, Eq)]
enum TunnelConnectError {
    Retry(String),
    Terminal {
        reason: TunnelCloseReason,
        message: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum TunnelConnectionEnd {
    Retry(String),
    Terminal {
        reason: TunnelCloseReason,
        message: String,
    },
}

impl From<String> for TunnelConnectionEnd {
    fn from(message: String) -> Self {
        Self::Retry(message)
    }
}

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

async fn current_upstream_port(
    state: &Arc<Mutex<State>>,
    config_path: &str,
) -> Result<u16, String> {
    let routes = {
        let s = state.lock().map_err(|_| "dev server state poisoned")?;
        if !s.apps.contains_key(config_path) {
            return Err("app is not registered".to_string());
        }
        s.routes.clone()
    };

    routes
        .wait_for_active_port(&format!("reg:{config_path}"), Duration::from_secs(30))
        .await
        .filter(|port| *port != 0)
        .ok_or_else(|| "app has no active upstream port".to_string())
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
                let connected_at = tokio::time::Instant::now();
                match run_tunnel_connection(
                    socket,
                    snapshot.clone(),
                    Arc::clone(&state),
                    config_path.clone(),
                )
                .await
                {
                    Ok(()) => {}
                    Err(TunnelConnectionEnd::Retry(error)) => {
                        tracing::debug!("Tunnel connection closed: {error}");
                    }
                    Err(TunnelConnectionEnd::Terminal { reason, message }) => {
                        tracing::info!("Tunnel disabled: {message}");
                        disable_tunnel(&state, &config_path, reason);
                        return;
                    }
                }
                if connected_at.elapsed() >= Duration::from_secs(75) {
                    delay = TUNNEL_RECONNECT_INITIAL_DELAY;
                }
            }
            Err(TunnelConnectError::Retry(error)) => {
                tracing::debug!("Tunnel connection failed: {error}");
            }
            Err(TunnelConnectError::Terminal { reason, message }) => {
                tracing::warn!("Tunnel disabled: {message}");
                disable_tunnel(&state, &config_path, reason);
                return;
            }
        }

        if !tunnel_is_current(&state, &config_path, &url) {
            return;
        }
        if !reported_reconnecting {
            broadcast_tunnel_connection(&state, &config_path, &snapshot.app_name, &url, false);
            reported_reconnecting = true;
        }

        let mut entropy = [0_u8; 8];
        let _ = getrandom::fill(&mut entropy);
        tokio::time::sleep(reconnect_delay(delay, u64::from_le_bytes(entropy))).await;
        delay = (delay * 2).min(TUNNEL_RECONNECT_MAX_DELAY);
    }
}

fn reconnect_delay(delay: Duration, entropy: u64) -> Duration {
    delay / 2
        + Duration::from_secs_f64(delay.as_secs_f64() / 2.0 * (entropy as f64 / u64::MAX as f64))
}

async fn connect_tunnel_socket(
    connect_url: &str,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, TunnelConnectError> {
    let mut request = connect_url
        .into_client_request()
        .map_err(|error| TunnelConnectError::Retry(error.to_string()))?;
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static(TUNNEL_PROTOCOL),
    );
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_WIRE_MESSAGE))
        .max_frame_size(Some(MAX_WIRE_MESSAGE));
    let (socket, response) = tokio::time::timeout(
        TUNNEL_CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await
    .map_err(|_| TunnelConnectError::Retry("timed out connecting tunnel websocket".to_string()))?
    .map_err(classify_tunnel_connect_error)?;
    if response
        .headers()
        .get("sec-websocket-protocol")
        .is_none_or(|value| value != TUNNEL_PROTOCOL)
    {
        return Err(TunnelConnectError::Retry(
            "tunnel protocol mismatch; update Tako and the relay together".into(),
        ));
    }
    Ok(socket)
}

fn classify_tunnel_connect_error(error: WebSocketError) -> TunnelConnectError {
    match error {
        WebSocketError::Http(response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
            TunnelConnectError::Terminal {
                reason: TunnelCloseReason::LimitExceeded,
                message: tunnel_http_error_message(response.status(), response.body().as_deref()),
            }
        }
        error => TunnelConnectError::Retry(format!("failed to connect tunnel websocket: {error}")),
    }
}

fn tunnel_http_error_message(status: StatusCode, body: Option<&[u8]>) -> String {
    let message = body
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok())
        .and_then(|json| {
            json.get("error")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        });
    match message {
        Some(message) => format!("tunnel service refused connection ({status}): {message}"),
        None => format!("tunnel service refused connection ({status})"),
    }
}

fn terminal_close_reason(close: Option<&CloseFrame>) -> Option<(TunnelCloseReason, String)> {
    let close = close?;
    match close.code.into() {
        TUNNEL_CLOSE_ACTIVE_LIMIT => Some((
            TunnelCloseReason::LimitExceeded,
            "active tunnel limit reached".to_string(),
        )),
        TUNNEL_CLOSE_REPLACED => Some((
            TunnelCloseReason::Replaced,
            "tunnel was replaced by a newer session".to_string(),
        )),
        _ => None,
    }
}

struct HttpTask {
    abort: AbortHandle,
    credit: Arc<tokio::sync::Semaphore>,
}

struct WebSocketTask {
    abort: AbortHandle,
    sender: mpsc::Sender<Message>,
}

async fn run_tunnel_connection<S>(
    socket: WebSocketStream<S>,
    snapshot: TunnelSnapshot,
    state: Arc<Mutex<State>>,
    config_path: String,
) -> Result<(), TunnelConnectionEnd>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let client = local_proxy_client(&snapshot.local_host, &snapshot.listen_addr)?;
    run_connection(socket, snapshot, state, config_path, client).await
}

async fn run_connection<S>(
    socket: WebSocketStream<S>,
    snapshot: TunnelSnapshot,
    state: Arc<Mutex<State>>,
    config_path: String,
    client: reqwest::Client,
) -> Result<(), TunnelConnectionEnd>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut writer, mut reader) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Vec<u8>>(16);
    let mut writer_task = tokio::task::JoinSet::new();
    writer_task.spawn(async move {
        while let Some(payload) = outbound_rx.recv().await {
            tokio::time::timeout(
                Duration::from_secs(10),
                writer.send(Message::Binary(payload.into())),
            )
            .await
            .map_err(|_| "tunnel writer timed out".to_string())?
            .map_err(|error| format!("failed to send tunnel message: {error}"))?;
        }
        Ok::<(), String>(())
    });
    // All children are aborted on every exit path, including cancellation of the session.
    let mut tasks = tokio::task::JoinSet::new();
    let mut requests: BTreeMap<String, HttpTask> = BTreeMap::new();
    let mut web_sockets: BTreeMap<String, WebSocketTask> = BTreeMap::new();
    let mut keepalive = tokio::time::interval(TUNNEL_KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_pong = tokio::time::Instant::now();
    let mut ping_nonce = 0_u64;

    loop {
        tokio::select! {
            _ = writer_task.join_next() => return Err("tunnel websocket writer stopped".to_string().into()),
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Ok(id)) = completed {
                    requests.remove(&id);
                    web_sockets.remove(&id);
                }
            }
            _ = tokio::time::sleep_until(last_pong + Duration::from_secs(75)) => {
                return Err("tunnel heartbeat timed out".to_string().into());
            }
            _ = keepalive.tick() => {
                ping_nonce = ping_nonce.wrapping_add(1);
                send_client_message(&outbound_tx, ClientMessage::Ping { nonce: ping_nonce }).await?;
            }
            message = reader.next() => {
                let Some(message) = message else { return Ok(()); };
                let message = message.map_err(|error| format!("tunnel websocket error: {error}"))?;
                let Message::Binary(data) = message else {
                    if let Message::Close(close) = message {
                        return match terminal_close_reason(close.as_ref()) {
                            Some((reason, message)) => Err(TunnelConnectionEnd::Terminal { reason, message }),
                            None => Ok(()),
                        };
                    }
                    continue;
                };
                if data.len() > MAX_WIRE_MESSAGE { return Err("tunnel message too large".to_string().into()); }
                let message = rmp_serde::from_slice::<ServerMessage>(&data)
                    .map_err(|error| format!("invalid tunnel message: {error}"))?;
                match message {
                    ServerMessage::Request(request) => {
                        let id = request.id.clone();
                        if requests.contains_key(&id) { return Err("duplicate tunnel request".to_string().into()); }
                        if requests.len() >= MAX_IN_FLIGHT {
                            send_error_response(&outbound_tx, &id, 503, "Too many concurrent tunnel requests").await?;
                            continue;
                        }
                        let credit = Arc::new(tokio::sync::Semaphore::new(RESPONSE_WINDOW));
                        let task_credit = Arc::clone(&credit);
                        let client = client.clone();
                        let snapshot = snapshot.clone();
                        let outbound = outbound_tx.clone();
                        let task_id = id.clone();
                        let abort = tasks.spawn(async move {
                            let _ = forward_to_local_proxy(&client, &snapshot, request, &outbound, &task_credit).await;
                            task_id
                        });
                        requests.insert(id, HttpTask { abort, credit });
                    }
                    ServerMessage::Cancel { id } => {
                        if let Some(task) = requests.remove(&id) { task.abort.abort(); }
                    }
                    ServerMessage::ResponseCredit { id } => {
                        if let Some(task) = requests.get(&id) {
                            if task.credit.available_permits() >= RESPONSE_WINDOW { return Err("invalid response credit".to_string().into()); }
                            task.credit.add_permits(1);
                        }
                    }
                    ServerMessage::WebSocketOpen(open) => {
                        let id = open.id.clone();
                        if web_sockets.contains_key(&id) { return Err("duplicate tunnel websocket".to_string().into()); }
                        if web_sockets.len() >= MAX_IN_FLIGHT {
                            send_client_message(&outbound_tx, ClientMessage::WebSocketClose(TunnelWebSocketCloseMessage { id })).await?;
                            continue;
                        }
                        let (sender, incoming) = mpsc::channel(8);
                        let state = Arc::clone(&state);
                        let config_path = config_path.clone();
                        let outbound = outbound_tx.clone();
                        let task_id = id.clone();
                        let abort = tasks.spawn(async move {
                            let result = async {
                                let upstream_port = current_upstream_port(&state, &config_path).await?;
                                relay_local_websocket(upstream_port, open, &outbound, incoming).await
                            }.await;
                            if let Err(error) = result { tracing::debug!(%error, "local websocket closed"); }
                            let _ = send_client_message(&outbound, ClientMessage::WebSocketClose(TunnelWebSocketCloseMessage { id: task_id.clone() })).await;
                            task_id
                        });
                        web_sockets.insert(id, WebSocketTask { abort, sender });
                    }
                    ServerMessage::WebSocketFrame(frame) => {
                        let id = frame.id.clone();
                        let Some(task) = web_sockets.get(&id) else { continue; };
                        let delivered = if frame.data.len() <= MAX_WS_FRAME {
                            tunnel_frame_to_tungstenite_message(frame).ok().is_some_and(|message| task.sender.try_send(message).is_ok())
                        } else { false };
                        if !delivered {
                            if let Some(task) = web_sockets.remove(&id) { task.abort.abort(); }
                            send_client_message(&outbound_tx, ClientMessage::WebSocketClose(TunnelWebSocketCloseMessage { id })).await?;
                        }
                    }
                    ServerMessage::WebSocketClose(close) => {
                        if let Some(task) = web_sockets.remove(&close.id) { task.abort.abort(); }
                    }
                    ServerMessage::Pong { nonce } => {
                        if nonce == ping_nonce { last_pong = tokio::time::Instant::now(); }
                    }
                }
            }
        }
    }
}

async fn send_client_message(
    outbound: &mpsc::Sender<Vec<u8>>,
    message: ClientMessage,
) -> Result<(), String> {
    let payload = rmp_serde::to_vec_named(&message)
        .map_err(|error| format!("failed to encode tunnel message: {error}"))?;
    tokio::time::timeout(Duration::from_secs(10), outbound.send(payload))
        .await
        .map_err(|_| "tunnel writer queue timed out".to_string())?
        .map_err(|_| "tunnel websocket writer closed".to_string())
}

fn local_proxy_client(local_host: &str, listen_addr: &str) -> Result<reqwest::Client, String> {
    local_proxy_client_with_trusted_root(local_host, listen_addr, None)
}

#[cfg(test)]
fn local_proxy_client_with_root(
    local_host: &str,
    listen_addr: &str,
    trusted_root_pem: &str,
) -> Result<reqwest::Client, String> {
    local_proxy_client_with_trusted_root(local_host, listen_addr, Some(trusted_root_pem))
}

fn local_proxy_client_with_trusted_root(
    local_host: &str,
    listen_addr: &str,
    trusted_root_pem: Option<&str>,
) -> Result<reqwest::Client, String> {
    let listen_addr = local_proxy_listen_addr(listen_addr)?;
    if !listen_addr.ip().is_loopback() {
        return Err("tunnel forwarding requires a loopback proxy address".to_string());
    }
    let mut client_builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(local_host, &[listen_addr]);
    if let Some(pem) = trusted_root_pem {
        let certificate = reqwest::Certificate::from_pem(pem.as_bytes())
            .map_err(|error| format!("invalid tunnel local proxy root certificate: {error}"))?;
        client_builder = client_builder.add_root_certificate(certificate);
    }
    client_builder
        .build()
        .map_err(|error| format!("failed to build tunnel HTTP client: {error}"))
}

async fn send_error_response(
    outbound: &mpsc::Sender<Vec<u8>>,
    id: &str,
    status: u16,
    error: &str,
) -> Result<(), String> {
    send_client_message(
        outbound,
        ClientMessage::Response(TunnelResponseMessage {
            id: id.into(),
            status,
            headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
        }),
    )
    .await?;
    send_client_message(
        outbound,
        ClientMessage::ResponseChunk(TunnelBodyChunk {
            id: id.into(),
            data: error.as_bytes().to_vec(),
        }),
    )
    .await?;
    send_client_message(outbound, ClientMessage::ResponseEnd { id: id.into() }).await
}

async fn forward_to_local_proxy(
    client: &reqwest::Client,
    snapshot: &TunnelSnapshot,
    request: TunnelRequestMessage,
    outbound: &mpsc::Sender<Vec<u8>>,
    credit: &tokio::sync::Semaphore,
) -> Result<(), String> {
    let id = request.id.clone();
    let response = match forward_to_local_proxy_inner(client, snapshot, request).await {
        Ok(response) => response,
        Err(error) => return send_error_response(outbound, &id, 502, &error).await,
    };
    send_client_message(
        outbound,
        ClientMessage::Response(TunnelResponseMessage {
            id: id.clone(),
            status: response.status().as_u16(),
            headers: forwarded_response_headers(response.headers()),
        }),
    )
    .await?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                return send_client_message(outbound, ClientMessage::ResponseError { id }).await;
            }
        };
        for data in chunk.chunks(CHUNK_SIZE) {
            credit
                .acquire()
                .await
                .map_err(|_| "response cancelled".to_string())?
                .forget();
            send_client_message(
                outbound,
                ClientMessage::ResponseChunk(TunnelBodyChunk {
                    id: id.clone(),
                    data: data.to_vec(),
                }),
            )
            .await?;
        }
    }
    send_client_message(outbound, ClientMessage::ResponseEnd { id }).await
}

async fn forward_to_local_proxy_inner(
    client: &reqwest::Client,
    snapshot: &TunnelSnapshot,
    request: TunnelRequestMessage,
) -> Result<reqwest::Response, String> {
    let method = request
        .method
        .parse::<reqwest::Method>()
        .map_err(|error| format!("invalid method: {error}"))?;
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
    // Only establishing the response has a deadline; SSE may live indefinitely.
    tokio::time::timeout(Duration::from_secs(30), builder.body(request.body).send())
        .await
        .map_err(|_| "local proxy response headers timed out".to_string())?
        .map_err(|error| format!("local proxy request failed: {error}"))
}

async fn relay_local_websocket(
    upstream_port: u16,
    open: TunnelWebSocketOpenMessage,
    outbound: &mpsc::Sender<Vec<u8>>,
    mut from_tunnel: mpsc::Receiver<Message>,
) -> Result<(), String> {
    let id = open.id;
    let mut request = local_websocket_url(upstream_port, &open.path)?
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
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_FRAME))
        .max_frame_size(Some(MAX_WS_FRAME));
    let (socket, _) = tokio::time::timeout(
        TUNNEL_CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await
    .map_err(|_| "local websocket handshake timed out".to_string())?
    .map_err(|error| format!("local websocket request failed: {error}"))?;
    let (mut writer, mut reader) = socket.split();
    loop {
        tokio::select! {
            message = from_tunnel.recv() => {
                let Some(message) = message else { break; };
                tokio::time::timeout(Duration::from_secs(10), writer.send(message))
                    .await.map_err(|_| "local websocket write timed out".to_string())?
                    .map_err(|error| error.to_string())?;
            }
            message = reader.next() => {
                let Some(Ok(message)) = message else { break; };
                if matches!(message, Message::Close(_)) { break; }
                let Some(frame) = tungstenite_message_to_tunnel_frame(&id, message) else { continue; };
                send_client_message(outbound, ClientMessage::WebSocketFrame(frame)).await?;
            }
        }
    }
    Ok(())
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
        .filter_map(|(name, value)| {
            let lower_name = name.to_ascii_lowercase();
            if is_hop_by_hop_or_host(&lower_name)
                || lower_name == "origin"
                || (lower_name.starts_with("sec-websocket-")
                    && lower_name != "sec-websocket-protocol")
            {
                return None;
            }
            Some((name.clone(), value.clone()))
        })
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
        data,
    })
}

fn tunnel_frame_to_tungstenite_message(
    frame: TunnelWebSocketFrameMessage,
) -> Result<Message, String> {
    let data = frame.data;
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
    fn reconnect_jitter_spreads_attempts_without_exceeding_backoff() {
        for delay in [TUNNEL_RECONNECT_INITIAL_DELAY, TUNNEL_RECONNECT_MAX_DELAY] {
            let low = reconnect_delay(delay, 0);
            let high = reconnect_delay(delay, u64::MAX);
            assert!(low >= delay / 2);
            assert!(high <= delay);
            assert!(high > low);
        }
    }

    async fn send_relay<S>(socket: &mut WebSocketStream<S>, message: ServerMessage)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        socket
            .send(Message::Binary(
                rmp_serde::to_vec_named(&message).unwrap().into(),
            ))
            .await
            .unwrap();
    }

    async fn receive_client<S>(socket: &mut WebSocketStream<S>) -> ClientMessage
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        loop {
            let Message::Binary(data) = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
            else {
                continue;
            };
            let message = rmp_serde::from_slice(&data).unwrap();
            if let ClientMessage::Ping { nonce } = message {
                send_relay(socket, ServerMessage::Pong { nonce }).await;
            } else {
                return message;
            }
        }
    }

    fn test_request(id: &str, path: &str) -> ServerMessage {
        ServerMessage::Request(TunnelRequestMessage {
            id: id.into(),
            method: "GET".into(),
            path: path.into(),
            headers: vec![],
            body: vec![],
        })
    }

    // Exercise the actual session reader/writer and HTTPS forwarding, not just a
    // semaphore: an unfinished response must leave the tunnel usable, and Cancel
    // must close the upstream connection and release its task.
    #[tokio::test]
    async fn streaming_request_does_not_block_fast_request_and_cancel_closes_upstream() {
        assert_slow_response_isolated(false, false).await;
    }

    #[tokio::test]
    async fn exhausted_response_window_does_not_block_fast_request() {
        assert_slow_response_isolated(true, false).await;
    }

    #[tokio::test]
    async fn aborting_session_cancels_upstream_requests() {
        assert_slow_response_isolated(false, true).await;
    }

    async fn assert_slow_response_isolated(bulk: bool, abort_session: bool) {
        use openssl::{
            pkey::PKey,
            ssl::{SslAcceptor, SslMethod},
            x509::X509,
        };
        use std::io::{Read, Write};
        use tokio_tungstenite::tungstenite::protocol::Role;
        let ca = tako::dev::LocalCA::generate().unwrap();
        let cert = ca.generate_leaf_cert("app.test").unwrap();
        let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        acceptor
            .set_certificate(&X509::from_pem(cert.cert_pem.as_bytes()).unwrap())
            .unwrap();
        acceptor
            .set_private_key(&PKey::private_key_from_pem(cert.key_pem.as_bytes()).unwrap())
            .unwrap();
        let acceptor = Arc::new(acceptor.build());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let client = local_proxy_client_with_root("app.test", &addr, ca.ca_cert_pem()).unwrap();
        let (closed_tx, closed_rx) = oneshot::channel();
        let closed_tx = Arc::new(Mutex::new(Some(closed_tx)));
        let server = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let stream = stream.into_std().unwrap();
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let acceptor = Arc::clone(&acceptor);
                let closed_tx = Arc::clone(&closed_tx);
                connections.spawn_blocking(move || {
                    let mut stream = acceptor.accept(stream).unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        stream.read_exact(&mut byte).unwrap();
                        request.push(byte[0]);
                    }
                    if request.starts_with(b"GET /sse ") {
                        let write = if bulk {
                            let body = vec![42; CHUNK_SIZE * RESPONSE_WINDOW * 2];
                            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
                            stream.write_all(&body)
                        } else {
                            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\nd\r\ndata: first\n\n\r\n")
                        };
                        if write.is_err() {
                            let _ = closed_tx.lock().unwrap().take().unwrap().send(true);
                            return;
                        }
                        stream.flush().unwrap();
                        let mut byte = [0];
                        let closed = match stream.read(&mut byte) {
                            Ok(0) => true,
                            Err(error) => !matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
                            _ => false,
                        };
                        let _ = closed_tx.lock().unwrap().take().unwrap().send(closed);
                    } else {
                        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfast").unwrap();
                    }
                });
            }
            while let Some(result) = connections.join_next().await {
                result.unwrap();
            }
        });
        let (client_socket, relay_socket) = tokio::io::duplex(MAX_WIRE_MESSAGE);
        let client_socket =
            WebSocketStream::from_raw_socket(client_socket, Role::Client, None).await;
        let mut relay = WebSocketStream::from_raw_socket(relay_socket, Role::Server, None).await;
        let snapshot = TunnelSnapshot {
            config_path: "/tmp/tunnel.toml".into(),
            app_name: "app".into(),
            local_host: "app.test".into(),
            listen_addr: addr,
        };
        let state = tunnel_state_with_app(&snapshot.config_path, 0, false);
        let task = tokio::spawn(run_connection(
            client_socket,
            snapshot.clone(),
            state,
            snapshot.config_path,
            client,
        ));
        send_relay(&mut relay, test_request("slow", "/sse")).await;
        assert!(
            matches!(receive_client(&mut relay).await, ClientMessage::Response(head) if head.id == "slow" && head.status == 200)
        );
        for _ in 0..if bulk { RESPONSE_WINDOW } else { 1 } {
            let ClientMessage::ResponseChunk(chunk) = receive_client(&mut relay).await else {
                panic!("expected slow body chunk")
            };
            assert_eq!(chunk.id, "slow");
            assert!(chunk.data.len() <= CHUNK_SIZE);
            if !bulk {
                assert_eq!(chunk.data, b"data: first\n\n");
            }
        }
        send_relay(&mut relay, test_request("fast", "/fast")).await;
        assert!(
            matches!(receive_client(&mut relay).await, ClientMessage::Response(head) if head.id == "fast" && head.status == 200)
        );
        assert!(
            matches!(receive_client(&mut relay).await, ClientMessage::ResponseChunk(chunk) if chunk.id == "fast" && chunk.data == b"fast")
        );
        assert!(
            matches!(receive_client(&mut relay).await, ClientMessage::ResponseEnd { id } if id == "fast")
        );
        if abort_session {
            task.abort();
        } else {
            send_relay(&mut relay, ServerMessage::Cancel { id: "slow".into() }).await;
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(5), closed_rx)
                .await
                .unwrap()
                .unwrap()
        );
        task.abort();
        let _ = task.await;
        server.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn missing_heartbeat_acknowledgement_ends_the_session() {
        use tokio_tungstenite::tungstenite::protocol::Role;
        let (client_socket, relay_socket) = tokio::io::duplex(4096);
        let client_socket =
            WebSocketStream::from_raw_socket(client_socket, Role::Client, None).await;
        let mut relay = WebSocketStream::from_raw_socket(relay_socket, Role::Server, None).await;
        let snapshot = TunnelSnapshot {
            config_path: "/tmp/tunnel.toml".into(),
            app_name: "app".into(),
            local_host: "app.test".into(),
            listen_addr: "127.0.0.1:47831".into(),
        };
        let state = tunnel_state_with_app(&snapshot.config_path, 0, false);
        let task = tokio::spawn(run_tunnel_connection(
            client_socket,
            snapshot.clone(),
            state,
            snapshot.config_path,
        ));
        let _ = relay.next().await.unwrap().unwrap(); // Deliberately do not acknowledge the first Ping.
        tokio::time::advance(Duration::from_secs(76)).await;
        assert!(
            matches!(task.await.unwrap(), Err(TunnelConnectionEnd::Retry(message)) if message.contains("heartbeat"))
        );
    }

    use tokio_tungstenite::tungstenite::handshake::server::Response as ServerResponse;
    use tokio_tungstenite::tungstenite::http::{Request, Response as HttpResponse};

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
    fn tunnel_connect_error_treats_limit_as_terminal() {
        let response = HttpResponse::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .body(Some(
                br#"{"error":"too many active tunnels for this machine (limit 5)"}"#.to_vec(),
            ))
            .unwrap();

        let error = classify_tunnel_connect_error(WebSocketError::Http(Box::new(response)));

        assert_eq!(
            error,
            TunnelConnectError::Terminal {
                reason: TunnelCloseReason::LimitExceeded,
                message: "tunnel service refused connection (429 Too Many Requests): too many active tunnels for this machine (limit 5)".to_string(),
            }
        );
    }

    #[test]
    fn tunnel_connect_error_retries_other_http_failures() {
        let response = HttpResponse::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(None)
            .unwrap();

        let error = classify_tunnel_connect_error(WebSocketError::Http(Box::new(response)));

        assert!(
            matches!(error, TunnelConnectError::Retry(message) if message.contains("502 Bad Gateway"))
        );
    }

    #[test]
    fn terminal_close_reason_maps_active_limit_code() {
        let close = CloseFrame {
            code: TUNNEL_CLOSE_ACTIVE_LIMIT.into(),
            reason: "active_tunnel_limit".into(),
        };

        assert_eq!(
            terminal_close_reason(Some(&close)),
            Some((
                TunnelCloseReason::LimitExceeded,
                "active tunnel limit reached".to_string()
            ))
        );
    }

    #[test]
    fn terminal_close_reason_maps_replaced_code() {
        let close = CloseFrame {
            code: TUNNEL_CLOSE_REPLACED.into(),
            reason: "tunnel_replaced".into(),
        };

        assert_eq!(
            terminal_close_reason(Some(&close)),
            Some((
                TunnelCloseReason::Replaced,
                "tunnel was replaced by a newer session".to_string()
            ))
        );
    }

    #[test]
    fn terminal_close_reason_ignores_normal_close() {
        let close = CloseFrame {
            code: 1000.into(),
            reason: "normal".into(),
        };

        assert_eq!(terminal_close_reason(Some(&close)), None);
    }

    #[test]
    fn local_proxy_client_requires_loopback_destination() {
        for address in ["192.0.2.1:443", "[2001:db8::1]:443", "0.0.0.0:443"] {
            let error = local_proxy_client("app.test", address).unwrap_err();
            assert!(error.contains("loopback"));
        }
        for address in ["127.0.0.1:443", "127.77.0.1:443", "[::1]:443"] {
            assert!(local_proxy_client("app.test", address).is_ok());
        }
    }

    #[tokio::test]
    async fn local_proxy_client_rejects_untrusted_certificate() {
        use openssl::pkey::PKey;
        use openssl::ssl::{SslAcceptor, SslMethod};
        use openssl::x509::X509;

        let ca = tako::dev::LocalCA::generate().expect("generate test CA");
        let cert = ca
            .generate_leaf_cert("app.test")
            .expect("generate test certificate");
        let mut acceptor =
            SslAcceptor::mozilla_intermediate(SslMethod::tls()).expect("create TLS acceptor");
        acceptor
            .set_certificate(&X509::from_pem(cert.cert_pem.as_bytes()).expect("parse cert"))
            .expect("set cert");
        acceptor
            .set_private_key(
                &PKey::private_key_from_pem(cert.key_pem.as_bytes()).expect("parse key"),
            )
            .expect("set key");
        let acceptor = acceptor.build();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind TLS fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let server = tokio::task::spawn_blocking(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let _ = acceptor.accept(stream);
        });

        let client = local_proxy_client("app.test", &format!("127.0.0.1:{port}"))
            .expect("build local proxy client");
        let result = client.get(format!("https://app.test:{port}/")).send().await;

        server.await.expect("server task");
        assert!(
            result.is_err(),
            "the local proxy client must reject an untrusted certificate"
        );
    }

    #[tokio::test]
    async fn local_proxy_client_accepts_certificate_from_explicitly_trusted_ca() {
        use openssl::pkey::PKey;
        use openssl::ssl::{SslAcceptor, SslMethod};
        use openssl::x509::X509;
        use std::io::{Read, Write};

        let ca = tako::dev::LocalCA::generate().expect("generate test CA");
        let cert = ca
            .generate_leaf_cert("app.test")
            .expect("generate test certificate");
        let mut acceptor =
            SslAcceptor::mozilla_intermediate(SslMethod::tls()).expect("create TLS acceptor");
        acceptor
            .set_certificate(&X509::from_pem(cert.cert_pem.as_bytes()).expect("parse cert"))
            .expect("set cert");
        acceptor
            .set_private_key(
                &PKey::private_key_from_pem(cert.key_pem.as_bytes()).expect("parse key"),
            )
            .expect("set key");
        let acceptor = acceptor.build();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind TLS fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let server = tokio::task::spawn_blocking(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let Ok(mut stream) = acceptor.accept(stream) else {
                return;
            };
            let mut request = [0; 4096];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(
                b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        });

        let client = local_proxy_client_with_root(
            "app.test",
            &format!("127.0.0.1:{port}"),
            ca.ca_cert_pem(),
        )
        .expect("build local proxy client");
        let result = client.get(format!("https://app.test:{port}/")).send().await;

        server.await.expect("server task");
        assert!(
            result.is_ok(),
            "an explicitly trusted CA should succeed: {result:?}"
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
            ("sec-websocket-version".to_string(), "13".to_string()),
            ("sec-websocket-protocol".to_string(), "vite-hmr".to_string()),
            (
                "origin".to_string(),
                "https://public.tako.website".to_string(),
            ),
        ];
        assert_eq!(
            forwarded_websocket_headers(&headers),
            vec![("sec-websocket-protocol".to_string(), "vite-hmr".to_string())]
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

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn local_websocket_open_forwards_initial_text_frame() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind websocket fixture");
        let upstream_port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept websocket");
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |request: &Request<()>, mut response: ServerResponse| {
                    assert_eq!(
                        request
                            .headers()
                            .get("sec-websocket-protocol")
                            .and_then(|value| value.to_str().ok()),
                        Some("vite-hmr")
                    );
                    response
                        .headers_mut()
                        .insert("sec-websocket-protocol", "vite-hmr".parse().unwrap());
                    Ok(response)
                },
            )
            .await
            .expect("accept websocket handshake");
            socket
                .send(Message::Text(r#"{"type":"connected"}"#.into()))
                .await
                .expect("send initial frame");
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        let (outbound, mut inbound) = mpsc::channel(4);
        let open = TunnelWebSocketOpenMessage {
            id: "ws-1".to_string(),
            path: "/".to_string(),
            headers: vec![("sec-websocket-protocol".to_string(), "vite-hmr".to_string())],
        };

        let (_to_local, from_tunnel) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            relay_local_websocket(upstream_port, open, &outbound, from_tunnel).await
        });
        let message = tokio::time::timeout(Duration::from_secs(1), inbound.recv())
            .await
            .expect("initial frame timed out")
            .expect("initial frame");
        let ClientMessage::WebSocketFrame(frame) =
            rmp_serde::from_slice(&message).expect("client message")
        else {
            panic!("expected frame")
        };
        assert_eq!(frame.id, "ws-1");
        assert_eq!(frame.kind, TunnelWebSocketFrameKind::Text);
        assert_eq!(frame.data, br#"{"type":"connected"}"#);
        task.await.unwrap().unwrap();
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn websocket_port_lookup_uses_current_app_route() {
        let config_path = "/tmp/tako.toml";
        let state = tunnel_state_with_app(config_path, 0, false);
        {
            let mut guard = state.lock().expect("state lock");
            guard
                .routes
                .activate_with_port(&format!("reg:{config_path}"), 5173);
            guard
                .apps
                .get_mut(config_path)
                .expect("registered app")
                .upstream_port = 5173;
        }

        let port = current_upstream_port(&state, config_path)
            .await
            .expect("current port");

        assert_eq!(port, 5173);
    }

    fn tunnel_state_with_app(
        config_path: &str,
        upstream_port: u16,
        active: bool,
    ) -> Arc<Mutex<State>> {
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        let routes = crate::proxy::Routes::default();
        routes.set_routes(
            format!("reg:{config_path}"),
            vec!["app.test".to_string()],
            upstream_port,
            active,
        );
        let state = Arc::new(Mutex::new(State::new(
            shutdown_tx,
            routes,
            crate::control::EventsHub::default(),
            true,
            53535,
            47831,
            "127.0.0.1:47831".to_string(),
            "127.77.0.1".to_string(),
        )));
        state.lock().expect("state lock").apps.insert(
            config_path.to_string(),
            crate::state::RuntimeApp {
                project_dir: "/tmp".to_string(),
                name: "app".to_string(),
                variant: None,
                hosts: vec!["app.test".to_string()],
                upstream_port,
                is_idle: false,
                command: vec!["bun".to_string(), "dev".to_string()],
                worker_command: None,
                workflow_retention_ms: Some(tako_workflows::DEFAULT_RETENTION_MS),
                env: Default::default(),
                log_buffer: crate::state::LogBuffer::new(),
                pid: None,
                client_pid: None,
                tunnel: None,
                readiness_failure_hint: None,
                bootstrap_token: "token".to_string(),
                secrets: Default::default(),
                storages: Default::default(),
            },
        );
        state
    }
}

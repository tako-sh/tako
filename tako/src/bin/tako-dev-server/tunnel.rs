mod handshake;

use handshake::connect_tunnel_socket;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use crate::tunnel_wire::*;
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest, protocol::CloseFrame},
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

// Track sent/reserved bytes separately: Semaphore::available_permits excludes
// permits reserved by a waiting acquire_many, so it cannot validate peer credit.
struct ResponseCredit {
    permits: tokio::sync::Semaphore,
    outstanding: AtomicUsize,
}

impl ResponseCredit {
    fn new() -> Self {
        Self {
            permits: tokio::sync::Semaphore::new(RESPONSE_WINDOW),
            outstanding: AtomicUsize::new(0),
        }
    }

    async fn consume(&self, len: usize) -> Result<(), String> {
        let bytes = response_chunk_cost(len);
        let permit = self
            .permits
            .acquire_many(bytes as u32)
            .await
            .map_err(|_| "response cancelled".to_string())?;
        self.outstanding.fetch_add(bytes, Ordering::Relaxed);
        permit.forget();
        Ok(())
    }

    fn replenish(&self, bytes: usize) -> Result<(), String> {
        if bytes == 0 {
            return Err("invalid response credit".into());
        }
        let mut outstanding = self.outstanding.load(Ordering::Relaxed);
        loop {
            let remaining = outstanding
                .checked_sub(bytes)
                .ok_or("invalid response credit")?;
            match self.outstanding.compare_exchange_weak(
                outstanding,
                remaining,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => outstanding = current,
            }
        }
        self.permits.add_permits(bytes);
        Ok(())
    }
}

struct HttpTask {
    abort: AbortHandle,
    credit: Arc<ResponseCredit>,
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
                            send_error_response(&outbound_tx, &id, 503, "Too many concurrent tunnel requests", None).await?;
                            continue;
                        }
                        let credit = Arc::new(ResponseCredit::new());
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
                    ServerMessage::ResponseCredit { id, bytes } => {
                        if let Some(task) = requests.get(&id) {
                            task.credit.replenish(bytes)?;
                        }
                    }
                    ServerMessage::WebSocketOpen(open) => {
                        let id = open.id.clone();
                        if web_sockets.contains_key(&id) { return Err("duplicate tunnel websocket".to_string().into()); }
                        if web_sockets.len() >= MAX_IN_FLIGHT {
                            send_websocket_response(&outbound_tx, id, 503, vec![]).await?;
                            continue;
                        }
                        let (sender, incoming) = mpsc::channel(8);
                        let state = Arc::clone(&state);
                        let config_path = config_path.clone();
                        let outbound = outbound_tx.clone();
                        let task_id = id.clone();
                        let abort = tasks.spawn(async move {
                            let result = match current_upstream_port(&state, &config_path).await {
                                Ok(upstream_port) => relay_local_websocket(upstream_port, open, &outbound, incoming).await,
                                Err(_) => send_websocket_response(&outbound, open.id, 502, vec![]).await,
                            };
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
    credit: Option<&ResponseCredit>,
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
    let mut end = error.len().min(CHUNK_SIZE);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    let data = error.as_bytes()[..end].to_vec();
    if let Some(credit) = credit {
        credit.consume(data.len()).await?;
    }
    send_client_message(
        outbound,
        ClientMessage::ResponseChunk(TunnelBodyChunk {
            id: id.into(),
            data,
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
    credit: &ResponseCredit,
) -> Result<(), String> {
    let id = request.id.clone();
    let response = match forward_to_local_proxy_inner(client, snapshot, request).await {
        Ok(response) => response,
        Err(error) => return send_error_response(outbound, &id, 502, &error, Some(credit)).await,
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
            credit.consume(data.len()).await?;
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

async fn send_websocket_response(
    outbound: &mpsc::Sender<Vec<u8>>,
    id: String,
    status: u16,
    headers: Vec<(String, String)>,
) -> Result<(), String> {
    send_client_message(
        outbound,
        ClientMessage::WebSocketResponse(TunnelWebSocketResponseMessage {
            id,
            status,
            headers,
        }),
    )
    .await
}

fn websocket_response_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    // The relay needs Connection tokens to strip all hop-by-hop headers.
    headers
        .iter()
        .filter_map(|(name, value)| Some((name.as_str().into(), value.to_str().ok()?.into())))
        .collect()
}

fn valid_local_websocket_upgrade(
    request: &reqwest::header::HeaderMap,
    response: &reqwest::header::HeaderMap,
) -> bool {
    let Some(key) = request.get("sec-websocket-key") else {
        return false;
    };
    let expected_accept =
        tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    let has_upgrade = response
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    if !has_upgrade
        || !response
            .get("upgrade")
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
        || response.get_all("sec-websocket-accept").iter().count() != 1
        || !response
            .get("sec-websocket-accept")
            .is_some_and(|value| value.as_bytes() == expected_accept.as_bytes())
        || response.contains_key("sec-websocket-extensions")
    {
        return false;
    }
    let mut selected = response.get_all("sec-websocket-protocol").iter();
    let Some(protocol) = selected.next() else {
        return true;
    };
    selected.next().is_none()
        && request
            .get_all("sec-websocket-protocol")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|offered| offered.trim().as_bytes() == protocol.as_bytes())
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
        request.headers_mut().append(name, value);
    }
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_FRAME))
        .max_frame_size(Some(MAX_WS_FRAME));
    // Use the existing HTTP client for the upgrade. Tungstenite's HTTP client
    // rejects a valid 101 with no selected subprotocol when one was offered.
    // RFC 6455 allows the app to decline all offered protocols.
    let client = reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())?;
    let url = request.uri().to_string().replacen("ws://", "http://", 1);
    let response = match tokio::time::timeout(
        TUNNEL_CONNECT_TIMEOUT,
        client.get(url).headers(request.headers().clone()).send(),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => return send_websocket_response(outbound, id, 502, vec![]).await,
        Err(_) => return send_websocket_response(outbound, id, 504, vec![]).await,
    };
    let status = response.status().as_u16();
    let headers = websocket_response_headers(response.headers());
    if status != 101 {
        return send_websocket_response(outbound, id, status, headers).await;
    }
    if !valid_local_websocket_upgrade(request.headers(), response.headers()) {
        return send_websocket_response(outbound, id, 502, vec![]).await;
    }
    let upgraded = match tokio::time::timeout(TUNNEL_CONNECT_TIMEOUT, response.upgrade()).await {
        Ok(Ok(upgraded)) => upgraded,
        Ok(Err(_)) => return send_websocket_response(outbound, id, 502, vec![]).await,
        Err(_) => return send_websocket_response(outbound, id, 504, vec![]).await,
    };
    let socket = WebSocketStream::from_raw_socket(
        upgraded,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        Some(config),
    )
    .await;
    // Queue acceptance before any frames, including an app's immediate greeting.
    send_websocket_response(outbound, id.clone(), status, headers).await?;
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
mod tests;

#[cfg(test)]
#[path = "tunnel_benchmark.rs"]
mod benchmark;

#[cfg(test)]
#[path = "tunnel_transport_tests.rs"]
mod transport_tests;

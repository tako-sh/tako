use super::*;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TunnelConnectError {
    Retry(String),
    Terminal {
        reason: TunnelCloseReason,
        message: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TunnelConnectionEnd {
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

pub(super) async fn run_tunnel_session(
    connect_url: String,
    snapshot: TunnelSnapshot,
    state: Arc<Mutex<State>>,
    config_path: String,
    url: String,
    generation: u64,
) {
    let mut delay = TUNNEL_RECONNECT_INITIAL_DELAY;
    let mut reported_reconnecting = false;

    loop {
        if !tunnel_is_current(&state, &config_path, generation) {
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
                        generation,
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
                        disable_tunnel_generation(&state, &config_path, reason, Some(generation));
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
                disable_tunnel_generation(&state, &config_path, reason, Some(generation));
                return;
            }
        }

        if !tunnel_is_current(&state, &config_path, generation) {
            return;
        }
        if !reported_reconnecting {
            broadcast_tunnel_connection(
                &state,
                &config_path,
                &snapshot.app_name,
                &url,
                false,
                generation,
            );
            reported_reconnecting = true;
        }

        let mut entropy = [0_u8; 8];
        let _ = getrandom::fill(&mut entropy);
        tokio::time::sleep(reconnect_delay(delay, u64::from_le_bytes(entropy))).await;
        delay = (delay * 2).min(TUNNEL_RECONNECT_MAX_DELAY);
    }
}

pub(super) fn reconnect_delay(delay: Duration, entropy: u64) -> Duration {
    delay / 2
        + Duration::from_secs_f64(delay.as_secs_f64() / 2.0 * (entropy as f64 / u64::MAX as f64))
}

pub(super) fn terminal_close_reason(
    close: Option<&CloseFrame>,
) -> Option<(TunnelCloseReason, String)> {
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
pub(super) struct ResponseCredit {
    pub(super) permits: tokio::sync::Semaphore,
    pub(super) outstanding: AtomicUsize,
}

impl ResponseCredit {
    pub(super) fn new() -> Self {
        Self {
            permits: tokio::sync::Semaphore::new(RESPONSE_WINDOW),
            outstanding: AtomicUsize::new(0),
        }
    }

    pub(super) async fn consume(&self, len: usize) -> Result<(), String> {
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

    pub(super) fn replenish(&self, bytes: usize) -> Result<(), String> {
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

pub(super) struct HttpTask {
    abort: AbortHandle,
    credit: Arc<ResponseCredit>,
}

pub(super) struct WebSocketTask {
    abort: AbortHandle,
    sender: mpsc::Sender<Message>,
}

pub(super) async fn run_tunnel_connection<S>(
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

pub(super) async fn run_connection<S>(
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

pub(super) async fn send_client_message(
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

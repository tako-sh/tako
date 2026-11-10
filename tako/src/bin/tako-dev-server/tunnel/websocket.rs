use super::*;

pub(super) async fn send_websocket_response(
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

pub(super) fn websocket_response_headers(
    headers: &reqwest::header::HeaderMap,
) -> Vec<(String, String)> {
    // The relay needs Connection tokens to strip all hop-by-hop headers.
    headers
        .iter()
        .filter_map(|(name, value)| Some((name.as_str().into(), value.to_str().ok()?.into())))
        .collect()
}

pub(super) fn valid_local_websocket_upgrade(
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

pub(super) async fn relay_local_websocket(
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

pub(super) fn local_websocket_url(upstream_port: u16, path: &str) -> Result<String, String> {
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

pub(super) fn forwarded_websocket_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
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

pub(super) fn tungstenite_message_to_tunnel_frame(
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

pub(super) fn tunnel_frame_to_tungstenite_message(
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

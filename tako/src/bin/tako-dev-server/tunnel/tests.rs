use super::*;

#[tokio::test]
async fn byte_credit_rejects_unearned_acknowledgements_with_waiting_producer() {
    let credit = Arc::new(ResponseCredit::new());
    assert!(credit.replenish(0).is_err());
    assert!(credit.replenish(1).is_err());
    let spent = RESPONSE_WINDOW - 512;
    credit.consume(spent).await.unwrap();
    let waiting_credit = Arc::clone(&credit);
    let waiting = tokio::spawn(async move { waiting_credit.consume(CHUNK_SIZE).await });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    assert!(
        credit.replenish(RESPONSE_WINDOW).is_err(),
        "reserved permits must not look like sent bytes"
    );
    credit.replenish(spent).unwrap();
    waiting.await.unwrap().unwrap();
    credit.replenish(CHUNK_SIZE).unwrap();
    assert_eq!(credit.permits.available_permits(), RESPONSE_WINDOW);
}

#[tokio::test]
async fn tiny_chunks_use_byte_budget_and_release_exact_credit() {
    let credit = ResponseCredit::new();
    for _ in 0..20 {
        credit.consume(13).await.unwrap();
    }
    assert_eq!(
        credit.outstanding.load(Ordering::Relaxed),
        20 * MIN_CHUNK_CREDIT
    );
    credit.replenish(20 * MIN_CHUNK_CREDIT).unwrap();
    assert_eq!(credit.permits.available_permits(), RESPONSE_WINDOW);
}

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
                        let body = vec![42; RESPONSE_WINDOW * 4];
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
    let client_socket = WebSocketStream::from_raw_socket(client_socket, Role::Client, None).await;
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
    let mut spent = 0;
    while spent < if bulk { RESPONSE_WINDOW } else { 1 } {
        let ClientMessage::ResponseChunk(chunk) = receive_client(&mut relay).await else {
            panic!("expected slow body chunk")
        };
        assert_eq!(chunk.id, "slow");
        assert!(chunk.data.len() <= CHUNK_SIZE);
        spent += response_chunk_cost(chunk.data.len());
        if bulk && RESPONSE_WINDOW - spent < CHUNK_SIZE {
            break;
        }
        if !bulk {
            assert_eq!(chunk.data, b"data: first\n\n");
        }
    }
    send_relay(&mut relay, test_request("fast", "/fast")).await;
    loop {
        match receive_client(&mut relay).await {
            ClientMessage::ResponseChunk(chunk) if bulk && chunk.id == "slow" => {
                spent += response_chunk_cost(chunk.data.len());
                assert!(spent <= RESPONSE_WINDOW);
            }
            ClientMessage::Response(head) if head.id == "fast" && head.status == 200 => break,
            message => panic!("unexpected message: {message:?}"),
        }
    }
    assert!(
        matches!(receive_client(&mut relay).await, ClientMessage::ResponseChunk(chunk) if chunk.id == "fast" && chunk.data == b"fast")
    );
    assert!(
        matches!(receive_client(&mut relay).await, ClientMessage::ResponseEnd { id } if id == "fast")
    );
    if bulk {
        send_relay(
            &mut relay,
            ServerMessage::ResponseCredit {
                id: "slow".into(),
                bytes: spent,
            },
        )
        .await;
        assert!(
            matches!(receive_client(&mut relay).await, ClientMessage::ResponseChunk(chunk) if chunk.id == "slow")
        );
    }
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
    let client_socket = WebSocketStream::from_raw_socket(client_socket, Role::Client, None).await;
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

use super::handshake::classify_tunnel_connect_error;
use tokio_tungstenite::tungstenite::handshake::server::Response as ServerResponse;
use tokio_tungstenite::tungstenite::http::{Request, StatusCode};

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
    let url = tunnel_connect_url("https://tako.website/api", "app.test", "a&b=c space").unwrap();
    assert_eq!(
        url,
        "wss://tako.website/api/v1/tunnels/connect?host=app.test&session=a%26b%3Dc+space"
    );
}

#[test]
fn tunnel_connect_error_treats_limit_as_terminal() {
    let error = classify_tunnel_connect_error(
        StatusCode::TOO_MANY_REQUESTS,
        Some(br#"{"error":"too many active tunnels for this machine (limit 5)"}"#),
    );

    assert_eq!(
        error,
        TunnelConnectError::Terminal {
            reason: TunnelCloseReason::LimitExceeded,
            message: "tunnel service refused connection (429 Too Many Requests): too many active tunnels for this machine (limit 5)".to_string(),
        }
    );
}

#[test]
fn invalidated_tunnel_session_stops_reconnecting() {
    assert!(matches!(
        classify_tunnel_connect_error(StatusCode::UNAUTHORIZED, None),
        TunnelConnectError::Terminal {
            reason: TunnelCloseReason::Replaced,
            ..
        }
    ));
}

#[test]
fn tunnel_connect_error_retries_other_http_failures() {
    let error = classify_tunnel_connect_error(StatusCode::BAD_GATEWAY, None);

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
        .set_private_key(&PKey::private_key_from_pem(cert.key_pem.as_bytes()).expect("parse key"))
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
        .set_private_key(&PKey::private_key_from_pem(cert.key_pem.as_bytes()).expect("parse key"))
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

    let client =
        local_proxy_client_with_root("app.test", &format!("127.0.0.1:{port}"), ca.ca_cert_pem())
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

#[tokio::test]
async fn tunneled_channel_and_presence_websockets_use_the_trusted_proxy() {
    use openssl::{
        pkey::PKey,
        ssl::{SslAcceptor, SslMethod},
        x509::X509,
    };
    use std::io::{Read, Write};
    let ca = tako::dev::LocalCA::generate().unwrap();
    let cert = ca.generate_leaf_cert("app.test").unwrap();
    let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    acceptor
        .set_certificate(&X509::from_pem(cert.cert_pem.as_bytes()).unwrap())
        .unwrap();
    acceptor
        .set_private_key(&PKey::private_key_from_pem(cert.key_pem.as_bytes()).unwrap())
        .unwrap();
    let acceptor = acceptor.build();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let paths = [
        "/_tako/channels/chat?room=lobby",
        "/_tako/presence/online?room=lobby",
    ];
    let server = tokio::task::spawn_blocking(move || {
        for path in paths {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stream = acceptor.accept(stream).unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap();
            assert!(
                headers.starts_with(&format!("GET {path} HTTP/1.1\r\n")),
                "{headers}"
            );
            assert!(
                headers.to_lowercase().contains("host: app.test:"),
                "{headers}"
            );
            // The proxy owns authorization; preserve its rejection to the browser.
            stream
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        }
    });
    let client = local_proxy_client_with_root("app.test", &address, ca.ca_cert_pem()).unwrap();
    for path in paths {
        let url = local_proxy_url("app.test", &address, path).unwrap();
        let (outbound, mut responses) = mpsc::channel(4);
        let (_sender, receiver) = mpsc::channel(4);
        relay_local_websocket(
            &client,
            &url,
            TunnelWebSocketOpenMessage {
                id: "test".into(),
                path: path.into(),
                headers: vec![],
            },
            &outbound,
            receiver,
        )
        .await
        .unwrap();
        let bytes = responses.recv().await.unwrap();
        assert!(
            matches!(rmp_serde::from_slice::<ClientMessage>(&bytes).unwrap(), ClientMessage::WebSocketResponse(response) if response.status == 403)
        );
    }
    server.await.unwrap();
}

#[test]
fn local_proxy_url_normalizes_paths() {
    let url = local_proxy_url("app.test", "127.0.0.1:47831", "api").unwrap();
    assert_eq!(url, "https://app.test:47831/api");
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
    let frame =
        tungstenite_message_to_tunnel_frame("ws-1", Message::Text("hello".into())).expect("frame");

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
                let offered: Vec<_> = request
                    .headers()
                    .get_all("sec-websocket-protocol")
                    .iter()
                    .map(|value| value.to_str().unwrap())
                    .collect();
                assert_eq!(offered, vec!["first", "vite-hmr"]);
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
        headers: vec![
            ("sec-websocket-protocol".into(), "first".into()),
            ("sec-websocket-protocol".into(), "vite-hmr".into()),
        ],
    };

    let (_to_local, from_tunnel) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        relay_local_websocket(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{upstream_port}{}", open.path),
            open,
            &outbound,
            from_tunnel,
        )
        .await
    });
    let ready = tokio::time::timeout(Duration::from_secs(1), inbound.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(rmp_serde::from_slice::<ClientMessage>(&ready).unwrap(), ClientMessage::WebSocketResponse(response) if response.status == 101 && response.headers.contains(&("sec-websocket-protocol".into(), "vite-hmr".into())))
    );
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

#[test]
fn websocket_upgrade_checks_accept_key_protocol_and_extensions() {
    let mut request = "ws://127.0.0.1:1234/".into_client_request().unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", "first, second".parse().unwrap());
    let good =
        tokio_tungstenite::tungstenite::handshake::server::create_response(&request).unwrap();
    assert!(valid_local_websocket_upgrade(
        request.headers(),
        good.headers()
    ));
    let mut selected = good.headers().clone();
    selected.insert("sec-websocket-protocol", "second".parse().unwrap());
    assert!(valid_local_websocket_upgrade(request.headers(), &selected));
    selected.append("sec-websocket-protocol", "second".parse().unwrap());
    assert!(!valid_local_websocket_upgrade(request.headers(), &selected));
    for (name, value) in [
        ("sec-websocket-accept", "wrong-key"),
        ("sec-websocket-protocol", "unrequested"),
        ("sec-websocket-extensions", "permessage-deflate"),
        ("upgrade", "h2c"),
        ("connection", "keep-alive"),
    ] {
        let mut headers = good.headers().clone();
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
        assert!(
            !valid_local_websocket_upgrade(request.headers(), &headers),
            "accepted invalid {name}"
        );
    }
}

#[tokio::test]
async fn local_websocket_may_accept_without_selecting_an_offered_protocol() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket.send(Message::Text("accepted".into())).await.unwrap();
        socket.close(None).await.unwrap();
    });
    let (outbound, mut inbound) = mpsc::channel(8);
    let (_sender, receiver) = mpsc::channel(8);
    relay_local_websocket(
        &reqwest::Client::new(),
        &format!("http://127.0.0.1:{port}/"),
        TunnelWebSocketOpenMessage {
            id: "no-protocol".into(),
            path: "/".into(),
            headers: vec![("sec-websocket-protocol".into(), "optional".into())],
        },
        &outbound,
        receiver,
    )
    .await
    .unwrap();
    let message = inbound.recv().await.unwrap();
    assert!(
        matches!(rmp_serde::from_slice::<ClientMessage>(&message).unwrap(), ClientMessage::WebSocketResponse(response) if response.status == 101)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn local_websocket_rejection_preserves_status_and_headers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
        }
        stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\nRetry-After: 5\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
    });
    let (outbound, mut incoming) = mpsc::channel(4);
    let (_sender, receiver) = mpsc::channel(8);
    relay_local_websocket(
        &reqwest::Client::new(),
        &format!("http://127.0.0.1:{upstream_port}/"),
        TunnelWebSocketOpenMessage {
            id: "rejected".into(),
            path: "/".into(),
            headers: vec![],
        },
        &outbound,
        receiver,
    )
    .await
    .unwrap();
    let message = incoming.recv().await.unwrap();
    assert!(
        matches!(rmp_serde::from_slice::<ClientMessage>(&message).unwrap(), ClientMessage::WebSocketResponse(response) if response.id == "rejected" && response.status == 403 && response.headers.contains(&("retry-after".into(), "5".into())))
    );
    assert!(
        incoming.try_recv().is_err(),
        "rejection must not produce frames"
    );
    server.await.unwrap();
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

pub(super) fn tunnel_state_with_app(
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
            compose: None,
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

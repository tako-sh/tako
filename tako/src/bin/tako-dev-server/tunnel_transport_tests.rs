use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::http::{Request, Response};

#[tokio::test]
async fn websocket_upgrade_uses_http_proxy_and_preserves_frames() {
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy.local_addr().unwrap();
    let relay = tokio::spawn(async move {
        let (stream, _) = proxy.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_hdr_async(
            stream,
            |request: &Request<()>, mut response: Response<()>| {
                assert_eq!(request.uri(), "http://relay.invalid/connect?session=test");
                assert_eq!(request.headers()["sec-websocket-protocol"], TUNNEL_PROTOCOL);
                response.headers_mut().insert(
                    "sec-websocket-protocol",
                    HeaderValue::from_static(TUNNEL_PROTOCOL),
                );
                Ok(response)
            },
        )
        .await
        .unwrap();
        // The first frame may arrive in the same read as the upgrade headers.
        socket
            .send(Message::Binary(vec![0, 1, 255].into()))
            .await
            .unwrap();
        let message = socket.next().await.unwrap().unwrap();
        assert_eq!(message, Message::Text("through proxy".into()));
        socket.send(message).await.unwrap();
    });
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
    child.args([
        "--exact",
        "tunnel::transport_tests::websocket_proxy_client",
        "--ignored",
        "--nocapture",
    ]);
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
        "REQUEST_METHOD",
    ] {
        child.env_remove(key);
    }
    child.env("HTTP_PROXY", format!("http://{proxy_addr}"));
    tokio::time::timeout(Duration::from_secs(10), async {
        let output = child.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        relay.await.unwrap();
    })
    .await
    .unwrap();
}

// Run in a subprocess to exercise production proxy discovery without changing
// environment variables underneath other tests in this process.
#[tokio::test]
#[ignore = "helper for websocket_upgrade_uses_http_proxy_and_preserves_frames"]
async fn websocket_proxy_client() {
    let mut socket = connect_tunnel_socket("ws://relay.invalid/connect?session=test")
        .await
        .unwrap();
    assert_eq!(
        socket.next().await.unwrap().unwrap(),
        Message::Binary(vec![0, 1, 255].into())
    );
    socket
        .send(Message::Text("through proxy".into()))
        .await
        .unwrap();
    assert_eq!(
        socket.next().await.unwrap().unwrap(),
        Message::Text("through proxy".into())
    );
}

#[tokio::test]
async fn direct_websocket_upgrade_still_works() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_hdr_async(
            stream,
            |_: &Request<()>, mut response: Response<()>| {
                response.headers_mut().insert(
                    "sec-websocket-protocol",
                    HeaderValue::from_static(TUNNEL_PROTOCOL),
                );
                Ok(response)
            },
        )
        .await
        .unwrap();
        socket.send(Message::Text("direct".into())).await.unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut socket =
            connect_tunnel_socket_with_client(&format!("ws://{address}/connect"), &client)
                .await
                .unwrap();
        assert_eq!(
            socket.next().await.unwrap().unwrap(),
            Message::Text("direct".into())
        );
        relay.await.unwrap();
    })
    .await
    .unwrap();
}

#[test]
fn upgrade_requires_valid_handshake_and_exact_protocol() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("upgrade", HeaderValue::from_static("WebSocket"));
    headers.insert(
        "connection",
        HeaderValue::from_static("keep-alive, Upgrade"),
    );
    headers.insert("sec-websocket-accept", HeaderValue::from_static("expected"));
    headers.insert(
        "sec-websocket-protocol",
        HeaderValue::from_static(TUNNEL_PROTOCOL),
    );
    assert_eq!(validate_tunnel_upgrade(&headers, "expected"), Ok(()));
    for name in [
        "upgrade",
        "connection",
        "sec-websocket-accept",
        "sec-websocket-protocol",
    ] {
        let mut invalid = headers.clone();
        invalid.remove(name);
        assert!(
            validate_tunnel_upgrade(&invalid, "expected").is_err(),
            "missing {name}"
        );
        invalid.insert(name, HeaderValue::from_static("wrong"));
        assert!(
            validate_tunnel_upgrade(&invalid, "expected").is_err(),
            "wrong {name}"
        );
    }
    headers.insert(
        "sec-websocket-extensions",
        HeaderValue::from_static("permessage-deflate"),
    );
    assert!(validate_tunnel_upgrade(&headers, "expected").is_err());
}

#[tokio::test]
async fn upgrade_http_errors_keep_limit_classification() {
    for (status, terminal) in [(429, true), (502, false)] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
                assert!(request.len() < 8192);
            }
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            connect_tunnel_socket_with_client(&format!("ws://{address}/"), &client),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(
            matches!(
                error,
                TunnelConnectError::Terminal {
                    reason: TunnelCloseReason::LimitExceeded,
                    ..
                }
            ),
            terminal
        );
        relay.await.unwrap();
    }
}

#[tokio::test]
async fn oversized_upgrade_error_bodies_are_capped() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
            assert!(request.len() < 8192);
        }
        stream
            .write_all(b"HTTP/1.1 429 Error\r\nContent-Length: 1000000000\r\n\r\n")
            .await
            .unwrap();
        // Stream body until the client gives up and drops the connection.
        let filler = vec![b'x'; 64 * 1024];
        while stream.write_all(&filler).await.is_ok() {}
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        connect_tunnel_socket_with_client(&format!("ws://{address}/"), &client),
    )
    .await
    .expect("errors without reading the full body")
    .unwrap_err();
    assert!(matches!(
        error,
        TunnelConnectError::Terminal {
            reason: TunnelCloseReason::LimitExceeded,
            ..
        }
    ));
    relay.await.unwrap();
}

#[tokio::test]
async fn connection_errors_do_not_expose_session_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let error = connect_tunnel_socket_with_client(
        &format!("ws://{address}/?session=secret-session"),
        &client,
    )
    .await
    .unwrap_err();
    assert!(!format!("{error:?}").contains("secret-session"));
}

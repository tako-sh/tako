//! Runs against the actual relay test executable built from tako-sh/tunnel.
//! The only fixtures are the local app endpoints behind the real client.

use super::*;
use tokio::net::TcpListener;

#[tokio::test]
#[ignore = "run tunnel/scripts/test-conformance.py with both checkouts"]
async fn real_relay_contract() {
    tokio::time::timeout(Duration::from_secs(60), check_contract())
        .await
        .expect("client/relay conformance timed out");
}

async fn check_contract() {
    let temp = tempfile::tempdir().unwrap();
    let ready = temp.path().join("relay-address");
    let mut relay = tokio::process::Command::new(
        std::env::var_os("TUNNEL_CONFORMANCE_RELAY_TEST_BIN").expect("relay test executable"),
    )
    .args([
        "--exact",
        "conformance::relay_fixture",
        "--ignored",
        "--nocapture",
    ])
    .env("TUNNEL_CONFORMANCE_READY", &ready)
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let address = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(address) = std::fs::read_to_string(&ready) {
                break address;
            }
            assert!(relay.try_wait().unwrap().is_none(), "relay fixture exited");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let base = format!("http://{address}");
    let identity = TakoIdentity::load_or_create().unwrap();
    let public_key = identity.public_key().unwrap();
    let created = register(&base, &identity, &public_key).await;

    let (snapshot, local_client, mut closed, http_fixture) = http_fixture().await;
    let (ws_port, ws_fixture) = websocket_fixture().await;
    let state = super::tests::tunnel_state_with_app(&snapshot.config_path, ws_port, true);
    let browser = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut sessions = tokio::task::JoinSet::new();
    connect(
        &mut sessions,
        &base,
        &created,
        &snapshot,
        &state,
        &local_client,
    )
    .await;
    wait_connected(&browser, &base, &created.host).await;

    let request = |path: &str| {
        browser
            .get(format!("{base}{path}"))
            .header("host", &created.host)
    };
    let upload = (0..131_111).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let response = browser
        .post(format!("{base}/echo"))
        .header("host", &created.host)
        .body(upload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), upload);
    let response = request("/large").send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), vec![42; 600_000]);

    // A live SSE must deliver immediately, leave other requests usable, and
    // close the local HTTP connection when the downstream consumer disappears.
    let mut sse = request("/sse").send().await.unwrap();
    assert_eq!(sse.status(), 200);
    assert_eq!(
        sse.chunk().await.unwrap().unwrap().as_ref(),
        b"data: first\n\n"
    );
    assert_eq!(
        request("/fast").send().await.unwrap().text().await.unwrap(),
        "fast"
    );
    drop(sse);
    assert!(
        closed.recv().await.unwrap(),
        "Cancel must close the app connection"
    );

    let mut socket = browser_socket(&address, &created.host).await;
    for message in [
        Message::Text("round trip".into()),
        Message::Binary(vec![0, 1, 255].into()),
    ] {
        socket.send(message.clone()).await.unwrap();
        assert_eq!(socket.next().await.unwrap().unwrap(), message);
    }
    let replacement = register(&base, &identity, &public_key).await;
    assert_eq!(
        replacement.host, created.host,
        "registration must preserve the URL"
    );
    connect(
        &mut sessions,
        &base,
        &replacement,
        &snapshot,
        &state,
        &local_client,
    )
    .await;
    assert!(matches!(
        sessions.join_next().await.unwrap().unwrap(),
        Err(TunnelConnectionEnd::Terminal {
            reason: TunnelCloseReason::Replaced,
            ..
        })
    ));
    acknowledge_close(&mut socket).await;
    wait_connected(&browser, &base, &created.host).await;
    assert_eq!(
        request("/fast").send().await.unwrap().text().await.unwrap(),
        "fast"
    );

    // SIGTERM must release streaming HTTP, browsers, and the client session.
    let mut socket = browser_socket(&address, &created.host).await;
    let mut sse = request("/sse").send().await.unwrap();
    assert_eq!(
        sse.chunk().await.unwrap().unwrap().as_ref(),
        b"data: first\n\n"
    );
    assert!(
        tokio::process::Command::new("kill")
            .args(["-TERM", &relay.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    acknowledge_close(&mut socket).await;
    assert!(
        sse.chunk().await.is_err(),
        "unfinished SSE must fail on shutdown"
    );
    assert!(matches!(
        sessions.join_next().await.unwrap().unwrap(),
        Ok(()) | Err(TunnelConnectionEnd::Retry(_))
    ));
    assert!(
        closed.recv().await.unwrap(),
        "shutdown must cancel the app stream"
    );
    assert!(
        relay.wait().await.unwrap().success(),
        "relay must drain and exit cleanly"
    );
    http_fixture.await.unwrap();
    ws_fixture.await.unwrap();
}

async fn register(base: &str, identity: &TakoIdentity, public_key: &str) -> CreatedTunnel {
    let challenge = create_tunnel_challenge(base, "conformance", public_key)
        .await
        .unwrap();
    let signature = identity
        .sign_tunnel(&challenge.nonce, "conformance", &challenge.host, public_key)
        .unwrap();
    let created = create_tunnel(
        base,
        "conformance",
        public_key,
        &challenge.nonce,
        &signature,
    )
    .await
    .unwrap();
    assert!(
        create_tunnel(
            base,
            "conformance",
            public_key,
            &challenge.nonce,
            &signature
        )
        .await
        .is_err(),
        "authenticated challenges must be single-use"
    );
    created
}

async fn connect(
    tasks: &mut tokio::task::JoinSet<Result<(), TunnelConnectionEnd>>,
    base: &str,
    created: &CreatedTunnel,
    snapshot: &TunnelSnapshot,
    state: &Arc<Mutex<State>>,
    client: &reqwest::Client,
) {
    let socket =
        connect_tunnel_socket(&tunnel_connect_url(base, &created.host, &created.session).unwrap())
            .await
            .unwrap();
    tasks.spawn(run_connection(
        socket,
        snapshot.clone(),
        state.clone(),
        snapshot.config_path.clone(),
        client.clone(),
    ));
}

async fn wait_connected(client: &reqwest::Client, base: &str, host: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let info: serde_json::Value = client
                .get(format!("{base}/v1/tunnels/{host}"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if info["connected"] == true {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

type BrowserSocket = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn browser_socket(address: &str, host: &str) -> BrowserSocket {
    let mut request = format!("ws://{address}/socket")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert("host", host.parse().unwrap());
    request
        .headers_mut()
        .insert("sec-websocket-protocol", "conformance".parse().unwrap());
    let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.headers()["sec-websocket-protocol"], "conformance");
    assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
    assert_eq!(
        socket.next().await.unwrap().unwrap(),
        Message::Text("hello".into())
    );
    socket
}

async fn acknowledge_close(socket: &mut BrowserSocket) {
    let Message::Close(Some(close)) = socket.next().await.unwrap().unwrap() else {
        panic!("expected browser close");
    };
    assert_eq!(u16::from(close.code), 1011);
    socket.flush().await.unwrap();
}

// Tungstenite's handshake callback requires its unboxed HTTP error response.
#[allow(clippy::result_large_err)]
async fn websocket_fixture() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let mut sockets = tokio::task::JoinSet::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            sockets.spawn(async move {
                let mut socket = tokio_tungstenite::accept_hdr_async(stream, |
                    request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                    mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    assert_eq!(request.headers()["sec-websocket-protocol"], "conformance");
                    response.headers_mut().insert("sec-websocket-protocol", "conformance".parse().unwrap());
                    for cookie in ["one=1", "two=2"] {
                        response.headers_mut().append("set-cookie", cookie.parse().unwrap());
                    }
                    Ok(response)
                }).await.unwrap();
                socket.send(Message::Text("hello".into())).await.unwrap();
                while let Some(Ok(message)) = socket.next().await {
                    if message.is_close() { let _ = socket.flush().await; break; }
                    socket.send(message).await.unwrap();
                }
            });
        }
        while let Some(result) = sockets.join_next().await {
            result.unwrap();
        }
    });
    (port, task)
}

async fn http_fixture() -> (
    TunnelSnapshot,
    reqwest::Client,
    mpsc::Receiver<bool>,
    tokio::task::JoinHandle<()>,
) {
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
    let acceptor = Arc::new(acceptor.build());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let client = local_proxy_client_with_root("app.test", &address, ca.ca_cert_pem()).unwrap();
    let (closed_tx, closed_rx) = mpsc::channel(2);
    let task = tokio::spawn(async move {
        let mut requests = tokio::task::JoinSet::new();
        for _ in 0..6 {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = stream.into_std().unwrap();
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let acceptor = acceptor.clone();
            let closed = closed_tx.clone();
            requests.spawn_blocking(move || {
                let mut stream = acceptor.accept(stream).unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                }
                let headers = String::from_utf8(headers).unwrap();
                let path = headers.split_whitespace().nth(1).unwrap();
                if path == "/sse" {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nd\r\ndata: first\n\n\r\n").unwrap();
                    stream.flush().unwrap();
                    let disconnected = match stream.read(&mut [0]) {
                        Ok(0) => true,
                        Err(error) => !matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
                        _ => false,
                    };
                    closed.blocking_send(disconnected).unwrap();
                } else {
                    let body = match path {
                        "/echo" => {
                            let length: usize = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(str::trim).map(str::to_owned)).unwrap().parse().unwrap();
                            let mut body = vec![0; length];
                            stream.read_exact(&mut body).unwrap();
                            body
                        },
                        "/large" => vec![42; 600_000],
                        "/fast" => b"fast".to_vec(),
                        _ => panic!("unexpected local path: {path}"),
                    };
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                    stream.write_all(&body).unwrap();
                }
            });
        }
        while let Some(result) = requests.join_next().await {
            result.unwrap();
        }
    });
    (
        TunnelSnapshot {
            config_path: "/conformance/tako.toml".into(),
            app_name: "conformance".into(),
            local_host: "app.test".into(),
            listen_addr: address,
        },
        client,
        closed_rx,
        task,
    )
}

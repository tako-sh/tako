use super::*;
use bytes::Bytes;
use pingora_core::protocols::l4::stream::Stream;
use pingora_proxy::Session;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// An app serving a `feed` definition with an integer `id` param over SSE and
/// WebSocket.
async fn feed_app() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut session = Session::new_h1(Box::new(Stream::from(socket)));
                session.read_request().await.unwrap();
                while session.read_request_body().await.unwrap().is_some() {}
                let data = match session.req_header().uri.path() {
                    "/channels/registry" => json!([{
                        "channel": "feed",
                        "paramsSchema": {
                            "type": "object",
                            "properties": {"id": {"type": "integer"}},
                            "required": ["id"],
                            "additionalProperties": false
                        },
                        "auth": false,
                        "transport": "ws"
                    }]),
                    "/channels/authorize" => {
                        json!({"ok": true, "transport": "ws", "maxConnectionLifetimeMs": 3000})
                    }
                    path => panic!("unexpected path {path}"),
                }
                .to_string();
                let mut header = ResponseHeader::build(200, None).unwrap();
                header
                    .insert_header("Content-Type", "application/json")
                    .unwrap();
                header
                    .insert_header("Content-Length", data.len().to_string())
                    .unwrap();
                session
                    .write_response_header(Box::new(header), false)
                    .await
                    .unwrap();
                session
                    .write_response_body(Some(Bytes::from(data)), true)
                    .await
                    .unwrap();
            });
        }
    });
    port
}

fn proxy_for_feed(root: &std::path::Path, app_port: u16) -> Arc<TakoProxy> {
    let manager = Arc::new(AppManager::new(root.into()));
    let app = manager.register_app(AppConfig {
        name: "feed-app".into(),
        version: "v1".into(),
        path: root.into(),
        ..Default::default()
    });
    let instance = app.allocate_instance();
    instance.set_port(app_port);
    app.set_instance_state(&instance, InstanceState::Healthy);
    let lb = Arc::new(LoadBalancer::new(manager));
    app.set_state(crate::socket::AppState::Running);
    lb.register_app(app);
    let mut routes = RouteTable::default();
    routes.set_app_routes("feed-app".into(), vec!["feed.example".into()]);
    Arc::new(TakoProxy::new(
        lb,
        Arc::new(RwLock::new(routes)),
        ProxyConfig {
            redirect_http_to_https: false,
            ..Default::default()
        },
        Arc::new(ColdStartManager::new(ColdStartConfig::default())),
        CloudflareIpRanges::default(),
    ))
}

async fn serve(proxy: Arc<TakoProxy>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let proxy = proxy.clone();
            tokio::spawn(async move {
                let mut session = Session::new_h1(Box::new(Stream::from(socket)));
                session.read_request().await.unwrap();
                let mut ctx = proxy.new_ctx();
                let _ = proxy.request_filter(&mut session, &mut ctx).await;
                proxy.logging(&mut session, None, &mut ctx).await;
            });
        }
    });
    addr
}

/// Send a request and read the whole response; a closed stream ends it long
/// before the 3 s connection lifetime.
async fn request(
    addr: std::net::SocketAddr,
    transport: &str,
    query: &str,
    headers: &str,
) -> Vec<u8> {
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(
            format!(
                "GET /_tako/channels/{transport}/feed?{query} HTTP/1.1\r\nHost: feed.example\r\n{headers}Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
        .await
        .expect("a closed stream ends the response")
        .unwrap();
    response
}

async fn sse(addr: std::net::SocketAddr, query: &str, headers: &str) -> String {
    String::from_utf8(request(addr, "sse", query, headers).await).unwrap()
}

fn data(response: &str) -> Vec<Value> {
    response
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

const UPGRADE: &str = "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n";

/// Server frames of an upgraded response: (opcode, payload).
fn websocket_frames(response: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let start = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("upgrade response")
        + 4;
    assert!(response.starts_with(b"HTTP/1.1 101"));
    let mut frames = Vec::new();
    let mut rest = &response[start..];
    while rest.len() >= 2 {
        let opcode = rest[0] & 0x0f;
        let (len, offset) = match rest[1] & 0x7f {
            126 => (u16::from_be_bytes([rest[2], rest[3]]) as usize, 4),
            127 => panic!("unexpectedly large frame"),
            len => (len as usize, 2),
        };
        frames.push((opcode, rest[offset..offset + len].to_vec()));
        rest = &rest[offset + len..];
    }
    frames
}

#[tokio::test]
async fn closed_streams_end_sse_and_websocket_readers_with_the_terminal_signal() {
    let temp = TempDir::new().unwrap();
    let addr = serve(proxy_for_feed(temp.path(), feed_app().await)).await;
    let store = tako_channels::ChannelStore::open(&crate::channels::app_channels_db_path(
        temp.path(),
        "feed-app",
    ))
    .unwrap();
    let publish = |id: i64, data: &str| {
        let publication = tako_channels::TrustedPublication::parse(
            "feed",
            &json!({"id": id}),
            json!({"type": "token", "data": data}),
        )
        .unwrap();
        store.publish_trusted(&publication).unwrap()
    };
    let first = publish(1, "hel");
    let second = publish(1, "lo");
    let other = publish(2, "open");
    let marker = store
        .publish_trusted(
            &tako_channels::TrustedPublication::close("feed", &json!({"id": 1}), None).unwrap(),
        )
        .unwrap();
    assert_eq!(marker.channel, "feed");
    let close_event = format!("id: {}\nevent: tako.close\ndata: {{}}\n\n", marker.id);

    // `from=start` replays every retained message, then the terminal event.
    let replay = sse(addr, "id=1&from=start", "").await;
    assert_eq!(
        data(&replay),
        vec![
            json!({"id": first.id, "eventId": first.event_id, "channel": "feed", "type": "token", "data": "hel"}),
            json!({"id": second.id, "eventId": second.event_id, "channel": "feed", "type": "token", "data": "lo"}),
            json!({}),
        ],
        "{replay}"
    );
    assert!(replay.contains(&close_event), "{replay}");
    assert!(!replay.contains("tako.stream"), "{replay}");
    // An explicit cursor wins over `from`; at the head or the marker, the
    // reader gets only the terminal event.
    for (query, headers, expected) in [
        (
            "id=1&from=start",
            format!("Last-Event-ID: {}\r\n", first.id),
            2,
        ),
        ("id=1", String::new(), 1),
        ("id=1", format!("Last-Event-ID: {}\r\n", marker.id), 1),
    ] {
        let response = sse(addr, query, &headers).await;
        assert_eq!(data(&response).len(), expected, "{query}: {response}");
        assert!(response.contains(&close_event), "{query}: {response}");
    }
    // `from` is reserved: it is not a param and takes only `start`.
    let invalid = sse(addr, "id=1&from=end", "").await;
    assert!(invalid.starts_with("HTTP/1.1 400"), "{invalid}");

    // WebSocket readers get the same messages and a stream-closed close frame.
    let frames = websocket_frames(&request(addr, "ws", "id=1&from=start", UPGRADE).await);
    assert_eq!(frames.len(), 3, "{frames:?}");
    for ((opcode, payload), expected) in frames.iter().zip([&first, &second]) {
        assert_eq!(*opcode, 0x1);
        let message: Value = serde_json::from_slice(payload).unwrap();
        assert_eq!(message["id"], expected.id.as_str());
        assert_eq!(message["channel"], "feed");
    }
    let mut close = 4200u16.to_be_bytes().to_vec();
    close.extend_from_slice(b"stream-closed");
    assert_eq!(frames[2], (0x8, close.clone()));
    for query in [
        "id=1".to_owned(),
        format!("id=1&from=start&last_message_id={}", second.id),
    ] {
        let frames = websocket_frames(&request(addr, "ws", &query, UPGRADE).await);
        assert_eq!(frames, vec![(0x8, close.clone())]);
    }

    // Another binding of the channel stays open and publishable.
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(
            b"GET /_tako/channels/sse/feed?id=2&from=start HTTP/1.1\r\nHost: feed.example\r\n\r\n",
        )
        .await
        .unwrap();
    let mut open = Vec::new();
    let read =
        tokio::time::timeout(Duration::from_millis(500), client.read_to_end(&mut open)).await;
    assert!(read.is_err(), "an open stream keeps its response open");
    let open = String::from_utf8(open).unwrap();
    assert_eq!(data(&open).len(), 1, "{open}");
    assert_eq!(data(&open)[0]["id"], other.id.as_str());
    assert!(!open.contains("tako.close"), "{open}");
    publish(2, "more");
    let rejected = tako_channels::TrustedPublication::parse(
        "feed",
        &json!({"id": 1}),
        json!({"type": "token", "data": "late"}),
    )
    .unwrap();
    assert!(matches!(
        store.publish_trusted(&rejected),
        Err(tako_channels::ChannelError::StreamClosed)
    ));
}

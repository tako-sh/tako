//! `tako dev` authorizes channel subscribe and connect through the app, with
//! production's request shape, credential handling and error mapping.

use super::test_app::{dev_proxy, fake_app, grant};
use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

#[tokio::test]
async fn shared_websocket_is_routed_and_authorized_by_the_dev_proxy() {
    use pingora_proxy::ProxyHttp;
    let (app_port, calls) = fake_app(definitions(), authorize).await;
    let routes = crate::proxy::Routes::default();
    routes.set_routes("app".into(), vec!["app.test".into()], app_port, true);
    routes.set_store_auth("app", "app.tako".into(), test_app::TOKEN.into());
    let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel();
    let proxy = crate::proxy::DevProxy {
        routes,
        events,
        channels: DevChannelStore::new(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut session = Session::new_h1(Box::new(
            pingora_core::protocols::l4::stream::Stream::from(socket),
        ));
        session.read_request().await.unwrap();
        assert!(
            proxy
                .request_filter(&mut session, &mut proxy.new_ctx())
                .await
                .unwrap()
        );
    });
    let mut request = format!("ws://{endpoint}/_tako/channels/ws")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("host", "app.test".parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket.send(Message::text(json!({"type":"subscribe", "id":"page", "channel":"room", "query":"room=1", "headers":{"Authorization":"Bearer good"}}).to_string())).await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame: Value = serde_json::from_str(next.to_text().unwrap()).unwrap();
    assert_eq!(frame, json!({"type":"subscribed", "id":"page"}));
    let call = calls.lock().unwrap()[0].clone();
    assert_eq!(call["operation"], "connect");
    assert_eq!(call["params"], json!({"room":1}));
    assert_eq!(call["header"]["value"], "good");
    socket.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

fn definitions() -> Value {
    json!([
        {
            "channel": "chat",
            "paramsSchema": {
                "type": "object",
                "properties": {"room": {"type": "integer"}},
                "required": ["room"],
                "additionalProperties": false
            },
            "auth": {"headerName": "authorization", "cookieName": "sid"},
            "maxConnectionLifetimeMs": 2000,
        },
        {
            "channel": "room",
            "paramsSchema": {
                "type": "object",
                "properties": {"room": {"type": "integer"}},
                "required": ["room"],
                "additionalProperties": false
            },
            "auth": {"headerName": "authorization", "cookieName": "sid"},
            "transport": "ws",
            "maxConnectionLifetimeMs": 3000,
        },
    ])
}

/// Allows the `good` token or cookie with a subject, answers `ok: false` for
/// `deny-body`, grants `sse-only` without the WebSocket transport, and denies
/// everything else with 403.
fn authorize(input: &Value) -> (u16, Value) {
    let definitions = definitions();
    let definition = definitions
        .as_array()
        .unwrap()
        .iter()
        .find(|definition| definition["channel"] == input["channel"])
        .unwrap();
    let credential = input["header"]["value"]
        .as_str()
        .or(input["cookie"].as_str());
    match credential {
        Some("good") => {
            let mut grant = grant(definition);
            grant["subject"] = json!("user-1");
            (200, grant)
        }
        Some("deny-body") => (200, json!({"ok": false})),
        Some("sse-only") => (200, json!({"ok": true, "maxConnectionLifetimeMs": 3000})),
        _ => (403, json!({"ok": false})),
    }
}

#[tokio::test]
async fn sse_subscribe_is_authorized_and_denied_by_the_app() {
    let (app_port, calls) = fake_app(definitions(), authorize).await;
    let dev_store = DevChannelStore::new();
    let (endpoint, _) = dev_proxy(app_port, &dev_store).await;
    let client = reqwest::Client::new();
    let subscribe = |channel: &str, header: Option<(&str, &str)>| {
        let mut request = client.get(format!(
            "http://{endpoint}/_tako/channels/sse/{channel}?room=1"
        ));
        if let Some((name, value)) = header {
            request = request.header(name, value);
        }
        request.send()
    };

    // Declared credentials are required before the app is asked.
    assert_eq!(subscribe("chat", None).await.unwrap().status(), 401);
    assert!(calls.lock().unwrap().is_empty());

    let allowed = subscribe("chat", Some(("authorization", "Bearer good")))
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200);
    assert_eq!(
        calls.lock().unwrap()[0],
        json!({
            "channel": "chat",
            "operation": "subscribe",
            "params": {"room": 1},
            "header": {"scheme": "Bearer", "value": "good"},
        })
    );
    drop(allowed);

    let cookie = subscribe("chat", Some(("cookie", "other=x; sid=good")))
        .await
        .unwrap();
    assert_eq!(cookie.status(), 200);
    assert_eq!(calls.lock().unwrap()[1]["cookie"], "good");
    drop(cookie);

    // A 403 from the app and an `ok: false` answer both deny with 403.
    for token in ["Bearer bad", "Bearer deny-body"] {
        let denied = subscribe("chat", Some(("authorization", token)))
            .await
            .unwrap();
        assert_eq!(denied.status(), 403, "{token}");
        assert_eq!(
            denied.json::<Value>().await.unwrap(),
            json!({"error": "Forbidden"})
        );
    }
    // A channel the app does not define is not served.
    let unknown = subscribe("missing", Some(("authorization", "Bearer good")))
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
}

#[tokio::test]
async fn transport_paths_reject_mismatches_before_authorization() {
    let (app_port, calls) = fake_app(definitions(), authorize).await;
    let (endpoint, _) = dev_proxy(app_port, &DevChannelStore::new()).await;
    for path in ["/_tako/channels/ws", "/_tako/channels/ws/room"] {
        assert_eq!(
            reqwest::get(format!("http://{endpoint}{path}"))
                .await
                .unwrap()
                .status(),
            426
        );
    }
    let result =
        tokio_tungstenite::connect_async(format!("ws://{endpoint}/_tako/channels/sse/room?room=1"))
            .await;
    match result {
        Err(tungstenite::Error::Http(response)) => assert_eq!(response.status().as_u16(), 400),
        other => panic!("expected rejection of WebSocket on SSE path: {other:?}"),
    }
    assert!(calls.lock().unwrap().is_empty());
}

async fn connect(
    endpoint: std::net::SocketAddr,
    channel: &str,
    headers: &[(&'static str, &str)],
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    u16,
> {
    let mut request = format!("ws://{endpoint}/_tako/channels/ws/{channel}?room=1")
        .into_client_request()
        .unwrap();
    for (name, value) in headers {
        request.headers_mut().insert(*name, value.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(error) => panic!("websocket connect failed: {error}"),
    }
}

/// Messages the dev store holds for `room` with `{room: 1}`, once `count` arrived.
async fn stored(dev_store: &DevChannelStore, app_id: &str, count: usize) -> Vec<Value> {
    let key = ChannelStream::new("room", &json!({"room": 1}))
        .key()
        .to_owned();
    let store = dev_store.store_for_app(app_id);
    for _ in 0..100 {
        let messages = store.read_after(&key, None, 100).unwrap_or_default();
        if messages.len() >= count {
            return messages.into_iter().map(|message| message.data).collect();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("expected {count} stored messages");
}

#[tokio::test]
async fn websocket_connect_uses_the_grant_and_first_frame_auth() {
    let (app_port, calls) = fake_app(definitions(), authorize).await;
    let dev_store = DevChannelStore::new();
    let (endpoint, app_id) = dev_proxy(app_port, &dev_store).await;
    let publish = |data: i64| Message::text(json!({"type": "msg", "data": data}).to_string());

    // Header auth on the upgrade; the grant allows client publishes.
    let mut socket = connect(endpoint, "room", &[("authorization", "Bearer good")])
        .await
        .unwrap();
    socket.send(publish(1)).await.unwrap();
    assert_eq!(stored(&dev_store, &app_id, 1).await, vec![json!(1)]);
    assert_eq!(calls.lock().unwrap()[0]["operation"], "connect");
    socket.close(None).await.unwrap();

    // Without the header, the token arrives in the first `tako.auth` frame.
    let mut socket = connect(endpoint, "room", &[]).await.unwrap();
    socket
        .send(Message::text(
            r#"{"type":"tako.auth","token":"Bearer good"}"#,
        ))
        .await
        .unwrap();
    socket.send(publish(2)).await.unwrap();
    assert_eq!(
        stored(&dev_store, &app_id, 2).await,
        vec![json!(1), json!(2)]
    );
    let call = calls.lock().unwrap().last().unwrap().clone();
    assert_eq!(call["operation"], "connect");
    assert_eq!(call["params"], json!({"room": 1}));
    assert_eq!(call["header"], json!({"scheme": "Bearer", "value": "good"}));
    socket.close(None).await.unwrap();

    // A denied first frame closes the socket with verify-rejected, before
    // anything the socket sent is published.
    let mut socket = connect(endpoint, "room", &[]).await.unwrap();
    socket
        .send(Message::text(
            r#"{"type":"tako.auth","token":"Bearer bad"}"#,
        ))
        .await
        .unwrap();
    let _ = socket.send(publish(3)).await;
    let close = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(frame))) => return frame,
                Some(Ok(_)) => continue,
                other => panic!("expected a close frame, got {other:?}"),
            }
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(u16::from(close.code), 4403);
    assert_eq!(close.reason.as_str(), "verify-rejected");

    // A malformed first frame closes with auth-frame-malformed.
    let mut socket = connect(endpoint, "room", &[]).await.unwrap();
    socket.send(Message::text("not json")).await.unwrap();
    match socket.next().await {
        Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 4400),
        other => panic!("expected a close frame, got {other:?}"),
    }

    // Rejections on the upgrade request map to production's HTTP statuses.
    assert_eq!(
        connect(endpoint, "room", &[("authorization", "Bearer bad")])
            .await
            .err(),
        Some(403)
    );
    // The app granted the connection without the WebSocket transport, so the
    // socket is never upgraded and cannot publish.
    assert_eq!(
        connect(endpoint, "room", &[("authorization", "Bearer sse-only")])
            .await
            .err(),
        Some(400)
    );
    // An SSE-only channel refuses sockets before asking the app.
    let asked = calls.lock().unwrap().len();
    assert_eq!(
        connect(endpoint, "chat", &[("authorization", "Bearer good")])
            .await
            .err(),
        Some(400)
    );
    assert_eq!(calls.lock().unwrap().len(), asked);
    // Cookie sockets need a same-origin handshake, as in production.
    assert_eq!(
        connect(endpoint, "room", &[("cookie", "sid=good")])
            .await
            .err(),
        Some(403)
    );
    let origin = format!("https://{endpoint}");
    // The scheme declares a header, so the cookie is forwarded with the first
    // frame, which may carry no token.
    let mut socket = connect(
        endpoint,
        "room",
        &[("cookie", "sid=good"), ("origin", &origin)],
    )
    .await
    .unwrap();
    socket
        .send(Message::text(r#"{"type":"tako.auth"}"#))
        .await
        .unwrap();
    socket.send(publish(4)).await.unwrap();
    assert_eq!(
        stored(&dev_store, &app_id, 3).await,
        vec![json!(1), json!(2), json!(4)]
    );
    let call = calls.lock().unwrap().last().unwrap().clone();
    assert_eq!(call["cookie"], "good");
    assert!(call.get("header").is_none(), "{call}");
}

#[tokio::test]
async fn unreachable_app_fails_with_a_clear_error_instead_of_allowing() {
    let closed_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let (failing_port, _) = fake_app(definitions(), |_| (500, json!({}))).await;
    for app_port in [closed_port, failing_port] {
        let dev_store = DevChannelStore::new();
        let (endpoint, _) = dev_proxy(app_port, &dev_store).await;
        let response = reqwest::Client::new()
            .get(format!("http://{endpoint}/_tako/channels/sse/chat?room=1"))
            .header("authorization", "Bearer good")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503, "{app_port}");
        let body = response.json::<Value>().await.unwrap();
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("Channel authorization unavailable"),
            "{body}"
        );
        assert_eq!(
            connect(endpoint, "room", &[("authorization", "Bearer good")])
                .await
                .err(),
            Some(503)
        );
    }
}

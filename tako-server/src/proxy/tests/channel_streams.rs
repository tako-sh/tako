use super::*;
use bytes::Bytes;
use pingora_core::protocols::l4::stream::Stream;
use pingora_proxy::Session;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// An app serving a `chat` definition with an integer `room` param, and
/// recording what each authorization request asked for.
async fn chat_app() -> (u16, Arc<parking_lot::Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let authorized = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let seen = authorized.clone();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut session = Session::new_h1(Box::new(Stream::from(socket)));
                session.read_request().await.unwrap();
                let mut body = Vec::new();
                while let Some(chunk) = session.read_request_body().await.unwrap() {
                    body.extend_from_slice(&chunk);
                }
                let data = match session.req_header().uri.path() {
                    "/channels/registry" => json!([{
                        "channel": "chat",
                        "paramsSchema": {
                            "type": "object",
                            "properties": {"room": {"type": "integer"}},
                            "required": ["room"],
                            "additionalProperties": false
                        },
                        "auth": false
                    }, {
                        // Silent on additionalProperties: undeclared params are
                        // still not params.
                        "channel": "open",
                        "paramsSchema": {
                            "type": "object",
                            "properties": {"room": {"type": "integer"}}
                        },
                        "auth": false
                    }]),
                    "/channels/authorize" => {
                        seen.lock()
                            .push(serde_json::from_slice::<Value>(&body).unwrap());
                        json!({"ok": true, "maxConnectionLifetimeMs": 700})
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
    (port, authorized)
}

fn proxy_for_chat(root: &std::path::Path, app_port: u16) -> Arc<TakoProxy> {
    let manager = Arc::new(AppManager::new(root.into()));
    let app = manager.register_app(AppConfig {
        name: "chat-app".into(),
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
    routes.set_app_routes("chat-app".into(), vec!["chat.example".into()]);
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

/// Serve every connection through the proxy's request filter.
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

async fn get(addr: std::net::SocketAddr, query: &str) -> TcpStream {
    get_channel(addr, "chat", query).await
}

async fn get_channel(addr: std::net::SocketAddr, channel: &str, query: &str) -> TcpStream {
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(
            format!(
                "GET /_tako/channels/sse/{channel}?{query} HTTP/1.1\r\nHost: chat.example\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    client
}

/// Read a response's status line and headers, leaving the body unread.
async fn response_head(client: &mut TcpStream) -> String {
    let mut head = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !head.ends_with(b"\r\n\r\n") {
            head.push(client.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    String::from_utf8(head).unwrap()
}

async fn read_all(mut client: TcpStream) -> String {
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(response).unwrap()
}

fn messages(response: &str) -> Vec<Value> {
    response
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

#[tokio::test]
async fn sse_subscribers_receive_only_the_stream_their_params_select() {
    let temp = TempDir::new().unwrap();
    let (app_port, authorized) = chat_app().await;
    let proxy = proxy_for_chat(temp.path(), app_port);
    let addr = serve(proxy).await;

    let invalid = read_all(get(addr, "room=one").await).await;
    assert!(invalid.starts_with("HTTP/1.1 400"), "{invalid}");

    // The proxy writes the response head only after resolving where the
    // reader starts, so once it arrives a publish cannot slip before it.
    let (mut one, mut two) = (get(addr, "room=1").await, get(addr, "room=2").await);
    let heads = [response_head(&mut one).await, response_head(&mut two).await];
    for head in &heads {
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    }
    let store = tako_channels::ChannelStore::open(&crate::channels::app_channels_db_path(
        temp.path(),
        "chat-app",
    ))
    .unwrap();
    let publish = |params: Value, data: i64| {
        let publication = tako_channels::TrustedPublication::parse(
            "chat",
            &params,
            json!({"type": "msg", "data": data}),
        )
        .unwrap();
        store.publish_trusted(&publication).unwrap()
    };
    publish(json!({"room": 2}), 2);
    let first = publish(json!({"room": 1.0}), 1);
    publish(json!({}), 0);

    let (one, two) = (read_all(one).await, read_all(two).await);
    assert_eq!(
        messages(&one),
        vec![
            json!({"id": first.id, "eventId": first.event_id, "channel": "chat", "type": "msg", "data": 1})
        ],
        "{one}"
    );
    assert_eq!(messages(&two).len(), 1, "{two}");
    assert_eq!(messages(&two)[0]["data"], 2);
    for response in [&one, &two] {
        assert!(!response.contains("tako.stream"), "{response}");
    }
    // The app authorizes the public channel name with the coerced params.
    let mut rooms: Vec<Value> = authorized
        .lock()
        .iter()
        .map(|request| {
            assert_eq!(request["channel"], "chat");
            request["params"].clone()
        })
        .collect();
    rooms.sort_by_key(|params| params["room"].as_i64());
    assert_eq!(rooms, vec![json!({"room": 1}), json!({"room": 2})]);
}

#[tokio::test]
async fn sse_cursors_from_one_binding_reset_on_another() {
    let temp = TempDir::new().unwrap();
    let (app_port, _) = chat_app().await;
    let proxy = proxy_for_chat(temp.path(), app_port);
    let addr = serve(proxy).await;
    let store = tako_channels::ChannelStore::open(&crate::channels::app_channels_db_path(
        temp.path(),
        "chat-app",
    ))
    .unwrap();
    let publication = tako_channels::TrustedPublication::parse(
        "chat",
        &json!({"room": 1}),
        json!({"type": "msg", "data": 1}),
    )
    .unwrap();
    let message = store.publish_trusted(&publication).unwrap();
    for (query, resumes) in [("room=1", true), ("room=2", false)] {
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(
                format!(
                    "GET /_tako/channels/sse/chat?{query} HTTP/1.1\r\nHost: chat.example\r\nLast-Event-ID: {}\r\nConnection: close\r\n\r\n",
                    message.id
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let response = read_all(client).await;
        assert_eq!(
            response.contains("event: tako.reset") || response.starts_with("HTTP/1.1 410"),
            !resumes,
            "{query}: {response}"
        );
    }
}

#[tokio::test]
async fn undeclared_params_are_rejected_before_they_start_a_stream() {
    let temp = TempDir::new().unwrap();
    let (app_port, authorized) = chat_app().await;
    let proxy = proxy_for_chat(temp.path(), app_port);
    let addr = serve(proxy).await;

    for (channel, query) in [
        ("open", "room=1&junk=a"),
        ("open", "junk=a"),
        ("chat", "room=1&x=1"),
    ] {
        let response = read_all(get_channel(addr, channel, query).await).await;
        assert!(
            response.starts_with("HTTP/1.1 400"),
            "{channel}?{query}: {response}"
        );
        assert!(response.contains("unknown param"), "{response}");
    }
    assert!(
        authorized.lock().is_empty(),
        "rejected before authorization"
    );
    let streams = || {
        rusqlite::Connection::open(crate::channels::app_channels_db_path(
            temp.path(),
            "chat-app",
        ))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM channel_metadata WHERE channel LIKE 'tako.stream:%'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    };
    assert_eq!(streams(), 0, "no stream bookkeeping for rejected params");

    // Reserved names are not params: `from` still selects where a reader starts.
    let response = read_all(get_channel(addr, "open", "room=1&from=start").await).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(streams(), 1);
}

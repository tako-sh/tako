use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Fixture {
    address: std::net::SocketAddr,
    store: Arc<ChannelStore>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    app: Arc<tokio::sync::RwLock<AppAuthorizer>>,
}

#[async_trait::async_trait]
impl AppResolver for tokio::sync::RwLock<AppAuthorizer> {
    async fn resolve(&self) -> std::result::Result<AppAuthorizer, crate::ChannelError> {
        Ok(self.read().await.clone())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Fixture {
    async fn start() -> Self {
        let app_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = app_listener.local_addr().unwrap().to_string();
        let app_task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = app_listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        tasks.spawn(async move {
                            let mut socket = BufReader::new(socket);
                            let mut first = String::new(); socket.read_line(&mut first).await.unwrap();
                            let mut length = 0;
                            loop {
                                let mut line = String::new(); socket.read_line(&mut line).await.unwrap();
                                if line == "\r\n" || line.is_empty() { break; }
                                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") { length = value.trim().parse().unwrap(); }
                            }
                            let mut body = vec![0; length]; socket.read_exact(&mut body).await.unwrap();
                            let response = if first.starts_with("GET /channels/registry") {
                                let schema = json!({"type":"object", "properties":{"room":{"type":"string"}}, "additionalProperties":false});
                                json!([
                                    {"channel":"news", "paramsSchema":schema, "auth":false, "transport":"ws"},
                                    {"channel":"announcements", "paramsSchema":schema, "auth":false},
                                    {"channel":"chat", "paramsSchema":schema, "auth":false, "transport":"ws"},
                                    {"channel":"private", "paramsSchema":schema, "auth":{"headerName":"Authorization"}, "transport":"ws"},
                                    {"channel":"api-key", "paramsSchema":schema, "auth":{"headerName":"X-API-Key"}, "transport":"ws"},
                                    {"channel":"tako.presence:room", "paramsSchema":schema, "auth":false, "transport":"ws"},
                                    {"channel":"tako.store:items", "paramsSchema":{"type":"object"}, "auth":false}
                                ])
                            } else {
                                let request: Value = serde_json::from_slice(&body).unwrap();
                                if matches!(request["channel"].as_str(), Some("private" | "api-key")) && request["header"]["value"] != "valid" {
                                    json!({"ok":false})
                                } else {
                                    let mut auth = json!({"ok":true,"subject":"trusted", "maxConnectionLifetimeMs":60000});
                                    if request["operation"] == "connect" && request["channel"] != "announcements" { auth["transport"] = "ws".into(); }
                                    auth
                                }
                            }.to_string();
                            let header = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nx-tako-internal-token: test-secret\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len());
                            socket.write_all(header.as_bytes()).await.unwrap();
                            socket.write_all(response.as_bytes()).await.unwrap();
                            socket.flush().await.unwrap();
                        });
                    }
                    _ = tasks.join_next(), if !tasks.is_empty() => {}
                }
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let store = Arc::new(ChannelStore::open_in_memory().unwrap());
        let server_store = store.clone();
        let app = Arc::new(tokio::sync::RwLock::new(AppAuthorizer {
            endpoint,
            internal_host: "app.tako".into(),
            internal_token_header: "x-tako-internal-token",
            internal_token: "test-secret".into(),
        }));
        let server_app = app.clone();
        let task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let store = server_store.clone();
                        let app = server_app.clone();
                        tasks.spawn(async move {
                            let mut session = Session::new_h1(Box::new(pingora_core::protocols::l4::stream::Stream::from(socket)));
                            session.read_request().await.unwrap();
                            let writer = Arc::new(crate::LocalChannelWriter::new(store.clone()));
                            let _ = serve(&mut session, app, store, writer, false).await;
                        });
                    }
                    _ = tasks.join_next(), if !tasks.is_empty() => {}
                }
            }
        });
        Self {
            address,
            store,
            tasks: vec![task, app_task],
            app,
        }
    }
    async fn connect(&self) -> Client {
        connect_async(format!("ws://{}{PATH}", self.address))
            .await
            .unwrap()
            .0
    }
    fn publish(&self, name: &str, room: &str, data: Value) -> crate::ChannelMessage {
        let stream = crate::ChannelStream::new(name, &json!({"room":room}));
        self.store
            .append(
                stream.key(),
                &crate::ChannelPublishPayload {
                    r#type: "message".into(),
                    data,
                },
            )
            .unwrap()
    }
}

#[tokio::test]
async fn tokens_use_the_declared_auth_header_and_explicit_headers_win() {
    let fixture = Fixture::start().await;
    let mut client = fixture.connect().await;
    let mut request = subscribe("token", "api-key", "a");
    request["token"] = "valid".into();
    send(&mut client, request).await;
    assert_eq!(frame(&mut client).await["type"], "subscribed");

    let mut request = subscribe("explicit", "api-key", "b");
    request["token"] = "invalid".into();
    request["headers"] = json!({"x-api-key":"valid"});
    send(&mut client, request).await;
    assert_eq!(frame(&mut client).await["type"], "subscribed");

    let mut request = subscribe("denied", "api-key", "c");
    request["token"] = "valid".into();
    request["headers"] = json!({"X-API-Key":"invalid"});
    send(&mut client, request).await;
    assert_eq!(frame(&mut client).await["code"], 4403);
}

async fn send(client: &mut Client, value: Value) {
    client.send(Message::text(value.to_string())).await.unwrap();
}
async fn frame(client: &mut Client) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client
                .next()
                .await
                .expect("socket ended")
                .expect("socket error")
            {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(_) => {
                    client.flush().await.unwrap();
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    })
    .await
    .expect("frame timeout")
}
fn subscribe(id: &str, channel: &str, room: &str) -> Value {
    json!({"type":"subscribe","id":id,"channel":channel,"query":format!("room={room}"),"from":"start"})
}
async fn data(client: &mut Client, id: &str) -> Value {
    let mut text = String::new();
    loop {
        let frame = frame(client).await;
        assert_eq!(frame["id"], id);
        if frame["type"] == "subscribed" {
            continue;
        }
        assert_eq!(frame["type"], "data");
        text.push_str(frame["data"].as_str().unwrap());
        assert!(frame.to_string().len() <= MAX_FRAME_BYTES);
        if frame["end"] == true {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

#[tokio::test]
async fn shared_replay_is_scoped_and_terminal_close_keeps_other_subscriptions_alive() {
    let fixture = Fixture::start().await;
    let old = fixture.publish("news", "a", json!("old"));
    fixture.publish("news", "b", json!("other"));
    let mut client = fixture.connect().await;
    send(&mut client, subscribe("a", "news", "a")).await;
    assert_eq!(data(&mut client, "a").await["id"], old.id);
    send(&mut client, subscribe("b", "news", "b")).await;
    assert_eq!(data(&mut client, "b").await["data"], "other");
    let closed = crate::TrustedPublication::close("news", &json!({"room":"a"}), None).unwrap();
    fixture.store.publish_trusted(&closed).unwrap();
    let close = frame(&mut client).await;
    assert_eq!(close["id"], "a");
    assert_eq!(close["code"], 4200);
    fixture.publish("news", "b", json!("still live"));
    assert_eq!(data(&mut client, "b").await["data"], "still live");
    send(&mut client, json!({"type":"unsubscribe","id":"b"})).await;
    assert_eq!(frame(&mut client).await["type"], "unsubscribed");
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn authorization_and_publish_permissions_are_per_subscription() {
    let fixture = Fixture::start().await;
    let mut client = fixture.connect().await;
    let mut rejected = subscribe("denied", "private", "a");
    rejected["headers"] = json!({"Authorization":"Bearer wrong"});
    send(&mut client, rejected).await;
    let rejected = frame(&mut client).await;
    assert_eq!(rejected["type"], "closed");
    assert_eq!(rejected["id"], "denied");
    let mut accepted = subscribe("allowed", "private", "a");
    accepted["headers"] = json!({"Authorization":"Bearer valid"});
    send(&mut client, accepted).await;
    assert_eq!(frame(&mut client).await["type"], "subscribed");
    send(
        &mut client,
        json!({"type":"send","id":"allowed","data":{"type":"message","data":42}}),
    )
    .await;
    assert_eq!(data(&mut client, "allowed").await["data"], 42);
    send(&mut client, subscribe("reader", "announcements", "a")).await;
    assert_eq!(frame(&mut client).await["code"], 4403);
    send(
        &mut client,
        json!({"type":"send","id":"allowed","data":{"type":"message","data":43}}),
    )
    .await;
    assert_eq!(data(&mut client, "allowed").await["data"], 43);
}

#[tokio::test]
async fn unsubscribe_removes_presence_without_closing_the_shared_socket() {
    let fixture = Fixture::start().await;
    let mut client = fixture.connect().await;
    send(&mut client, subscribe("page", "tako.presence:room", "a")).await;
    assert_eq!(
        data(&mut client, "page").await["type"],
        "tako.presence.ready"
    );
    send(&mut client, json!({"type":"send","id":"page","data":{"type":"tako.presence","subject":"forged","data":{"viewing":true}}})).await;
    let snapshot = data(&mut client, "page").await;
    assert_eq!(snapshot["members"][0]["subject"], "trusted");
    send(&mut client, json!({"type":"unsubscribe","id":"page"})).await;
    assert_eq!(frame(&mut client).await["type"], "unsubscribed");
    let scope = crate::presence_scope("tako.presence:room", &json!({"room":"a"}));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !fixture
            .store
            .presence(&scope, crate::store::now_unix_ms())
            .unwrap()
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.publish("news", "a", json!("still connected"));
    send(&mut client, subscribe("next-page", "news", "a")).await;
    assert_eq!(
        data(&mut client, "next-page").await["data"],
        "still connected"
    );
}

#[tokio::test]
async fn large_utf8_events_are_chunked_and_oversized_events_close_only_their_subscription() {
    let fixture = Fixture::start().await;
    let large = "🦑".repeat(50_000);
    fixture.publish("news", "a", json!(large));
    let mut client = fixture.connect().await;
    send(&mut client, subscribe("a", "news", "a")).await;
    assert_eq!(data(&mut client, "a").await["data"], large);
    fixture.publish("news", "a", json!("x".repeat(MAX_MESSAGE_BYTES)));
    let closed = frame(&mut client).await;
    assert_eq!(closed["id"], "a");
    assert_eq!(closed["code"], 1009);
    fixture.publish("news", "b", json!(1));
    send(&mut client, subscribe("b", "news", "b")).await;
    assert_eq!(data(&mut client, "b").await["data"], 1);
}

#[tokio::test]
async fn subscribed_means_live_delivery_is_ready_without_a_replay_cursor() {
    let fixture = Fixture::start().await;
    let mut client = fixture.connect().await;
    // A live reader must not skip a publication made immediately after its ack.
    for index in 0..10 {
        let id = index.to_string();
        let mut request = subscribe(&id, "news", &id);
        request.as_object_mut().unwrap().remove("from");
        send(&mut client, request).await;
        assert_eq!(frame(&mut client).await["type"], "subscribed");
        fixture.publish("news", &id, json!(index));
        assert_eq!(data(&mut client, &id).await["data"], index);
        send(&mut client, json!({"type":"unsubscribe", "id":id})).await;
        assert_eq!(frame(&mut client).await["type"], "unsubscribed");
    }
}

#[tokio::test]
async fn stores_cannot_join_the_shared_websocket() {
    let fixture = Fixture::start().await;
    let mut client = fixture.connect().await;
    send(
        &mut client,
        json!({"type":"subscribe", "id":"store", "channel":"tako.store:items"}),
    )
    .await;
    assert_eq!(frame(&mut client).await["code"], 4403);
}

#[tokio::test]
async fn unavailable_authorizer_reconnects_to_resolve_the_current_app() {
    let fixture = Fixture::start().await;
    let mut client = fixture.connect().await;
    send(&mut client, subscribe("layout", "news", "a")).await;
    assert_eq!(frame(&mut client).await["type"], "subscribed");
    fixture.tasks[1].abort();
    tokio::task::yield_now().await;
    send(&mut client, subscribe("page", "news", "b")).await;
    assert_eq!(frame(&mut client).await["code"], 1013);
    let closed = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(closed, Message::Close(Some(close)) if close.code == tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Restart)
    );
}

#[tokio::test]
async fn new_subscriptions_resolve_the_current_deployment_on_an_existing_socket() {
    let fixture = Fixture::start().await;
    let mut client = fixture.connect().await;
    send(&mut client, subscribe("layout", "news", "a")).await;
    assert_eq!(frame(&mut client).await["type"], "subscribed");
    let replacement = Fixture::start().await;
    *fixture.app.write().await = replacement.app.read().await.clone();
    fixture.tasks[1].abort();
    tokio::task::yield_now().await;
    send(&mut client, subscribe("page", "news", "b")).await;
    assert_eq!(
        frame(&mut client).await,
        json!({"type":"subscribed", "id":"page"})
    );
    fixture.publish("news", "a", json!("layout survives deployment"));
    assert_eq!(
        data(&mut client, "layout").await["data"],
        "layout survives deployment"
    );
}

#[tokio::test]
async fn cancelled_subscription_ids_can_be_reused_without_stale_replay() {
    let fixture = Fixture::start().await;
    fixture.publish("news", "old", json!("stale"));
    fixture.publish("news", "new", json!("current"));
    let mut client = fixture.connect().await;
    // Put cancellation in the same physical read as subscribe, while auth is
    // still pending, and immediately reuse the logical ID for another scope.
    client
        .feed(Message::text(subscribe("page", "news", "old").to_string()))
        .await
        .unwrap();
    client
        .feed(Message::text(
            json!({"type":"unsubscribe", "id":"page"}).to_string(),
        ))
        .await
        .unwrap();
    client
        .feed(Message::text(subscribe("page", "news", "new").to_string()))
        .await
        .unwrap();
    client.flush().await.unwrap();
    assert_eq!(frame(&mut client).await["type"], "unsubscribed");
    assert_eq!(data(&mut client, "page").await["data"], "current");
}

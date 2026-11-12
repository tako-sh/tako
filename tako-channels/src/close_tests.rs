//! A stream can be closed by a durable terminal marker and read from its start.
use crate::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn payload(data: Value) -> ChannelPublishPayload {
    ChannelPublishPayload {
        r#type: "msg".into(),
        data,
    }
}

fn room(name: &str) -> ChannelStream {
    ChannelStream::new("chat", &json!({ "room": name }))
}

fn cursor(message: &ChannelMessage) -> ReplayCursor {
    message.id.parse().unwrap()
}

fn auth(value: Value) -> ChannelAuthResponse {
    serde_json::from_value(value).unwrap()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Storage-level contract shared by both backends.
fn close_is_durable_final_and_per_stream(store: &ChannelStore) {
    let (a, b) = (room("a"), room("b"));
    let lifecycle = auth(json!({"ok": true}));
    for stream in [&a, &b] {
        store.sync_channel(stream.key(), &lifecycle).unwrap();
    }
    let first = store.append(a.key(), &payload(json!("a1"))).unwrap();
    let second = store.append(a.key(), &payload(json!("a2"))).unwrap();
    store.append(b.key(), &payload(json!("b1"))).unwrap();
    assert_eq!(store.close_marker(a.key()).unwrap(), None);

    // Reading without a cursor is reading from the start.
    assert_eq!(
        store.read_after(a.key(), None, 10).unwrap(),
        vec![first.clone(), second.clone()]
    );

    let mut changes = store.changes(a.key());
    changes.borrow_and_update();
    let marker = store.close(a.key()).unwrap();
    assert!(is_close_marker(&marker));
    assert_eq!(marker.data, Value::Null);
    assert!(cursor(&marker).position > cursor(&second).position);
    assert!(changes.has_changed().unwrap(), "close wakes subscribers");

    // Closing again is a success that changes nothing.
    changes.borrow_and_update();
    assert_eq!(store.close(a.key()).unwrap().id, marker.id);
    assert!(!changes.has_changed().unwrap());

    // The marker is the last entry of the stream's replay, at its head.
    assert_eq!(store.close_marker(a.key()).unwrap(), Some(marker.clone()));
    assert_eq!(
        store.read_after(a.key(), None, 10).unwrap(),
        vec![first.clone(), second.clone(), marker.clone()]
    );
    assert_eq!(
        store
            .read_after(a.key(), Some(cursor(&second)), 10)
            .unwrap(),
        vec![marker.clone()]
    );
    assert!(
        store
            .read_after(a.key(), Some(cursor(&marker)), 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.replay_cursor(a.key(), None).unwrap(),
        Some(cursor(&marker))
    );
    assert_eq!(
        store.replay_cursor(a.key(), Some(cursor(&marker))).unwrap(),
        Some(cursor(&marker))
    );

    // Publishes to the closed stream fail by name; other bindings stay open.
    assert!(matches!(
        store.append(a.key(), &payload(json!("late"))),
        Err(ChannelError::StreamClosed)
    ));
    assert_eq!(store.read_after(a.key(), None, 10).unwrap().len(), 3);
    assert_eq!(store.close_marker(b.key()).unwrap(), None);
    assert_eq!(store.close_marker("chat").unwrap(), None);
    store.append(b.key(), &payload(json!("b2"))).unwrap();
    store.append("chat", &payload(json!("bare"))).unwrap();
}

#[test]
fn sqlite_close_is_durable_final_and_per_stream() {
    close_is_durable_final_and_per_stream(&ChannelStore::open_in_memory().unwrap());
}

fn postgres_url() -> Option<String> {
    std::env::var("TAKO_TEST_POSTGRES_URL").ok()
}

fn drop_schema(url: &str, schema: &str) {
    postgres::Client::connect(url, postgres::NoTls)
        .unwrap()
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}

#[test]
fn postgres_close_is_durable_final_and_per_stream() {
    let Some(url) = postgres_url() else {
        return;
    };
    let schema = format!("channel_close_{}", std::process::id());
    let store = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    close_is_durable_final_and_per_stream(&store);
    drop(store);
    drop_schema(&url, &schema);
}

#[test]
fn postgres_close_is_shared_by_every_server_and_survives_restart() {
    let Some(url) = postgres_url() else {
        return;
    };
    let schema = format!("channel_close_shared_{}", std::process::id());
    let (a, b) = (room("a"), room("b"));
    let marker = {
        let one = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
        let two = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
        let other_app = ChannelStore::open_postgres_with_schema(&url, &schema, "other").unwrap();
        let sent = one.append(a.key(), &payload(json!(1))).unwrap();
        let marker = one.close(a.key()).unwrap();
        // Another server sees the marker, rejects publishes, and closing again
        // there returns the same marker.
        assert_eq!(two.close_marker(a.key()).unwrap(), Some(marker.clone()));
        assert!(matches!(
            two.append(a.key(), &payload(json!(2))),
            Err(ChannelError::StreamClosed)
        ));
        assert_eq!(two.close(a.key()).unwrap().id, marker.id);
        assert_eq!(
            two.read_after(a.key(), Some(cursor(&sent)), 10).unwrap(),
            vec![marker.clone()]
        );
        // Closing is per app and per stream.
        other_app.append(a.key(), &payload(json!(3))).unwrap();
        two.append(b.key(), &payload(json!(4))).unwrap();
        marker
    };
    let reopened = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    assert_eq!(reopened.close_marker(a.key()).unwrap(), Some(marker));
    assert!(matches!(
        reopened.append(a.key(), &payload(json!(5))),
        Err(ChannelError::StreamClosed)
    ));
    drop(reopened);
    drop_schema(&url, &schema);
}

#[test]
fn postgres_closed_stream_lasts_until_retention_empties_it() {
    let Some(url) = postgres_url() else {
        return;
    };
    let schema = format!("channel_close_retention_{}", std::process::id());
    let store = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let a = room("a");
    let lifecycle = auth(json!({
        "ok": true, "replayWindowMs": 60_000, "maxConnectionLifetimeMs": 10_000
    }));
    store.sync_channel(a.key(), &lifecycle).unwrap();
    store.append(a.key(), &payload(json!(1))).unwrap();
    store.close(a.key()).unwrap();
    let mut metadata = || -> i64 {
        client
            .query_one(
                &format!("SELECT COUNT(*) FROM {schema}.channel_metadata WHERE channel = $1"),
                &[&a.key()],
            )
            .unwrap()
            .get(0)
    };
    let mut age = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    // Idle past every connection lifetime, but the retained marker keeps it closed.
    age.batch_execute(&format!(
        "UPDATE {schema}.channel_metadata SET last_activity_unix_ms = 1"
    ))
    .unwrap();
    store.prune_at(now());
    assert_eq!(metadata(), 1);
    assert!(store.close_marker(a.key()).unwrap().is_some());
    // Once the replay window retires the marker, the stream is released and
    // a later publish starts a fresh open stream.
    age.batch_execute(&format!(
        "UPDATE {schema}.channel_messages SET created_at_unix_ms = 1"
    ))
    .unwrap();
    store.prune_at(now());
    assert_eq!(metadata(), 0);
    assert_eq!(store.close_marker(a.key()).unwrap(), None);
    let reopened = store.append(a.key(), &payload(json!(2))).unwrap();
    assert_eq!(store.read_after(a.key(), None, 10).unwrap(), vec![reopened]);
    drop(store);
    drop_schema(&url, &schema);
}

#[test]
fn sqlite_close_survives_restart_and_cursors_resume_into_the_marker() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("channels.sqlite");
    let a = room("a");
    let (sent, marker) = {
        let store = ChannelStore::open(&path).unwrap();
        let sent = store.append(a.key(), &payload(json!(1))).unwrap();
        (sent, store.close(a.key()).unwrap())
    };
    let store = ChannelStore::open(&path).unwrap();
    assert_eq!(store.close_marker(a.key()).unwrap(), Some(marker.clone()));
    assert_eq!(
        store.read_after(a.key(), Some(cursor(&sent)), 10).unwrap(),
        vec![marker.clone()]
    );
    assert!(matches!(
        store.append(a.key(), &payload(json!(2))),
        Err(ChannelError::StreamClosed)
    ));
    assert_eq!(store.close(a.key()).unwrap(), marker);
}

#[test]
fn sqlite_closed_stream_is_released_only_after_retention_empties_it() {
    let store = ChannelStore::open_in_memory_without_maintenance();
    let (a, b) = (room("a"), room("b"));
    let lifecycle = auth(json!({
        "ok": true, "replayWindowMs": 1_000, "maxConnectionLifetimeMs": 10_000
    }));
    for stream in [&a, &b] {
        store.sync_channel(stream.key(), &lifecycle).unwrap();
        store.append(stream.key(), &payload(json!(1))).unwrap();
        store.close(stream.key()).unwrap();
    }
    let rows = |table: &str| store.raw_query_i64(&format!("SELECT COUNT(*) FROM {table}"), []);
    // Idle longer than any connection, but `b` keeps its retained history.
    store.raw_execute(
        "UPDATE channel_messages SET created_at_unix_ms = ?1 WHERE channel = ?2",
        (now() + 20_000, b.key()),
    );
    store.raw_execute("UPDATE channel_metadata SET last_activity_unix_ms = 1", []);
    store.prune_at(now() + 2_000);
    assert_eq!(rows("channel_metadata"), 1);
    assert_eq!(store.close_marker(a.key()).unwrap(), None);
    assert!(store.close_marker(b.key()).unwrap().is_some());
    assert!(matches!(
        store.append(b.key(), &payload(json!(2))),
        Err(ChannelError::StreamClosed)
    ));
    // `a` was released with its marker: a later publish starts a fresh stream.
    let fresh = store.append(a.key(), &payload(json!(3))).unwrap();
    assert_eq!(store.read_after(a.key(), None, 10).unwrap(), vec![fresh]);
}

#[test]
fn expired_marker_hides_from_readers_but_keeps_the_stream_closed_until_pruned() {
    let store = ChannelStore::open_in_memory_without_maintenance();
    let a = room("a");
    store
        .sync_channel(a.key(), &auth(json!({"ok": true, "replayWindowMs": 1_000})))
        .unwrap();
    store.close(a.key()).unwrap();
    store.raw_execute("UPDATE channel_messages SET created_at_unix_ms = 1", []);
    assert_eq!(store.close_marker(a.key()).unwrap(), None);
    assert!(store.read_after(a.key(), None, 10).unwrap().is_empty());
    assert!(matches!(
        store.append(a.key(), &payload(json!(1))),
        Err(ChannelError::StreamClosed)
    ));
    store.prune_at(now());
    store.append(a.key(), &payload(json!(1))).unwrap();
}

#[test]
fn mesh_peers_ingest_the_marker_and_reject_later_publishes() {
    let temp = tempfile::tempdir().unwrap();
    let origin = ChannelStore::open(&temp.path().join("origin.sqlite")).unwrap();
    let peer = ChannelStore::open(&temp.path().join("peer.sqlite")).unwrap();
    let mut publications = origin.publications();
    let (a, b) = (room("a"), room("b"));
    let mut next_event = || {
        let ChannelPublication::Event(event) = publications.try_recv().unwrap() else {
            panic!("expected an event")
        };
        event
    };
    origin.append(a.key(), &payload(json!(1))).unwrap();
    assert_eq!(
        peer.ingest_remote(&next_event()).unwrap(),
        RemoteIngest::Applied
    );
    let mut changes = peer.changes(a.key());
    changes.borrow_and_update();
    let marker = origin.close(a.key()).unwrap();
    let event = next_event();
    assert_eq!(event.channel, a.key());
    assert_eq!(event.payload.r#type, CLOSE_MESSAGE_TYPE);
    assert_eq!(peer.ingest_remote(&event).unwrap(), RemoteIngest::Applied);
    assert!(changes.has_changed().unwrap());
    let replicated = peer.close_marker(a.key()).unwrap().unwrap();
    assert_eq!(replicated.event_id, marker.event_id);
    assert!(matches!(
        peer.append(a.key(), &payload(json!(2))),
        Err(ChannelError::StreamClosed)
    ));
    // An idempotent close commits nothing, so peers receive nothing more.
    origin.close(a.key()).unwrap();
    assert!(publications.try_recv().is_err());
    assert_eq!(peer.close_marker(b.key()).unwrap(), None);
    peer.append(b.key(), &payload(json!(3))).unwrap();
}

#[test]
fn only_trusted_durable_channel_streams_can_be_closed() {
    let store = ChannelStore::open_in_memory().unwrap();
    let a = room("a");
    // Neither an app message nor a client frame can forge the marker.
    let forged = json!({"type": CLOSE_MESSAGE_TYPE, "data": null});
    assert!(TrustedPublication::parse("chat", &json!({}), forged.clone()).is_err());
    assert!(websocket::parse_publish_payload(forged.to_string().as_bytes()).is_err());
    // Ephemeral streams retain nothing a marker could follow.
    assert!(matches!(
        store.publish_ephemeral(a.key(), &crate::stream::close_payload()),
        Err(ChannelError::BadRequest(_))
    ));
    assert!(
        TrustedPublication::close("chat", &json!({}), Some(json!({"ephemeral": true}))).is_err()
    );
    for channel in ["tako.store:messages", "tako.presence:room"] {
        assert!(TrustedPublication::close(channel, &json!({}), None).is_err());
    }
    assert!(TrustedPublication::close("chat", &json!({"from": "start"}), None).is_err());

    // A close publication binds its stream and lifecycle like a publish.
    let close = TrustedPublication::close(
        "chat",
        &json!({"room": "a"}),
        Some(json!({"replayWindowMs": 5_000})),
    )
    .unwrap();
    assert!(close.is_close());
    assert_eq!(close.stream, a);
    let marker = store.publish_trusted(&close).unwrap();
    assert_eq!(marker.channel, "chat");
    assert_eq!(marker.r#type, CLOSE_MESSAGE_TYPE);
    assert_eq!(
        store.raw_query_i64(
            "SELECT replay_window_ms FROM channel_metadata WHERE channel = ?1",
            [a.key()]
        ),
        5_000
    );
    assert_eq!(store.publish_trusted(&close).unwrap(), marker);
    let message = TrustedPublication::parse(
        "chat",
        &json!({"room": "a"}),
        json!({"type": "msg", "data": 1}),
    )
    .unwrap();
    assert!(matches!(
        store.publish_trusted(&message),
        Err(ChannelError::StreamClosed)
    ));

    // A loaded definition rejects closing an ephemeral channel.
    let definition: ChannelDefinitionMeta = serde_json::from_value(json!({
        "channel": "chat", "paramsSchema": {"type": "object", "properties": {"room": {"type": "string"}}},
        "auth": false, "ephemeral": true
    }))
    .unwrap();
    assert!(validate_publication(&definition, &close, &json!({"room": "a"})).is_err());
    assert!(validate_publication(&definition, &message, &json!({"room": "a"})).is_ok());
}

#[test]
fn from_is_a_reserved_query_key_and_a_cursor_wins_over_it() {
    assert!(!parse_replay_from(None).unwrap());
    assert!(!parse_replay_from(Some("room=a")).unwrap());
    assert!(parse_replay_from(Some("room=a&from=start")).unwrap());
    assert!(parse_replay_from(Some("fr%6Fm=st%61rt")).unwrap());
    for query in ["from=end", "from=", "from", "from=start&from=0"] {
        assert!(parse_replay_from(Some(query)).is_err(), "{query}");
    }
    assert_eq!(
        params::channel_params_query(Some("room=a&from=start&fr%6Fm=x&last_message_id=1")),
        "room=a"
    );
    let at = ReplayCursor::unscoped(3);
    assert_eq!(ReplayFrom::new(None, false), ReplayFrom::Head);
    assert_eq!(ReplayFrom::new(None, true), ReplayFrom::Start);
    assert_eq!(ReplayFrom::new(Some(at), true), ReplayFrom::After(at));
    assert_eq!(ReplayFrom::Start.or_cursor(Some(at)), ReplayFrom::After(at));
    assert_eq!(ReplayFrom::Start.or_cursor(None), ReplayFrom::Start);
}

async fn sse_body(
    store: &Arc<ChannelStore>,
    stream: &ChannelStream,
    from: ReplayFrom,
    lifetime_ms: u64,
) -> String {
    let (mut client, io) = tokio::io::duplex(1 << 16);
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: local\r\n\r\n")
        .await
        .unwrap();
    let mut session = pingora_proxy::Session::new_h1(Box::new(io));
    session.read_request().await.unwrap();
    let header = pingora_http::ResponseHeader::build(200, None).unwrap();
    session
        .write_response_header(Box::new(header), false)
        .await
        .unwrap();
    let lifecycle = auth(json!({"ok": true, "maxConnectionLifetimeMs": lifetime_ms}));
    let resets = store.resets(stream.key());
    let after = store.replay_from_async(stream.key(), from).await.unwrap();
    let (store, stream) = (store.clone(), stream.clone());
    let server = tokio::spawn(async move {
        sse::serve_events(&mut session, &store, &stream, after, &lifecycle, resets)
            .await
            .unwrap();
    });
    let mut response = String::new();
    client.read_to_string(&mut response).await.unwrap();
    server.await.unwrap();
    response
}

fn sse_data(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

fn close_event(marker: &ChannelMessage) -> String {
    format!("id: {}\nevent: tako.close\ndata: {{}}\n\n", marker.id)
}

#[tokio::test]
async fn sse_delivers_every_message_then_the_terminal_event_and_ends() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let store = Arc::new(ChannelStore::open_in_memory().unwrap());
        let (a, b) = (room("a"), room("b"));
        let early = store.append(a.key(), &payload(json!("early"))).unwrap();
        // A live subscriber at the head gets the rest, then the terminal event,
        // and the response ends long before the connection lifetime.
        let writer = {
            let (store, a) = (store.clone(), a.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(40)).await;
                let last = store.append(a.key(), &payload(json!("last"))).unwrap();
                (last, store.close(a.key()).unwrap())
            })
        };
        // Resume after `early`, the head when the writer starts: the reader
        // gets the rest however late it resolves where to start.
        let live = sse_body(&store, &a, ReplayFrom::After(cursor(&early)), 60_000).await;
        let (last, marker) = writer.await.unwrap();
        assert!(live.contains(&close_event(&marker)), "{live}");
        assert_eq!(
            sse_data(&live),
            vec![
                serde_json::to_value(a.public(last.clone())).unwrap(),
                json!({}),
            ]
        );
        assert!(!live.contains("tako.stream"), "{live}");
        assert!(!live.contains(&a.key()[STREAM_CHANNEL_PREFIX.len()..]));

        // A later reader from the start replays everything, then the event.
        let replay = sse_body(&store, &a, ReplayFrom::Start, 60_000).await;
        assert_eq!(
            sse_data(&replay),
            vec![
                serde_json::to_value(a.public(early.clone())).unwrap(),
                serde_json::to_value(a.public(last.clone())).unwrap(),
                json!({}),
            ]
        );
        assert!(replay.contains(&close_event(&marker)), "{replay}");
        // A cursor before the marker resumes into the rest; one at the marker,
        // or a reader at the head of a closed stream, gets the event at once.
        let resumed = sse_body(&store, &a, ReplayFrom::After(cursor(&early)), 60_000).await;
        assert_eq!(sse_data(&resumed).len(), 2, "{resumed}");
        for from in [ReplayFrom::After(cursor(&marker)), ReplayFrom::Head] {
            let body = sse_body(&store, &a, from, 60_000).await;
            assert!(body.contains(&close_event(&marker)), "{body}");
            assert_eq!(sse_data(&body), vec![json!({})], "{body}");
        }
        // Another binding of the channel is still open.
        let open = sse_body(&store, &b, ReplayFrom::Start, 100).await;
        assert!(!open.contains("tako.close"), "{open}");
    })
    .await
    .unwrap();
}

async fn websocket(
    store: &Arc<ChannelStore>,
    writer: Arc<ChannelStore>,
    stream: &ChannelStream,
    from: ReplayFrom,
) -> (
    tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<()>,
) {
    let (client, io) = tokio::io::duplex(1 << 16);
    let (store, stream) = (store.clone(), stream.clone());
    let server = tokio::spawn(async move {
        let mut session = pingora_proxy::Session::new_h1(Box::new(io));
        session.read_request().await.unwrap();
        let response = websocket::build_websocket_upgrade_response(session.req_header()).unwrap();
        session
            .write_response_header(Box::new(response), false)
            .await
            .unwrap();
        websocket::serve_with_writer(
            &mut session,
            &store,
            &stream,
            from,
            auth(json!({"ok": true, "transport": "ws"})),
            json!({}),
            Default::default(),
            Arc::new(LocalChannelWriter::new(writer)),
        )
        .await
        .unwrap();
    });
    let (socket, _) = tokio_tungstenite::client_async("ws://local/", client)
        .await
        .unwrap();
    (socket, server)
}

fn assert_stream_closed(frame: tokio_tungstenite::tungstenite::Message) {
    use tokio_tungstenite::tungstenite::{Message, protocol::frame::coding::CloseCode};
    let Message::Close(Some(close)) = frame else {
        panic!("expected a close frame, got {frame:?}");
    };
    assert_eq!(close.code, CloseCode::from(4200));
    assert_eq!(close.reason.as_str(), "stream-closed");
    assert_eq!(ChannelCloseCode::StreamClosed.ws_close_code(), 4200);
}

#[tokio::test]
async fn websocket_delivers_every_message_then_the_stream_closed_frame() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    tokio::time::timeout(Duration::from_secs(10), async {
        let store = Arc::new(ChannelStore::open_in_memory().unwrap());
        let a = room("a");
        let early = store.append(a.key(), &payload(json!("early"))).unwrap();
        // Resume after `early` (the head): no wait for the server to resolve it.
        let (mut live, server) =
            websocket(&store, store.clone(), &a, ReplayFrom::After(cursor(&early))).await;
        let last = store.append(a.key(), &payload(json!("last"))).unwrap();
        store.close(a.key()).unwrap();
        let frame = live.next().await.unwrap().unwrap();
        let message: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert_eq!(message, serde_json::to_value(a.public(last)).unwrap());
        assert_stream_closed(live.next().await.unwrap().unwrap());
        server.await.unwrap();

        // Later readers: from the start replays, at the head closes at once.
        let (mut replay, server) = websocket(&store, store.clone(), &a, ReplayFrom::Start).await;
        let mut data = Vec::new();
        loop {
            match replay.next().await.unwrap().unwrap() {
                Message::Text(text) => {
                    data.push(serde_json::from_str::<Value>(&text).unwrap()["data"].clone())
                }
                frame => {
                    assert_stream_closed(frame);
                    break;
                }
            }
        }
        assert_eq!(data, vec![json!("early"), json!("last")]);
        server.await.unwrap();
        for from in [ReplayFrom::Head, ReplayFrom::After(cursor(&early))] {
            let (mut socket, server) = websocket(&store, store.clone(), &a, from).await;
            let mut frame = socket.next().await.unwrap().unwrap();
            if from != ReplayFrom::Head {
                assert!(frame.is_text());
                frame = socket.next().await.unwrap().unwrap();
            }
            assert_stream_closed(frame);
            server.await.unwrap();
        }

        // A client publish that races the close is answered with the same frame.
        let b = room("b");
        let closed = Arc::new(ChannelStore::open_in_memory().unwrap());
        closed.close(b.key()).unwrap();
        let (mut socket, server) = websocket(&store, closed, &b, ReplayFrom::Head).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        socket
            .send(Message::text(json!({"type": "msg", "data": 1}).to_string()))
            .await
            .unwrap();
        assert_stream_closed(socket.next().await.unwrap().unwrap());
        server.await.unwrap();
        assert!(store.read_after(b.key(), None, 10).unwrap().is_empty());
    })
    .await
    .unwrap();
}

/// A publish rejected because its stream is closed must not count as activity:
/// it would keep an inactivity-TTL stream, and its marker, alive forever.
fn rejected_publishes_leave_a_closed_stream_idle(
    store: &ChannelStore,
    last_activity: &mut dyn FnMut(Option<i64>) -> i64,
) {
    let lifecycle = Some(json!({"inactivityTtlMs": 60_000}));
    let close = TrustedPublication::close("chat", &json!({"room": "idle"}), lifecycle).unwrap();
    store.publish_trusted(&close).unwrap();
    let idle = last_activity(Some(1));
    assert_eq!(idle, 1);
    let publish = TrustedPublication::parse(
        "chat",
        &json!({"room": "idle"}),
        json!({"type": "msg", "data": 1, "lifecycle": {"inactivityTtlMs": 60_000}}),
    )
    .unwrap();
    assert!(matches!(
        store.publish_trusted(&publish),
        Err(ChannelError::StreamClosed)
    ));
    assert_eq!(
        last_activity(None),
        idle,
        "rejected publish touched the stream"
    );
    // Inactivity retires the marker, and the stream accepts publishes again.
    store.prune_at(now());
    store.publish_trusted(&publish).unwrap();
}

#[test]
fn sqlite_rejected_publishes_leave_a_closed_stream_idle() {
    let store = ChannelStore::open_in_memory_without_maintenance();
    let key = room("idle").key().to_owned();
    let conn = |set: Option<i64>| {
        if let Some(value) = set {
            store.raw_execute(
                "UPDATE channel_metadata SET last_activity_unix_ms = ?1 WHERE channel = ?2",
                rusqlite::params![value, key],
            );
        }
        store.raw_query_i64(
            "SELECT last_activity_unix_ms FROM channel_metadata WHERE channel = ?1",
            [&key],
        )
    };
    let mut conn = conn;
    rejected_publishes_leave_a_closed_stream_idle(&store, &mut conn);
}

#[test]
fn postgres_rejected_publishes_leave_a_closed_stream_idle() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!("channel_rejected_{}", std::process::id());
    let store = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let key = room("idle").key().to_owned();
    let mut activity = |set: Option<i64>| -> i64 {
        if let Some(value) = set {
            client
                .execute(
                    &format!(
                        "UPDATE {schema}.channel_metadata SET last_activity_unix_ms = $1
                         WHERE app_id = 'app' AND channel = $2"
                    ),
                    &[&value, &key],
                )
                .unwrap();
        }
        client
            .query_one(
                &format!(
                    "SELECT last_activity_unix_ms FROM {schema}.channel_metadata
                     WHERE app_id = 'app' AND channel = $1"
                ),
                &[&key],
            )
            .unwrap()
            .get(0)
    };
    rejected_publishes_leave_a_closed_stream_idle(&store, &mut activity);
    drop(store);
    postgres::Client::connect(&url, postgres::NoTls)
        .unwrap()
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}

#[test]
fn mesh_close_wins_over_a_publish_that_races_it_on_every_node() {
    let temp = tempfile::tempdir().unwrap();
    let a = ChannelStore::open(&temp.path().join("a.sqlite")).unwrap();
    let b = ChannelStore::open(&temp.path().join("b.sqlite")).unwrap();
    let (mut from_a, mut from_b) = (a.publications(), b.publications());
    let next = |publications: &mut tokio::sync::broadcast::Receiver<ChannelPublication>| {
        let ChannelPublication::Event(event) = publications.try_recv().unwrap() else {
            panic!("expected an event")
        };
        event
    };
    let stream = room("race");
    a.append(stream.key(), &payload(json!(1))).unwrap();
    assert_eq!(
        b.ingest_remote(&next(&mut from_a)).unwrap(),
        RemoteIngest::Applied
    );

    // A closes while B publishes, before either event reaches the other.
    a.close(stream.key()).unwrap();
    b.append(stream.key(), &payload(json!(2))).unwrap();
    let (close, raced) = (next(&mut from_a), next(&mut from_b));
    let resets = a.resets(stream.key());
    assert_eq!(b.ingest_remote(&close).unwrap(), RemoteIngest::Applied);
    // On A the racing publish would land after the marker: it is dropped.
    assert_eq!(a.ingest_remote(&raced).unwrap(), RemoteIngest::Dropped);
    let types = |store: &ChannelStore| -> Vec<String> {
        store
            .read_after(stream.key(), None, 10)
            .unwrap()
            .into_iter()
            .map(|message| message.r#type)
            .collect()
    };
    // Every node ends with the marker, and nothing follows it.
    assert_eq!(types(&a), ["msg", CLOSE_MESSAGE_TYPE]);
    assert_eq!(types(&b), ["msg", "msg", CLOSE_MESSAGE_TYPE]);

    // Closes racing each other: each node keeps its first marker.
    let other = room("both");
    a.close(other.key()).unwrap();
    b.close(other.key()).unwrap();
    let (from_a_close, from_b_close) = (next(&mut from_a), next(&mut from_b));
    assert_eq!(
        a.ingest_remote(&from_b_close).unwrap(),
        RemoteIngest::Dropped
    );
    assert_eq!(
        b.ingest_remote(&from_a_close).unwrap(),
        RemoteIngest::Dropped
    );
    assert!(a.close_marker(other.key()).unwrap().is_some());
    assert!(b.close_marker(other.key()).unwrap().is_some());

    // A dropped event is acknowledged in sequence: B's next event is no gap.
    b.append(room("later").key(), &payload(json!(3))).unwrap();
    assert_eq!(
        a.ingest_remote(&next(&mut from_b)).unwrap(),
        RemoteIngest::Applied
    );
    assert!(!resets.has_changed().unwrap(), "no reset storm");
}

#[test]
fn mesh_peers_keep_a_stream_as_long_as_its_origin_does() {
    let temp = tempfile::tempdir().unwrap();
    let origin = ChannelStore::open(&temp.path().join("origin.sqlite")).unwrap();
    let peer = ChannelStore::open(&temp.path().join("peer.sqlite")).unwrap();
    let mut publications = origin.publications();
    let mut next = || {
        let ChannelPublication::Event(event) = publications.try_recv().unwrap() else {
            panic!("expected an event")
        };
        event
    };
    let day = 24 * 60 * 60 * 1000;
    let lifecycle = json!({"replayWindowMs": day});
    let publish = TrustedPublication::parse(
        "chat",
        &json!({"room": "long"}),
        json!({"type": "msg", "data": 1, "lifecycle": lifecycle}),
    )
    .unwrap();
    origin.publish_trusted(&publish).unwrap();
    let close =
        TrustedPublication::close("chat", &json!({"room": "long"}), Some(lifecycle)).unwrap();
    origin.publish_trusted(&close).unwrap();
    let (message, marker) = (next(), next());
    assert_eq!(message.lifecycle.as_ref().unwrap().replay_window_ms, day);
    // The peer has no local subscriber and has never synced the stream.
    for event in [&message, &marker] {
        assert_eq!(peer.ingest_remote(event).unwrap(), RemoteIngest::Applied);
    }
    let key = publish.stream.key();
    assert_eq!(
        peer.raw_query_i64(
            "SELECT replay_window_ms FROM channel_metadata WHERE channel = ?1",
            [key]
        ),
        day as i64
    );
    // Past the default ten-minute window the peer still holds both rows and
    // the stream is still closed there.
    peer.prune_at(now() + 11 * 60 * 1000);
    assert_eq!(peer.read_after(key, None, 10).unwrap().len(), 2);
    assert!(matches!(
        peer.append(key, &payload(json!(2))),
        Err(ChannelError::StreamClosed)
    ));

    // A subscriber's grant on the origin travels the same way.
    let synced = room("synced");
    origin
        .sync_channel(
            synced.key(),
            &auth(json!({"ok": true, "replayWindowMs": day})),
        )
        .unwrap();
    origin.append(synced.key(), &payload(json!(3))).unwrap();
    assert_eq!(next().lifecycle.unwrap().replay_window_ms, day);
}

#[test]
fn mesh_peers_reopen_a_stream_once_its_marker_expired_where_the_publish_was_made() {
    let temp = tempfile::tempdir().unwrap();
    let a = ChannelStore::open(&temp.path().join("a.sqlite")).unwrap();
    let b = ChannelStore::open(&temp.path().join("b.sqlite")).unwrap();
    let mut from_a = a.publications();
    let mut next = || {
        let ChannelPublication::Event(event) = from_a.try_recv().unwrap() else {
            panic!("expected an event")
        };
        event
    };
    let window = 60_000;
    let lifecycle = json!({"replayWindowMs": window});
    let publish = |data: i64| {
        TrustedPublication::parse(
            "chat",
            &json!({"room": "again"}),
            json!({"type": "msg", "data": data, "lifecycle": lifecycle}),
        )
        .unwrap()
    };
    let key = publish(1).stream.key().to_owned();
    a.publish_trusted(&publish(1)).unwrap();
    let close =
        TrustedPublication::close("chat", &json!({"room": "again"}), Some(lifecycle.clone()))
            .unwrap();
    a.publish_trusted(&close).unwrap();
    let (message, marker) = (next(), next());
    for event in [&message, &marker] {
        assert_eq!(b.ingest_remote(event).unwrap(), RemoteIngest::Applied);
    }
    // Peers keep the origin's creation time, so every copy of the marker
    // expires at the same logical moment however their pruning is scheduled.
    let created = |store: &ChannelStore, kind: &str| {
        store.raw_query_i64(
            "SELECT created_at_unix_ms FROM channel_messages WHERE channel = ?1 AND type = ?2",
            [key.as_str(), kind],
        )
    };
    let closed_at = marker.created_at_unix_ms.unwrap();
    assert_eq!(created(&a, CLOSE_MESSAGE_TYPE), closed_at);
    assert_eq!(created(&b, CLOSE_MESSAGE_TYPE), closed_at);

    // A publish from a third server made while the marker was live raced
    // the close and is still dropped.
    let third = |sequence: u64, created_at_unix_ms: i64| ChannelEvent {
        id: ChannelEventId {
            origin: "third".into(),
            sequence,
        },
        channel: key.clone(),
        payload: payload(json!("third")),
        ephemeral: false,
        lifecycle: None,
        created_at_unix_ms: Some(created_at_unix_ms),
    };
    assert_eq!(
        b.ingest_remote(&third(1, closed_at + 1_000)).unwrap(),
        RemoteIngest::Dropped
    );
    assert!(b.close_marker(&key).unwrap().is_some());

    // The close happened a window ago: the origin's maintenance retires the
    // marker and the stream reopens there, while B has not pruned yet.
    for store in [&a, &b] {
        store.raw_execute(
            "UPDATE channel_messages SET created_at_unix_ms = created_at_unix_ms - ?1
             WHERE channel = ?2",
            rusqlite::params![window + 1, key],
        );
    }
    a.prune_at(now());
    assert!(a.read_after(&key, None, 10).unwrap().is_empty());
    a.publish_trusted(&publish(2)).unwrap();
    assert_eq!(b.ingest_remote(&next()).unwrap(), RemoteIngest::Applied);

    // B applies it as the origin did: the closed history is gone and readers
    // there see a fresh, open stream, the same one A has.
    let contents = |store: &ChannelStore| -> Vec<(String, Value)> {
        store
            .read_after(&key, None, 10)
            .unwrap()
            .into_iter()
            .map(|message| (message.r#type, message.data))
            .collect()
    };
    assert_eq!(contents(&a), [("msg".to_owned(), json!(2))]);
    assert_eq!(contents(&b), contents(&a));
    assert!(b.close_marker(&key).unwrap().is_none());
    b.append(&key, &payload(json!(3))).unwrap();
}

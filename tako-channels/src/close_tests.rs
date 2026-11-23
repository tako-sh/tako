//! A stream can be closed by a durable terminal marker and read from its start.
use crate::test_db::TestDb;
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

#[test]
fn close_is_durable_final_and_per_stream() {
    let db = TestDb::start();
    let store = db.writer();
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
fn close_is_shared_by_every_server_and_survives_restart() {
    let db = TestDb::start();
    let (a, b) = (room("a"), room("b"));
    let (sent, marker) = {
        let one = db.writer();
        let two = db.writer();
        let other_app = db.writer_for("other");
        let sent = one.append(a.key(), &payload(json!(1))).unwrap();
        let marker = one.close(a.key()).unwrap();
        // Another server sees the marker, rejects publishes, and closing again
        // there returns the same marker.
        assert_eq!(two.close_marker(a.key()).unwrap(), Some(marker.clone()));
        assert!(matches!(
            two.append(a.key(), &payload(json!(2))),
            Err(ChannelError::StreamClosed)
        ));
        assert_eq!(two.close(a.key()).unwrap(), marker);
        assert_eq!(
            two.read_after(a.key(), Some(cursor(&sent)), 10).unwrap(),
            vec![marker.clone()]
        );
        // Closing is per app and per stream.
        other_app.append(a.key(), &payload(json!(3))).unwrap();
        two.append(b.key(), &payload(json!(4))).unwrap();
        (sent, marker)
    };
    // After a restart, cursors still resume into the marker.
    let reopened = db.writer();
    assert_eq!(
        reopened.close_marker(a.key()).unwrap(),
        Some(marker.clone())
    );
    assert_eq!(
        reopened
            .read_after(a.key(), Some(cursor(&sent)), 10)
            .unwrap(),
        vec![marker.clone()]
    );
    assert!(matches!(
        reopened.append(a.key(), &payload(json!(5))),
        Err(ChannelError::StreamClosed)
    ));
    assert_eq!(reopened.close(a.key()).unwrap(), marker);
}

#[test]
fn closed_stream_is_released_only_after_retention_empties_it() {
    let db = TestDb::start();
    let store = db.without_maintenance();
    let (a, b) = (room("a"), room("b"));
    let lifecycle = auth(json!({
        "ok": true, "replayWindowMs": 60_000, "maxConnectionLifetimeMs": 10_000
    }));
    for stream in [&a, &b] {
        store.sync_channel(stream.key(), &lifecycle).unwrap();
        store.append(stream.key(), &payload(json!(1))).unwrap();
        store.close(stream.key()).unwrap();
    }
    let streams = || db.query_i64("SELECT COUNT(*) FROM tako_replay_streams");
    // Idle past every connection lifetime, but retained markers keep both closed.
    db.execute("UPDATE tako_replay_streams SET activity_ms = 1");
    store.prune();
    assert_eq!(streams(), 2);
    assert!(store.close_marker(a.key()).unwrap().is_some());
    // Once the replay window retires `a`'s history, it is released with its
    // marker; `b` keeps its retained history and stays closed.
    db.execute(&format!(
        "UPDATE tako_replay_messages SET created_ms = 1 WHERE channel = '{}'",
        a.key()
    ));
    store.prune();
    assert_eq!(streams(), 1);
    assert_eq!(store.close_marker(a.key()).unwrap(), None);
    assert!(store.close_marker(b.key()).unwrap().is_some());
    assert!(matches!(
        store.append(b.key(), &payload(json!(2))),
        Err(ChannelError::StreamClosed)
    ));
    // A later publish starts a fresh open stream.
    let fresh = store.append(a.key(), &payload(json!(3))).unwrap();
    assert_eq!(store.read_after(a.key(), None, 10).unwrap(), vec![fresh]);
}

#[test]
fn expired_marker_hides_from_readers_but_keeps_the_stream_closed_until_pruned() {
    let db = TestDb::start();
    let store = db.without_maintenance();
    let a = room("a");
    store
        .sync_channel(a.key(), &auth(json!({"ok": true, "replayWindowMs": 1_000})))
        .unwrap();
    store.close(a.key()).unwrap();
    db.execute("UPDATE tako_replay_messages SET created_ms = 1");
    assert_eq!(store.close_marker(a.key()).unwrap(), None);
    assert!(store.read_after(a.key(), None, 10).unwrap().is_empty());
    assert!(matches!(
        store.append(a.key(), &payload(json!(1))),
        Err(ChannelError::StreamClosed)
    ));
    store.prune();
    store.append(a.key(), &payload(json!(1))).unwrap();
}

#[test]
fn only_trusted_durable_channel_streams_can_be_closed() {
    let db = TestDb::start();
    let store = db.writer();
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
        db.query_i64(&format!(
            "SELECT window_ms FROM tako_replay_streams WHERE channel = '{}'",
            a.key()
        )),
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
        let db = TestDb::start();
        let store = db.open().await;
        let (a, b) = (room("a"), room("b"));
        let early = store
            .append_async(a.key(), &payload(json!("early")))
            .await
            .unwrap();
        // A live subscriber at the head gets the rest, then the terminal event,
        // and the response ends long before the connection lifetime.
        let writer = {
            let (store, a) = (store.clone(), a.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(40)).await;
                store
                    .run(move |store| {
                        let last = store.append(a.key(), &payload(json!("last")))?;
                        Ok((last, store.close(a.key())?))
                    })
                    .await
                    .unwrap()
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
        let db = TestDb::start();
        let store = db.open().await;
        let a = room("a");
        let early = store
            .append_async(a.key(), &payload(json!("early")))
            .await
            .unwrap();
        // Resume after `early` (the head): no wait for the server to resolve it.
        let (mut live, server) =
            websocket(&store, store.clone(), &a, ReplayFrom::After(cursor(&early))).await;
        let (last, _) = {
            let a = a.clone();
            store
                .run(move |store| {
                    let last = store.append(a.key(), &payload(json!("last")))?;
                    Ok((last, store.close(a.key())?))
                })
                .await
                .unwrap()
        };
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
        let closed = db.open_for("closed").await;
        {
            let b = b.clone();
            closed.run(move |store| store.close(b.key())).await.unwrap();
        }
        let (mut socket, server) = websocket(&store, closed, &b, ReplayFrom::Head).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        socket
            .send(Message::text(json!({"type": "msg", "data": 1}).to_string()))
            .await
            .unwrap();
        assert_stream_closed(socket.next().await.unwrap().unwrap());
        server.await.unwrap();
        assert!(
            store
                .read_after_async(b.key(), None, 10)
                .await
                .unwrap()
                .is_empty()
        );
    })
    .await
    .unwrap();
}

/// A publish rejected because its stream is closed must not count as activity:
/// it would keep an inactivity-TTL stream, and its marker, alive forever.
#[test]
fn rejected_publishes_leave_a_closed_stream_idle() {
    let db = TestDb::start();
    let store = db.without_maintenance();
    let key = room("idle").key().to_owned();
    let last_activity = |set: Option<i64>| {
        if let Some(value) = set {
            db.execute(&format!(
                "UPDATE tako_replay_streams SET activity_ms = {value} WHERE channel = '{key}'"
            ));
        }
        db.query_i64(&format!(
            "SELECT activity_ms FROM tako_replay_streams WHERE channel = '{key}'"
        ))
    };
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
    store.prune();
    store.publish_trusted(&publish).unwrap();
}

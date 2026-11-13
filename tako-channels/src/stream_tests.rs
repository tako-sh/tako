//! Params select a stream: storage, replay, cursors, fanout and replication.
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
fn bindings_have_separate_replay_and_cursors(store: &ChannelStore) {
    let (a, b) = (room("a"), room("b"));
    let lifecycle = auth(json!({"ok": true}));
    for stream in [&a, &b] {
        store.sync_channel(stream.key(), &lifecycle).unwrap();
    }
    let (start_a, start_b) = (
        store.replay_cursor(a.key(), None).unwrap(),
        store.replay_cursor(b.key(), None).unwrap(),
    );
    let first = store.append(a.key(), &payload(json!("a1"))).unwrap();
    let other = store.append(b.key(), &payload(json!("b1"))).unwrap();
    let second = store.append(a.key(), &payload(json!("a2"))).unwrap();

    // Neither replay from the start nor replay after a cursor crosses bindings.
    assert_eq!(
        store.read_after(a.key(), None, 10).unwrap(),
        vec![first.clone(), second.clone()]
    );
    assert_eq!(
        store.read_after(a.key(), start_a, 10).unwrap(),
        vec![first.clone(), second.clone()]
    );
    assert_eq!(
        store.read_after(b.key(), start_b, 10).unwrap(),
        vec![other.clone()]
    );
    assert_eq!(
        store.replay_cursor(a.key(), None).unwrap(),
        Some(cursor(&second))
    );
    assert_eq!(
        store.replay_cursor(b.key(), None).unwrap(),
        Some(cursor(&other))
    );
    // The bare channel is a third stream: nothing was published without params.
    assert!(store.read_after("chat", None, 10).unwrap().is_empty());

    // A cursor names one binding. Another binding resets it instead of
    // resuming at a position it never issued.
    for (stream, foreign) in [(&b, &first), (&a, &other)] {
        assert!(matches!(
            store.read_after(stream.key(), Some(cursor(foreign)), 10),
            Err(ChannelError::StaleCursor)
        ));
        assert!(matches!(
            store.replay_cursor(stream.key(), Some(cursor(foreign))),
            Err(ChannelError::StaleCursor)
        ));
    }
    assert!(matches!(
        store.replay_cursor("chat", Some(cursor(&first))),
        Err(ChannelError::StaleCursor)
    ));
    assert_eq!(
        store.read_after(a.key(), Some(cursor(&first)), 10).unwrap(),
        vec![second]
    );
}

#[test]
fn sqlite_bindings_have_separate_replay_and_cursors() {
    bindings_have_separate_replay_and_cursors(&ChannelStore::open_in_memory().unwrap());
}

#[test]
fn postgres_bindings_have_separate_replay_and_cursors() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!("channel_streams_{}", std::process::id());
    let store = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    bindings_have_separate_replay_and_cursors(&store);
    // Shared history keeps one position space, so the cursor itself names its stream.
    let message = store.append(room("a").key(), &payload(json!(1))).unwrap();
    assert!(message.id.starts_with("p1."), "{}", message.id);
    assert!(matches!(
        store.read_after(
            room("a").key(),
            Some(ReplayCursor::unscoped(cursor(&message).position)),
            10
        ),
        Err(ChannelError::StaleCursor)
    ));
    drop(store);
    postgres::Client::connect(&url, postgres::NoTls)
        .unwrap()
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}

#[test]
fn sqlite_releases_idle_streams_and_resets_their_old_cursors() {
    let store = ChannelStore::open_in_memory_without_maintenance();
    let streams: Vec<ChannelStream> = (0..50).map(|n| room(&n.to_string())).collect();
    let lifecycle = auth(json!({
        "ok": true, "replayWindowMs": 1_000, "maxConnectionLifetimeMs": 10_000
    }));
    let mut cursors = Vec::new();
    for stream in &streams {
        store.sync_channel(stream.key(), &lifecycle).unwrap();
        cursors.push(cursor(
            &store.append(stream.key(), &payload(json!(1))).unwrap(),
        ));
    }
    let rows = |table: &str| store.raw_query_i64(&format!("SELECT COUNT(*) FROM {table}"), []);
    assert_eq!(rows("channel_metadata"), 50);

    // Expired replay alone keeps the stream: a subscriber may still be connected,
    // and a reader that saw everything resumes without a reset.
    let expired = now() + 2_000;
    store.prune_at(expired);
    assert_eq!(rows("channel_messages"), 0);
    assert_eq!(rows("channel_metadata"), 50);
    assert_eq!(
        store
            .replay_cursor(streams[0].key(), Some(cursors[0]))
            .unwrap(),
        Some(cursors[0])
    );

    // A subscriber that connects keeps its stream past the others' idle time.
    store.raw_execute(
        "UPDATE channel_metadata SET last_activity_unix_ms = ?1 WHERE channel = ?2",
        (expired + 60_000, streams[1].key()),
    );
    let idle = now() + 10_000 + 60_000 + 2_000;
    store.prune_at(idle);
    assert_eq!(rows("channel_metadata"), 1);
    assert_eq!(rows("channel_replay_floor"), 1);
    assert_eq!(
        store
            .replay_cursor(streams[1].key(), Some(cursors[1]))
            .unwrap(),
        Some(cursors[1])
    );

    // A released stream is reborn caught up with the store. The cursor from its
    // earlier life is stale even though nothing below it is retained anywhere.
    assert!(matches!(
        store.replay_cursor(streams[0].key(), Some(cursors[0])),
        Err(ChannelError::StaleCursor)
    ));
    store.sync_channel(streams[0].key(), &lifecycle).unwrap();
    let head = store.replay_cursor(streams[0].key(), None).unwrap();
    assert_eq!(head.unwrap().position, 50);
    assert!(matches!(
        store.replay_cursor(streams[0].key(), Some(cursors[0])),
        Err(ChannelError::StaleCursor)
    ));
    let reborn = store.append(streams[0].key(), &payload(json!(2))).unwrap();
    assert_eq!(
        store.read_after(streams[0].key(), head, 10).unwrap(),
        vec![reborn.clone()]
    );
    assert!(matches!(
        store.read_after(streams[0].key(), Some(cursors[0]), 10),
        Err(ChannelError::StaleCursor)
    ));

    // A stream first seen through a publish is born the same way.
    let fresh = store
        .append(room("fresh").key(), &payload(json!(3)))
        .unwrap();
    assert!(matches!(
        store.read_after(
            room("fresh").key(),
            Some(ReplayCursor {
                position: cursor(&reborn).position - 1,
                ..cursor(&fresh)
            }),
            10
        ),
        Err(ChannelError::StaleCursor)
    ));
    assert_eq!(
        store
            .read_after(
                room("fresh").key(),
                Some(ReplayCursor {
                    position: cursor(&reborn).position,
                    ..cursor(&fresh)
                }),
                10
            )
            .unwrap(),
        vec![fresh]
    );
}

#[test]
fn connection_lifetime_of_a_stream_only_grows_until_it_is_released() {
    let store = ChannelStore::open_in_memory_without_maintenance();
    let stream = room("a");
    let lifetime = |ms: u64| auth(json!({"ok": true, "maxConnectionLifetimeMs": ms}));
    store.sync_channel(stream.key(), &lifetime(50_000)).unwrap();
    // A later grant with a shorter lifetime must not expose the longer connection.
    store.sync_channel(stream.key(), &lifetime(1_000)).unwrap();
    assert_eq!(
        store.raw_query_i64(
            "SELECT max_connection_lifetime_ms FROM channel_metadata WHERE channel = ?1",
            [stream.key()]
        ),
        50_000
    );
    store.prune_at(now() + 1_000 + 60_000 + 2_000);
    assert_eq!(
        store.raw_query_i64("SELECT COUNT(*) FROM channel_metadata", []),
        1
    );
    store.prune_at(now() + 50_000 + 60_000 + 2_000);
    assert_eq!(
        store.raw_query_i64("SELECT COUNT(*) FROM channel_metadata", []),
        0
    );
}

#[test]
fn postgres_releases_idle_streams_and_resets_their_old_cursors() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!("channel_idle_{}", std::process::id());
    let store = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    let other_app = ChannelStore::open_postgres_with_schema(&url, &schema, "other").unwrap();
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let (idle, live, retained) = (room("idle"), room("live"), room("retained"));
    let lifecycle = auth(json!({"ok": true, "maxConnectionLifetimeMs": 10_000}));
    for stream in [&idle, &live, &retained] {
        store.sync_channel(stream.key(), &lifecycle).unwrap();
    }
    other_app.sync_channel(idle.key(), &lifecycle).unwrap();
    let seen = store.append(idle.key(), &payload(json!(1))).unwrap();
    let kept = store.append(retained.key(), &payload(json!(2))).unwrap();
    let mut rows = |app: &str| -> i64 {
        client
            .query_one(
                &format!("SELECT COUNT(*) FROM {schema}.channel_metadata WHERE app_id = $1"),
                &[&app],
            )
            .unwrap()
            .get(0)
    };
    let mut age = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    // Expire the idle stream's replay and move every stream but `live` past
    // its connection lifetime.
    age.batch_execute(&format!(
        "DELETE FROM {schema}.channel_messages WHERE app_id = 'app' AND channel = '{}';
         UPDATE {schema}.channel_metadata SET last_activity_unix_ms = 1
         WHERE app_id = 'app' AND channel <> '{}'",
        idle.key(),
        live.key()
    ))
    .unwrap();
    store.prune_at(now());
    // Retained messages and recent subscribers keep their streams; other apps
    // are untouched.
    assert_eq!(rows("app"), 2);
    assert_eq!(rows("other"), 1);
    assert_eq!(
        store.read_after(retained.key(), None, 10).unwrap(),
        vec![kept]
    );
    assert!(matches!(
        store.replay_cursor(idle.key(), Some(cursor(&seen))),
        Err(ChannelError::StaleCursor)
    ));
    // Reborn caught up with the shared history: the old cursor stays stale.
    store.sync_channel(idle.key(), &lifecycle).unwrap();
    let head = store.replay_cursor(idle.key(), None).unwrap().unwrap();
    assert!(head.position > cursor(&seen).position);
    assert!(matches!(
        store.read_after(idle.key(), Some(cursor(&seen)), 10),
        Err(ChannelError::StaleCursor)
    ));
    let reborn = store.append(idle.key(), &payload(json!(3))).unwrap();
    assert_eq!(
        store.read_after(idle.key(), Some(head), 10).unwrap(),
        vec![reborn]
    );
    drop((store, other_app));
    age.batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}

#[tokio::test]
async fn replication_and_ephemeral_fanout_stay_within_one_binding() {
    let temp = tempfile::tempdir().unwrap();
    let origin = ChannelStore::open(&temp.path().join("origin.sqlite")).unwrap();
    let peer = ChannelStore::open(&temp.path().join("peer.sqlite")).unwrap();
    let mut publications = origin.publications();
    let (a, b) = (room("a"), room("b"));

    let mut changes_a = peer.changes(a.key());
    let mut changes_b = peer.changes(b.key());
    changes_a.borrow_and_update();
    changes_b.borrow_and_update();
    let durable = origin.append(a.key(), &payload(json!("a1"))).unwrap();
    let ChannelPublication::Event(event) = publications.try_recv().unwrap() else {
        panic!("expected an event")
    };
    // Peers replicate the opaque key, as they do for Stores.
    assert_eq!(event.channel, a.key());
    assert_eq!(peer.ingest_remote(&event).unwrap(), RemoteIngest::Applied);
    assert!(changes_a.has_changed().unwrap());
    assert!(!changes_b.has_changed().unwrap());
    let replicated = peer.read_after(a.key(), None, 10).unwrap();
    assert_eq!(replicated.len(), 1);
    assert_eq!(replicated[0].event_id, durable.event_id);
    assert!(peer.read_after(b.key(), None, 10).unwrap().is_empty());
    assert!(peer.read_after("chat", None, 10).unwrap().is_empty());

    let mut live_a = peer.subscribe_ephemeral(a.key()).unwrap();
    let mut live_b = peer.subscribe_ephemeral(b.key()).unwrap();
    let sent = origin
        .publish_ephemeral(a.key(), &payload(json!("live")))
        .unwrap();
    let ChannelPublication::Event(event) = publications.try_recv().unwrap() else {
        panic!("expected an event")
    };
    assert_eq!(peer.ingest_remote(&event).unwrap(), RemoteIngest::Applied);
    let delivered = tokio::time::timeout(Duration::from_secs(1), live_a.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivered.event_id, sent.event_id);
    // Fanout is FIFO per stream: had `b` received `a`'s event, it would come
    // before `b`'s own.
    let own = origin
        .publish_ephemeral(b.key(), &payload(json!("b live")))
        .unwrap();
    let ChannelPublication::Event(event) = publications.try_recv().unwrap() else {
        panic!("expected an event")
    };
    assert_eq!(peer.ingest_remote(&event).unwrap(), RemoteIngest::Applied);
    let delivered = tokio::time::timeout(Duration::from_secs(1), live_b.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivered.event_id, own.event_id);
}

async fn sse_body(
    store: &Arc<ChannelStore>,
    stream: &ChannelStream,
    after: Option<ReplayCursor>,
) -> String {
    let (mut client, io) = tokio::io::duplex(1 << 16);
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: local\r\n\r\n")
        .await
        .unwrap();
    let mut session = pingora_proxy::Session::new_h1(Box::new(io));
    session.read_request().await.unwrap();
    let mut header = pingora_http::ResponseHeader::build(200, None).unwrap();
    header
        .insert_header("Content-Type", "text/event-stream")
        .unwrap();
    session
        .write_response_header(Box::new(header), false)
        .await
        .unwrap();
    let lifecycle = auth(json!({"ok": true, "maxConnectionLifetimeMs": 150}));
    let resets = store.resets(stream.key());
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

fn sse_messages(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

#[tokio::test]
async fn sse_replays_and_follows_one_binding_under_the_public_channel_name() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let store = Arc::new(ChannelStore::open_in_memory().unwrap());
        let (a, b) = (room("a"), room("b"));
        let start = store.replay_cursor(a.key(), None).unwrap();
        let first = store.append(a.key(), &payload(json!("a1"))).unwrap();
        store.append(b.key(), &payload(json!("b1"))).unwrap();
        let live = {
            let (store, a, b) = (store.clone(), a.clone(), b.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(40)).await;
                store.append(b.key(), &payload(json!("b2"))).unwrap();
                store.append(a.key(), &payload(json!("a2"))).unwrap()
            })
        };
        let body = sse_body(&store, &a, start).await;
        let second = live.await.unwrap();
        assert_eq!(
            sse_messages(&body),
            vec![
                json!({"id": first.id, "eventId": first.event_id, "channel": "chat", "type": "msg", "data": "a1"}),
                json!({"id": second.id, "eventId": second.event_id, "channel": "chat", "type": "msg", "data": "a2"}),
            ]
        );
        // Neither frames nor replay ids reveal the storage key or its params.
        assert!(!body.contains("tako.stream"), "{body}");
        assert!(!body.contains(&a.key()[STREAM_CHANNEL_PREFIX.len()..]));
        assert!(body.contains(&format!("id: {}\n", first.id)));

        // The other binding's cursor is reset, not resumed.
        let foreign = sse_body(&store, &b, Some(cursor(&first))).await;
        assert!(foreign.contains("event: tako.reset"), "{foreign}");
        assert_eq!(sse_messages(&foreign), vec![json!({})], "only the reset");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn ephemeral_sse_frames_carry_the_public_channel_name() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let store = ChannelStore::open_in_memory().unwrap();
        let (a, b) = (room("a"), room("b"));
        let (mut client, io) = tokio::io::duplex(1 << 16);
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: local\r\n\r\n")
            .await
            .unwrap();
        let mut session = pingora_proxy::Session::new_h1(Box::new(io));
        session.read_request().await.unwrap();
        let events = store.subscribe_ephemeral(a.key()).unwrap();
        let header = pingora_http::ResponseHeader::build(200, None).unwrap();
        session
            .write_response_header(Box::new(header), false)
            .await
            .unwrap();
        store
            .publish_ephemeral(b.key(), &payload(json!("b")))
            .unwrap();
        store
            .publish_ephemeral(a.key(), &payload(json!("a")))
            .unwrap();
        let lifecycle =
            auth(json!({"ok": true, "ephemeral": true, "maxConnectionLifetimeMs": 100}));
        let stream = a.clone();
        let server = tokio::spawn(async move {
            ephemeral::serve_events(&mut session, events, &lifecycle, &stream)
                .await
                .unwrap();
        });
        let mut body = String::new();
        client.read_to_string(&mut body).await.unwrap();
        server.await.unwrap();
        let messages = sse_messages(&body);
        assert_eq!(messages.len(), 1, "{body}");
        assert_eq!(messages[0]["channel"], "chat");
        assert_eq!(messages[0]["data"], "a");
        assert!(!body.contains("tako.stream"), "{body}");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn websocket_publishes_and_frames_stay_within_the_connection_binding() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    tokio::time::timeout(Duration::from_secs(10), async {
        let store = Arc::new(ChannelStore::open_in_memory().unwrap());
        let mut sockets = Vec::new();
        let mut servers = Vec::new();
        for name in ["a", "a", "b"] {
            let (client, io) = tokio::io::duplex(1 << 16);
            let store = store.clone();
            let params = json!({ "room": name });
            servers.push(tokio::spawn(async move {
                let mut session = pingora_proxy::Session::new_h1(Box::new(io));
                session.read_request().await.unwrap();
                let response =
                    websocket::build_websocket_upgrade_response(session.req_header()).unwrap();
                session
                    .write_response_header(Box::new(response), false)
                    .await
                    .unwrap();
                // From the start of an empty stream: a connection that has not
                // resolved its head yet still replays what was sent before.
                websocket::serve(
                    &mut session,
                    &store,
                    &ChannelStream::new("chat", &params),
                    ReplayFrom::Start,
                    auth(json!({"ok": true, "transport": "ws"})),
                    params,
                    Default::default(),
                )
                .await
                .unwrap();
            }));
            let (socket, _) = tokio_tungstenite::client_async("ws://local/", client)
                .await
                .unwrap();
            sockets.push(socket);
        }
        sockets[0]
            .send(Message::text(
                json!({"type": "msg", "data": "from a"}).to_string(),
            ))
            .await
            .unwrap();
        for socket in &mut sockets[..2] {
            let frame = socket.next().await.unwrap().unwrap();
            let message: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
            assert_eq!(message["channel"], "chat");
            assert_eq!(message["data"], "from a");
            assert!(!frame.to_text().unwrap().contains("tako.stream"));
        }
        // The other binding stays silent, and its own publish reaches only itself.
        sockets[2]
            .send(Message::text(
                json!({"type": "msg", "data": "from b"}).to_string(),
            ))
            .await
            .unwrap();
        let frame = sockets[2].next().await.unwrap().unwrap();
        let message: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert_eq!(message["data"], "from b");
        // `b` was stored and fanned out before its echo: had `a`'s connection
        // followed it, that frame would arrive before `a`'s next own message.
        sockets[1]
            .send(Message::text(
                json!({"type": "msg", "data": "a again"}).to_string(),
            ))
            .await
            .unwrap();
        let frame = sockets[0].next().await.unwrap().unwrap();
        let message: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert_eq!(message["data"], "a again");
        assert_eq!(
            store.read_after(room("a").key(), None, 10).unwrap().len(),
            2
        );
        assert_eq!(
            store.read_after(room("b").key(), None, 10).unwrap().len(),
            1
        );
        assert!(store.read_after("chat", None, 10).unwrap().is_empty());
        for socket in &mut sockets {
            socket.close(None).await.unwrap();
        }
        for server in servers {
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[test]
fn sqlite_refuses_new_streams_beyond_the_app_limit_without_breaking_existing_ones() {
    let store = ChannelStore::open_in_memory_without_maintenance();
    let lifecycle = auth(json!({"ok": true}));
    let existing = room("existing");
    store.sync_channel(existing.key(), &lifecycle).unwrap();
    store.sync_channel("base", &lifecycle).unwrap();
    // The trigger-maintained counter tracks every stream row.
    let streams = || store.raw_query_i64("SELECT stream_count FROM channel_store_state", []);
    assert_eq!(streams(), 2);
    store.raw_execute(
        "UPDATE channel_store_state SET stream_count = ?1",
        [crate::store::MAX_STREAMS],
    );

    let refused =
        |result: Result<_, ChannelError>| matches!(result, Err(ChannelError::StreamLimit));
    // Neither a subscriber nor a publisher can start another parameterized stream.
    assert!(refused(store.sync_channel(room("new").key(), &lifecycle)));
    assert!(refused(
        store
            .append(room("new").key(), &payload(json!(1)))
            .map(|_| ())
    ));
    assert!(refused(
        store
            .sync_channel(
                &crate::live_store::store_scope("tako.store:notes", &json!({"id": 1})),
                &lifecycle
            )
            .map(|_| ())
    ));
    // Streams that exist, and unparameterized channels, keep working.
    store.sync_channel(existing.key(), &lifecycle).unwrap();
    store.append(existing.key(), &payload(json!(2))).unwrap();
    store.append("base", &payload(json!(3))).unwrap();
    store.sync_channel("other-base", &lifecycle).unwrap();
    assert_eq!(store.read_after(existing.key(), None, 10).unwrap().len(), 1);
    // A mesh event for a new stream is acknowledged and reported as such, so
    // the broker can say which app is at its limit; existing streams apply.
    let event = |sequence: u64, channel: &str| crate::ChannelEvent {
        id: crate::ChannelEventId {
            origin: "peer".into(),
            sequence,
        },
        channel: channel.into(),
        payload: payload(json!(4)),
        ephemeral: false,
        lifecycle: None,
        created_at_unix_ms: None,
    };
    assert_eq!(
        store.ingest_remote(&event(1, room("new").key())).unwrap(),
        crate::RemoteIngest::StreamLimit
    );
    assert_eq!(
        store.ingest_remote(&event(2, existing.key())).unwrap(),
        crate::RemoteIngest::Applied
    );
}

#[test]
fn sqlite_maintenance_work_per_tick_is_bounded() {
    let store = ChannelStore::open_in_memory_without_maintenance();
    let lifecycle = auth(json!({
        "ok": true, "replayWindowMs": 1_000, "maxConnectionLifetimeMs": 1_000
    }));
    for n in 0..1_500 {
        let stream = room(&n.to_string());
        store.sync_channel(stream.key(), &lifecycle).unwrap();
        store.append(stream.key(), &payload(json!(n))).unwrap();
    }
    let rows = |table: &str| store.raw_query_i64(&format!("SELECT COUNT(*) FROM {table}"), []);
    let later = now() + 2_000;
    // One tick prunes the first 1,000 streams; the next continues after them.
    store.prune_at(later);
    assert_eq!(rows("channel_messages"), 500);
    store.prune_at(later);
    assert_eq!(rows("channel_messages"), 0);
    // Idle release is bounded the same way.
    let idle = now() + 1_000 + 60_000 + 2_000;
    store.prune_at(idle);
    assert_eq!(rows("channel_metadata"), 500);
    store.prune_at(idle);
    assert_eq!(rows("channel_metadata"), 0);
    assert_eq!(
        store.raw_query_i64("SELECT stream_count FROM channel_store_state", []),
        0
    );
}

#[test]
fn postgres_refuses_new_streams_beyond_the_app_limit_and_bounds_maintenance() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!("channel_limit_{}", std::process::id());
    let store = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    let other_app = ChannelStore::open_postgres_with_schema(&url, &schema, "other").unwrap();
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let lifecycle = auth(json!({"ok": true}));
    let existing = room("existing");
    store.sync_channel(existing.key(), &lifecycle).unwrap();
    client
        .batch_execute(&format!(
            "INSERT INTO {schema}.channel_metadata (app_id, channel, replay_window_ms,
                inactivity_ttl_ms, keepalive_interval_ms, max_connection_lifetime_ms,
                last_activity_unix_ms)
             SELECT 'app', 'tako.stream:filler' || n, 600000, 0, 25000, 7200000, {}
             FROM generate_series(2, {}) AS n",
            now(),
            crate::store::MAX_STREAMS
        ))
        .unwrap();
    let refused =
        |result: Result<_, ChannelError>| matches!(result, Err(ChannelError::StreamLimit));
    assert!(refused(store.sync_channel(room("new").key(), &lifecycle)));
    assert!(refused(
        store
            .append(room("new").key(), &payload(json!(1)))
            .map(|_| ())
    ));
    store.sync_channel(existing.key(), &lifecycle).unwrap();
    store.append(existing.key(), &payload(json!(2))).unwrap();
    store.append("base", &payload(json!(3))).unwrap();
    // The limit is per app.
    other_app
        .sync_channel(room("new").key(), &lifecycle)
        .unwrap();

    // Every filler stream is idle: one tick releases at most 1,000 of them.
    let count = |client: &mut postgres::Client| -> i64 {
        client
            .query_one(
                &format!("SELECT COUNT(*) FROM {schema}.channel_metadata WHERE app_id = 'app'"),
                &[],
            )
            .unwrap()
            .get(0)
    };
    let before = count(&mut client);
    store.prune_at(now() + 7_200_000 + 60_000 + 2_000);
    assert_eq!(before - count(&mut client), 1_000);
    drop((store, other_app));
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}

fn client_for_migration(url: &str) -> postgres::Client {
    postgres::Client::connect(url, postgres::NoTls).unwrap()
}

#[test]
fn postgres_stream_counter_stays_exact_across_creation_release_and_concurrency() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!("channel_counter_{}", std::process::id());
    let store = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    let other_app = ChannelStore::open_postgres_with_schema(&url, &schema, "other").unwrap();
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    // The cap check reads the counter; it must always equal the rows.
    let mut check = |expected: i64| {
        let row = client
            .query_one(
                &format!(
                    "SELECT (SELECT COUNT(*) FROM {schema}.channel_metadata WHERE app_id = 'app'),
                        COALESCE((SELECT streams FROM {schema}.channel_stream_counts
                                  WHERE app_id = 'app'), 0)"
                ),
                &[],
            )
            .unwrap();
        let (rows, counted): (i64, i64) = (row.get(0), row.get(1));
        assert_eq!((rows, counted), (expected, expected));
    };
    let lifecycle = auth(json!({"ok": true}));
    store.sync_channel(room("a").key(), &lifecycle).unwrap();
    store.append(room("b").key(), &payload(json!(1))).unwrap();
    store.append("base", &payload(json!(2))).unwrap();
    // Touching existing streams starts nothing.
    store.sync_channel(room("a").key(), &lifecycle).unwrap();
    store.append(room("b").key(), &payload(json!(3))).unwrap();
    other_app.sync_channel(room("a").key(), &lifecycle).unwrap();
    check(3);

    // Two connections, as two servers, start streams at the same time.
    let second = Arc::new(ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap());
    let first = Arc::new(store);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = [(first.clone(), "x"), (second.clone(), "y")]
        .into_iter()
        .map(|(store, prefix)| {
            let barrier = barrier.clone();
            let lifecycle = lifecycle.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for n in 0..40 {
                    let stream = room(&format!("{prefix}{n}"));
                    if n % 2 == 0 {
                        store.sync_channel(stream.key(), &lifecycle).unwrap();
                    } else {
                        store.append(stream.key(), &payload(json!(n))).unwrap();
                    }
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    check(83);

    // Releasing idle streams counts down; only messages' streams remain.
    first.prune_at(now() + 7_200_000 + 60_000 + 2_000);
    check(0);
    first.sync_channel(room("again").key(), &lifecycle).unwrap();
    check(1);

    // A store from before the counter gets it initialized from its rows.
    client_for_migration(&url)
        .batch_execute(&format!(
            "DROP TABLE {schema}.channel_stream_counts;
             DROP FUNCTION {schema}.channel_streams_started() CASCADE;
             DROP FUNCTION {schema}.channel_streams_released() CASCADE;
             DELETE FROM {schema}.channel_schema_migrations
             WHERE version = (SELECT MAX(version) FROM {schema}.channel_schema_migrations);"
        ))
        .unwrap();
    let migrated = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    check(1);
    migrated
        .sync_channel(room("after").key(), &lifecycle)
        .unwrap();
    check(2);
    drop((first, second, other_app, migrated));
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}

#[test]
fn postgres_readers_never_see_expired_history_that_maintenance_has_not_deleted() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!("channel_expired_{}", std::process::id());
    let store = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let mut run = |sql: &str, channel: &str| {
        client
            .execute(
                &sql.replace("{schema}", &schema),
                &[&channel, &(now() - 61_000)],
            )
            .unwrap();
    };
    // By message age: rows older than the replay window are gone for readers.
    let aged = room("aged");
    store
        .sync_channel(
            aged.key(),
            &auth(json!({"ok": true, "replayWindowMs": 60_000})),
        )
        .unwrap();
    let old = store.append(aged.key(), &payload(json!(1))).unwrap();
    let last_old = store.append(aged.key(), &payload(json!(2))).unwrap();
    let fresh = store.append(aged.key(), &payload(json!(3))).unwrap();
    run(
        "UPDATE {schema}.channel_messages SET created_at_unix_ms = $2
         WHERE app_id = 'app' AND channel = $1 AND data_json <> '3'",
        aged.key(),
    );
    assert_eq!(
        store.read_after(aged.key(), None, 10).unwrap(),
        vec![fresh.clone()]
    );
    assert!(matches!(
        store.read_after(aged.key(), Some(cursor(&old)), 10),
        Err(ChannelError::StaleCursor)
    ));
    assert!(matches!(
        store.replay_cursor(aged.key(), Some(cursor(&old))),
        Err(ChannelError::StaleCursor)
    ));
    // A reader that saw every expired row resumes.
    assert_eq!(
        store
            .read_after(aged.key(), Some(cursor(&last_old)), 10)
            .unwrap(),
        vec![fresh]
    );

    // By inactivity: the whole history of an idle stream is gone, its marker too.
    let idle = room("idle");
    store
        .sync_channel(
            idle.key(),
            &auth(json!({"ok": true, "inactivityTtlMs": 60_000})),
        )
        .unwrap();
    let first = store.append(idle.key(), &payload(json!(1))).unwrap();
    store.close(idle.key()).unwrap();
    run(
        "UPDATE {schema}.channel_metadata SET last_activity_unix_ms = $2
         WHERE app_id = 'app' AND channel = $1",
        idle.key(),
    );
    assert!(store.read_after(idle.key(), None, 10).unwrap().is_empty());
    assert!(matches!(
        store.read_after(idle.key(), Some(cursor(&first)), 10),
        Err(ChannelError::StaleCursor)
    ));
    assert_eq!(store.close_marker(idle.key()).unwrap(), None);
    // Until maintenance deletes it, the marker still keeps the stream closed.
    assert!(matches!(
        store.append(idle.key(), &payload(json!(2))),
        Err(ChannelError::StreamClosed)
    ));
    drop(store);
    postgres::Client::connect(&url, postgres::NoTls)
        .unwrap()
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}

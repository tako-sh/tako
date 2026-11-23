//! Params select a stream: storage, replay, cursors, fanout and mesh wakes.
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
fn bindings_have_separate_replay_and_cursors() {
    let db = TestDb::start();
    let store = db.writer();
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
fn releases_idle_streams_and_resets_their_old_cursors() {
    let db = TestDb::start();
    let store = db.without_maintenance();
    let other_app = db.without_maintenance_for("other");
    let (idle, live, retained) = (room("idle"), room("live"), room("retained"));
    let lifecycle = auth(json!({"ok": true, "maxConnectionLifetimeMs": 10_000}));
    for stream in [&idle, &live, &retained] {
        store.sync_channel(stream.key(), &lifecycle).unwrap();
    }
    other_app.sync_channel(idle.key(), &lifecycle).unwrap();
    let seen = store.append(idle.key(), &payload(json!(1))).unwrap();
    let kept = store.append(retained.key(), &payload(json!(2))).unwrap();
    let rows = |app: &str| {
        db.query_i64(&format!(
            "SELECT COUNT(*) FROM tako_replay_streams WHERE app = '{app}'"
        ))
    };

    // Expired replay alone keeps the stream: a subscriber may still be connected,
    // and a reader that saw everything resumes without a reset.
    db.execute(&format!(
        "UPDATE tako_replay_messages SET created_ms = 0 WHERE app = 'app' AND channel = '{}'",
        idle.key()
    ));
    store.prune();
    assert_eq!(
        db.query_i64("SELECT COUNT(*) FROM tako_replay_messages WHERE app = 'app'"),
        1
    );
    assert_eq!(rows("app"), 3);
    assert_eq!(
        store
            .replay_cursor(idle.key(), Some(cursor(&seen)))
            .unwrap(),
        Some(cursor(&seen))
    );

    // Move every stream but `live` past its connection lifetime. Retained
    // messages and recent subscribers keep their streams; other apps are
    // untouched.
    db.execute(&format!(
        "UPDATE tako_replay_streams SET activity_ms = 1 WHERE app = 'app' AND channel <> '{}'",
        live.key()
    ));
    store.prune();
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

    // A released stream is reborn caught up with the app's history. The cursor
    // from its earlier life is stale even though nothing below it is retained.
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
        vec![reborn.clone()]
    );

    // A stream first seen through a publish is born the same way.
    let fresh = store
        .append(room("fresh").key(), &payload(json!(4)))
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
    let db = TestDb::start();
    let store = db.without_maintenance();
    let stream = room("a");
    let lifetime = |ms: u64| auth(json!({"ok": true, "maxConnectionLifetimeMs": ms}));
    store.sync_channel(stream.key(), &lifetime(50_000)).unwrap();
    // A later grant with a shorter lifetime must not expose the longer connection.
    store.sync_channel(stream.key(), &lifetime(1_000)).unwrap();
    assert_eq!(
        db.query_i64("SELECT lifetime_ms FROM tako_replay_streams"),
        50_000
    );
    db.age(1_000 + 60_000 + 2_000);
    store.prune();
    assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_streams"), 1);
    db.age(50_000);
    store.prune();
    assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_streams"), 0);
}

#[tokio::test]
async fn mesh_wakes_and_ephemeral_fanout_stay_within_one_binding() {
    let db = TestDb::start();
    // Two servers' brokers for one app share its history.
    let (origin, peer) = (db.open().await, db.open().await);
    let mut publications = origin.publications();
    let (a, b) = (room("a"), room("b"));

    let mut changes_a = peer.changes(a.key());
    let mut changes_b = peer.changes(b.key());
    changes_a.borrow_and_update();
    changes_b.borrow_and_update();
    let durable = origin
        .append_async(a.key(), &payload(json!("a1")))
        .await
        .unwrap();
    // Peers wake the opaque key, as they do for Stores.
    let ChannelPublication::Wake { channel } = publications.try_recv().unwrap() else {
        panic!("expected a wake hint")
    };
    assert_eq!(channel, a.key());
    peer.wake(&channel);
    assert!(changes_a.has_changed().unwrap());
    assert!(!changes_b.has_changed().unwrap());
    let shared = peer.read_after_async(a.key(), None, 10).await.unwrap();
    assert_eq!(shared, vec![durable]);
    assert!(
        peer.read_after_async(b.key(), None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        peer.read_after_async("chat", None, 10)
            .await
            .unwrap()
            .is_empty()
    );

    let mut live_events = origin.ephemeral_publications();
    let mut live_a = peer.subscribe_ephemeral(a.key()).unwrap();
    let mut live_b = peer.subscribe_ephemeral(b.key()).unwrap();
    let sent = origin
        .publish_ephemeral(a.key(), &payload(json!("live")))
        .unwrap();
    let event = live_events.try_recv().unwrap();
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
    let event = live_events.try_recv().unwrap();
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
        let db = TestDb::start();
        let store = db.open().await;
        let (a, b) = (room("a"), room("b"));
        let start = store.replay_cursor_async(a.key(), None).await.unwrap();
        let first = store
            .append_async(a.key(), &payload(json!("a1")))
            .await
            .unwrap();
        store
            .append_async(b.key(), &payload(json!("b1")))
            .await
            .unwrap();
        let live = {
            let (store, a, b) = (store.clone(), a.clone(), b.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(40)).await;
                store
                    .append_async(b.key(), &payload(json!("b2")))
                    .await
                    .unwrap();
                store
                    .append_async(a.key(), &payload(json!("a2")))
                    .await
                    .unwrap()
            })
        };
        let body = sse_body(&store, &a, start).await;
        let second = live.await.unwrap();
        assert_eq!(
            sse_messages(&body),
            vec![
                json!({"id": first.id, "channel": "chat", "type": "msg", "data": "a1"}),
                json!({"id": second.id, "channel": "chat", "type": "msg", "data": "a2"}),
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
        let db = TestDb::start();
        let store = db.open().await;
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
        let db = TestDb::start();
        let store = db.open().await;
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
        for (key, count) in [
            (room("a").key().to_owned(), 2),
            (room("b").key().to_owned(), 1),
            ("chat".into(), 0),
        ] {
            assert_eq!(
                store.read_after_async(&key, None, 10).await.unwrap().len(),
                count
            );
        }
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
fn refuses_new_streams_beyond_the_app_limit_without_breaking_existing_ones() {
    let db = TestDb::start();
    let store = db.without_maintenance();
    let other_app = db.without_maintenance_for("other");
    let lifecycle = auth(json!({"ok": true}));
    let existing = room("existing");
    store.sync_channel(existing.key(), &lifecycle).unwrap();
    store.sync_channel("base", &lifecycle).unwrap();
    // The trigger-maintained counter tracks every stream row.
    let streams = || db.query_i64("SELECT stream_count FROM tako_replay_apps WHERE app = 'app'");
    assert_eq!(streams(), 2);
    db.execute(&format!(
        "UPDATE tako_replay_apps SET stream_count = {} WHERE app = 'app'",
        crate::store::MAX_STREAMS
    ));

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
    // The limit is per app.
    other_app
        .sync_channel(room("new").key(), &lifecycle)
        .unwrap();
}

#[test]
fn maintenance_work_per_tick_is_bounded() {
    const STREAMS: i64 = 5_000;
    const PER_TICK: i64 = 4_096;
    let db = TestDb::start();
    let store = db.without_maintenance();
    // Thousands of streams with one expired message each, inserted directly:
    // the triggers keep heads and counters as appends would.
    db.execute(&format!(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {STREAMS})
         INSERT INTO tako_replay_streams(app, channel, head, floor, activity_ms, window_ms,
             inactivity_ms, lifetime_ms)
         SELECT 'app', 'tako.stream:' || i, 0, 0, CAST(unixepoch('subsec') * 1000 AS INTEGER),
             1000, 0, 1000 FROM n;
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {STREAMS})
         INSERT INTO tako_replay_messages(app, channel, id, type, data_json, bytes, created_ms)
         SELECT 'app', 'tako.stream:' || i, i, 'msg', '1', 1, 0 FROM n;"
    ));
    let rows = |table: &str| db.query_i64(&format!("SELECT COUNT(*) FROM {table}"));
    assert_eq!(rows("tako_replay_messages"), STREAMS);
    // One tick deletes the first rows; the next continues after them.
    store.prune();
    assert_eq!(rows("tako_replay_messages"), STREAMS - PER_TICK);
    store.prune();
    assert_eq!(rows("tako_replay_messages"), 0);
    assert_eq!(rows("tako_replay_streams"), STREAMS);
    // Idle release is bounded the same way.
    db.age(1_000 + 60_000 + 2_000);
    store.prune();
    assert_eq!(rows("tako_replay_streams"), STREAMS - PER_TICK);
    store.prune();
    assert_eq!(rows("tako_replay_streams"), 0);
    assert_eq!(db.query_i64("SELECT stream_count FROM tako_replay_apps"), 0);
}

#[test]
fn stream_counter_stays_exact_across_creation_release_and_concurrency() {
    let db = TestDb::start();
    let store = db.without_maintenance();
    let other_app = db.without_maintenance_for("other");
    // The cap check reads the counter; it must always equal the rows.
    let check = |expected: i64| {
        let rows = db.query_i64("SELECT COUNT(*) FROM tako_replay_streams WHERE app = 'app'");
        let counted = db.query_i64("SELECT stream_count FROM tako_replay_apps WHERE app = 'app'");
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

    // Two handles, as two servers, start streams at the same time.
    let second = db.without_maintenance();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for (store, prefix) in [(&store, "x"), (&second, "y")] {
            let (barrier, lifecycle) = (&barrier, &lifecycle);
            scope.spawn(move || {
                barrier.wait();
                for n in 0..40 {
                    let stream = room(&format!("{prefix}{n}"));
                    if n % 2 == 0 {
                        store.sync_channel(stream.key(), lifecycle).unwrap();
                    } else {
                        store.append(stream.key(), &payload(json!(n))).unwrap();
                    }
                }
            });
        }
    });
    check(83);

    // Once every stream has gone idle and its messages have aged out,
    // releasing them counts down to zero.
    db.execute(
        "UPDATE tako_replay_streams SET activity_ms = 1 WHERE app = 'app';
         UPDATE tako_replay_messages SET created_ms = 1 WHERE app = 'app';",
    );
    store.prune();
    check(0);
    store.sync_channel(room("again").key(), &lifecycle).unwrap();
    check(1);
}

#[test]
fn readers_never_see_expired_history_that_maintenance_has_not_deleted() {
    let db = TestDb::start();
    let store = db.without_maintenance();
    let past = |table: &str, column: &str, channel: &str, filter: &str| {
        db.execute(&format!(
            "UPDATE {table} SET {column} = CAST(unixepoch('subsec') * 1000 AS INTEGER) - 61000
             WHERE channel = '{channel}' {filter}"
        ));
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
    past(
        "tako_replay_messages",
        "created_ms",
        aged.key(),
        "AND data_json <> '3'",
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
    past("tako_replay_streams", "activity_ms", idle.key(), "");
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
}

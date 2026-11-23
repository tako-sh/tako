use crate::test_db::TestDb;
use crate::*;
use tokio::sync::broadcast;

fn payload(n: usize) -> ChannelPublishPayload {
    ChannelPublishPayload {
        r#type: "event".into(),
        data: serde_json::json!(n),
    }
}

fn wake(channel: &str) -> ChannelPublication {
    ChannelPublication::Wake {
        channel: channel.into(),
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[tokio::test]
async fn committed_notifications_wake_readers_without_persisting_or_republishing() {
    let db = TestDb::start();
    let (writer, reader) = (db.open_for("writer").await, db.open_for("reader").await);
    let mut publications = reader.publications();
    let mut changes = reader.changes("events");
    let durable = writer.append_async("events", &payload(1)).await.unwrap();
    reader.notify_committed(&durable, false);
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    reader.notify_committed(&durable, false);
    assert!(!changes.has_changed().unwrap());
    assert!(
        reader
            .read_after_async("events", None, 10)
            .await
            .unwrap()
            .is_empty()
    );

    let mut live = reader.subscribe_ephemeral("live").unwrap();
    let ephemeral = writer.publish_ephemeral("live", &payload(2)).unwrap();
    reader.notify_committed(&ephemeral, true);
    assert_eq!(*live.recv().await.unwrap(), ephemeral);
    assert!(
        reader
            .read_after_async("live", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        publications.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

#[test]
fn broker_reader_never_creates_or_mutates_a_database() {
    let db = TestDb::start();
    let schema = || db.query_i64("SELECT COUNT(*) FROM sqlite_schema");
    let empty = schema();
    assert!(ChannelStore::open_reader_config(db.config("app")).is_err());
    assert_eq!(schema(), empty);

    let writer = db.writer();
    let cursor = writer.replay_cursor("events", None).unwrap();
    let reader = db.reader();
    let changes = reader.changes("events");
    let message = writer.append("events", &payload(1)).unwrap();
    wait_until("the reader did not poll the commit", || {
        changes.has_changed().unwrap()
    });
    let snapshot = || {
        db.query_i64(
            "SELECT (SELECT COUNT(*) FROM tako_replay_streams) * 1000
                + (SELECT COUNT(*) FROM tako_replay_messages)",
        )
    };
    let before = snapshot();
    assert_eq!(
        reader.read_after("events", cursor, 10).unwrap(),
        vec![message]
    );
    assert!(reader.replay_cursor("unseen", None).is_ok());
    assert!(reader.append("events", &payload(2)).is_err());
    assert!(reader.close("events").is_err());
    assert!(reader.publish_ephemeral("events", &payload(2)).is_err());
    assert!(reader.invalidate_replay().is_err());
    assert!(
        reader
            .sync_channel("unseen", &ChannelLifecycle::default().auth())
            .is_err()
    );
    assert!(
        reader
            .update_presence(
                "events",
                &PresenceMember {
                    connection_id: "connection".into(),
                    subject: None,
                    data: serde_json::Value::Null,
                },
                0
            )
            .is_err()
    );
    assert!(reader.remove_presence("events", "connection").is_err());
    assert_eq!(snapshot(), before);
    // Another app's reader finds no state of its own and creates none.
    assert!(ChannelStore::open_reader_config(db.config("other")).is_err());
    assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_apps"), 1);
}

#[test]
fn broker_reader_enforces_expiry_and_replay_fences_without_pruning() {
    let db = TestDb::start();
    let writer = db.without_maintenance();
    let cursor = writer.replay_cursor("events", None).unwrap();
    writer.append("events", &payload(1)).unwrap();
    let reader = db.reader();
    db.execute("UPDATE tako_replay_messages SET created_ms = 1");
    assert!(reader.read_after("events", None, 10).unwrap().is_empty());
    assert!(matches!(
        reader.read_after("events", cursor, 10),
        Err(ChannelError::StaleCursor)
    ));
    assert!(matches!(
        reader.replay_cursor("events", cursor),
        Err(ChannelError::StaleCursor)
    ));
    let head = reader.replay_cursor("events", None).unwrap();
    // A restored or imported database gets a new epoch before it is served.
    db.execute("UPDATE tako_replay_apps SET epoch = lower(hex(randomblob(16)))");
    assert!(matches!(
        reader.replay_cursor("events", head),
        Err(ChannelError::StaleCursor)
    ));
    assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_messages"), 1);
}

#[tokio::test]
async fn peers_ingest_live_events_without_rebroadcast_and_refuse_durable_ones() {
    let db = TestDb::start();
    let (a, b) = (db.open().await, db.open().await);
    let mut outbound = a.publications();
    let mut echoes = b.publications();
    let durable = a.append_async("events", &payload(1)).await.unwrap();
    let hint = outbound.try_recv().unwrap();
    assert_eq!(hint, wake("events"));
    let wire = serde_json::to_string(&hint).unwrap();
    assert_eq!(
        serde_json::from_str::<ChannelPublication>(&wire).unwrap(),
        hint
    );
    // History is shared: a peer only wakes its readers, and reads the same ids.
    let changes = b.changes("events");
    b.wake("events");
    assert!(changes.has_changed().unwrap());
    assert_eq!(
        b.read_after_async("events", None, 10).await.unwrap(),
        vec![durable.clone()]
    );
    let event = ChannelEvent {
        id: ChannelEventId {
            origin: "remote".into(),
            sequence: 1,
        },
        channel: "events".into(),
        payload: payload(2),
        ephemeral: false,
    };
    assert!(b.ingest_remote(&event).is_err());

    let mut live = b.subscribe_ephemeral("live").unwrap();
    let mut live_events = a.ephemeral_publications();
    let sent = a.publish_ephemeral("live", &payload(2)).unwrap();
    let event = live_events.try_recv().unwrap();
    assert_eq!(b.ingest_remote(&event).unwrap(), RemoteIngest::Applied);
    assert_eq!(*live.recv().await.unwrap(), sent);
    assert_eq!(b.ingest_remote(&event).unwrap(), RemoteIngest::Duplicate);
    assert_eq!(a.ingest_remote(&event).unwrap(), RemoteIngest::Duplicate);
    assert!(
        b.read_after_async("live", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        b.read_after_async("events", None, 10).await.unwrap(),
        vec![durable]
    );
    assert!(matches!(
        echoes.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

#[test]
fn transport_gaps_are_out_of_band_and_scoped_resets_do_not_publish() {
    let db = TestDb::start();
    let store = db.writer();
    let mut events = store.publications();
    let mut a = store.resets("a");
    let mut b = store.resets("b");
    store.signal_gap(Some("a"));
    assert!(a.has_changed().unwrap());
    assert!(!b.has_changed().unwrap());
    a.borrow_and_update();
    store.signal_gap(None);
    assert!(a.has_changed().unwrap());
    assert!(b.has_changed().unwrap());
    a.borrow_and_update();
    b.borrow_and_update();
    // A live event from a peer, even a skipped one, resets nothing.
    let event = ChannelEvent {
        id: ChannelEventId {
            origin: "live_remote".into(),
            sequence: 2,
        },
        channel: "a".into(),
        payload: payload(2),
        ephemeral: true,
    };
    assert_eq!(store.ingest_remote(&event).unwrap(), RemoteIngest::Applied);
    assert_eq!(
        store
            .ingest_remote(&ChannelEvent {
                id: ChannelEventId {
                    sequence: 1,
                    ..event.id.clone()
                },
                ..event.clone()
            })
            .unwrap(),
        RemoteIngest::Duplicate
    );
    assert!(!a.has_changed().unwrap());
    assert!(!b.has_changed().unwrap());
    // Fencing after transport loss keeps the shared history.
    let message = store.append("a", &payload(3)).unwrap();
    assert_eq!(events.try_recv().unwrap(), wake("a"));
    store.invalidate_replay().unwrap();
    assert!(a.has_changed().unwrap());
    assert!(b.has_changed().unwrap());
    assert_eq!(
        store.read_after("a", None, 100).unwrap(),
        vec![message.clone()]
    );
    assert_eq!(
        store.replay_cursor("a", message.id.parse().ok()).unwrap(),
        message.id.parse().ok()
    );
    assert!(store.read_after("b", None, 100).unwrap().is_empty());
    assert!(events.try_recv().is_err());
}

#[test]
fn concurrent_publishers_preserve_independent_durable_and_live_orders() {
    let db = TestDb::start();
    let store = std::sync::Arc::new(db.writer());
    let mut events = store.publications();
    let mut live = store.ephemeral_publications();
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || {
                for n in 0..20 {
                    if n % 2 == 0 {
                        store.append("events", &payload(n)).unwrap();
                    } else {
                        store.publish_ephemeral("events", &payload(n)).unwrap();
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    for sequence in 1..=20 {
        assert_eq!(events.try_recv().unwrap(), wake("events"));
        assert_eq!(live.try_recv().unwrap().id.sequence, sequence);
    }
    let positions: Vec<i64> = store
        .read_after("events", None, 100)
        .unwrap()
        .iter()
        .map(|message| message.id.parse::<ReplayCursor>().unwrap().position)
        .collect();
    assert_eq!(positions, (1..=20).collect::<Vec<_>>());
}

#[tokio::test]
async fn reset_ends_ephemeral_subscription_before_queued_business_events() {
    let db = TestDb::start();
    let store = db.open().await;
    let mut reset = store.subscribe_ephemeral("reset").unwrap();
    let mut other = store.subscribe_ephemeral("other").unwrap();
    store.publish_ephemeral("reset", &payload(1)).unwrap();
    store.publish_ephemeral("other", &payload(2)).unwrap();
    store.signal_gap(Some("reset"));
    assert!(reset.recv().await.is_none());
    assert!(reset.recv().await.is_none());
    assert_eq!(other.recv().await.unwrap().data, 2);
    assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_messages"), 0);
}

#[test]
fn live_publication_overflow_does_not_affect_durable_publications_or_cursors() {
    let db = TestDb::start();
    let store = db.writer();
    let mut events = store.publications();
    let mut live = store.ephemeral_publications();
    let resets = store.resets("a");
    for n in 0..300 {
        store.publish_ephemeral("a", &payload(n)).unwrap();
    }
    assert!(matches!(
        live.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(_))
    ));
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert!(!resets.has_changed().unwrap());
    assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_messages"), 0);
    assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_streams"), 0);
}

#[test]
fn mesh_uses_wake_hints_without_reinserting_remote_events() {
    let db = TestDb::start();
    let writer = db.writer();
    let reader = db.writer();
    let mut events = writer.publications();
    let mut echoes = reader.publications();
    let after = reader.replay_cursor("a", None).unwrap();
    let changes = reader.changes("a");
    let message = writer.append("a", &payload(1)).unwrap();
    assert_eq!(events.try_recv().unwrap(), wake("a"));
    reader.wake("a");
    assert!(changes.has_changed().unwrap());
    assert_eq!(reader.read_after("a", after, 100).unwrap(), vec![message]);
    let remote = ChannelEvent {
        id: ChannelEventId {
            origin: "remote".into(),
            sequence: 1,
        },
        channel: "a".into(),
        payload: payload(2),
        ephemeral: false,
    };
    assert!(reader.ingest_remote(&remote).is_err());
    assert!(echoes.try_recv().is_err());
    assert_eq!(reader.read_after("a", after, 100).unwrap().len(), 1);
}

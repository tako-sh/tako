use crate::*;
use tokio::sync::broadcast;

fn payload(n: usize) -> ChannelPublishPayload {
    ChannelPublishPayload {
        r#type: "event".into(),
        data: serde_json::json!(n),
    }
}

fn next_event(publications: &mut broadcast::Receiver<ChannelPublication>) -> ChannelEvent {
    let ChannelPublication::Event(event) = publications.try_recv().unwrap() else {
        panic!("expected an event")
    };
    event
}

#[tokio::test]
async fn committed_notifications_wake_readers_without_persisting_or_republishing() {
    let temp = tempfile::tempdir().unwrap();
    let writer = ChannelStore::open(&temp.path().join("writer.sqlite")).unwrap();
    let reader = ChannelStore::open(&temp.path().join("reader.sqlite")).unwrap();
    let mut publications = reader.publications();
    let changes = reader.changes("events");
    let durable = writer.append("events", &payload(1)).unwrap();
    reader.notify_committed(&durable, false);
    assert!(changes.has_changed().unwrap());
    assert!(reader.read_after("events", None, 10).unwrap().is_empty());

    let mut live = reader.subscribe_ephemeral("live").unwrap();
    let ephemeral = writer.publish_ephemeral("live", &payload(2)).unwrap();
    reader.notify_committed(&ephemeral, true);
    assert_eq!(*live.recv().await.unwrap(), ephemeral);
    assert!(reader.read_after("live", None, 10).unwrap().is_empty());
    assert!(matches!(
        publications.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

#[test]
fn canonical_handles_publish_and_wake_synchronously_for_all_paths() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("a.sqlite");
    let one = ChannelStore::open(&path).unwrap();
    let two = ChannelStore::open(&temp.path().join(".").join("a.sqlite")).unwrap();
    let mut publications = one.publications();
    let mut changes = one.changes("events");
    let untouched = one.changes("other");
    let durable = two.append("events", &payload(1)).unwrap();
    assert!(changes.has_changed().unwrap());
    assert!(!untouched.has_changed().unwrap());
    changes.borrow_and_update();
    let first = next_event(&mut publications);
    assert_eq!(durable.event_id.as_ref(), Some(&first.id));
    assert_eq!(first.payload, payload(1));
    assert!(!first.ephemeral);
    let ephemeral = two.publish_ephemeral("events", &payload(2)).unwrap();
    assert!(changes.has_changed().unwrap());
    let second = next_event(&mut publications);
    assert!(second.ephemeral);
    assert_eq!(second.id.sequence, first.id.sequence + 1);
    assert_eq!(second.id.origin, first.id.origin);
    assert_eq!(ephemeral.event_id.as_ref(), Some(&second.id));
    assert!(publications.try_recv().is_err());
    assert_eq!(one.read_after("events", None, 10).unwrap(), vec![durable]);
}

#[tokio::test]
async fn separate_databases_ingest_durable_and_ephemeral_without_rebroadcast() {
    let temp = tempfile::tempdir().unwrap();
    let a = ChannelStore::open(&temp.path().join("a.sqlite")).unwrap();
    let b = ChannelStore::open(&temp.path().join("b.sqlite")).unwrap();
    let mut outbound = a.publications();
    let mut echoes = b.publications();
    let durable = a.append("events", &payload(1)).unwrap();
    let event = next_event(&mut outbound);
    let wire = serde_json::to_string(&ChannelPublication::Event(event.clone())).unwrap();
    assert_eq!(
        serde_json::from_str::<ChannelPublication>(&wire).unwrap(),
        ChannelPublication::Event(event.clone())
    );
    let changes = b.changes("events");
    assert_eq!(b.ingest_remote(&event).unwrap(), RemoteIngest::Applied);
    assert!(changes.has_changed().unwrap());
    assert_eq!(b.ingest_remote(&event).unwrap(), RemoteIngest::Duplicate);
    assert_eq!(a.ingest_remote(&event).unwrap(), RemoteIngest::Duplicate);
    let remote = b.read_after("events", None, 10).unwrap();
    assert_eq!(remote.len(), 1);
    assert_ne!(remote[0].id, durable.id);
    assert_eq!(remote[0].event_id, durable.event_id);
    assert!(matches!(
        b.replay_cursor("events", durable.id.parse().ok()),
        Err(ChannelError::StaleCursor)
    ));
    let mut live = b.subscribe_ephemeral("live").unwrap();
    let sent = a.publish_ephemeral("live", &payload(2)).unwrap();
    let event = next_event(&mut outbound);
    assert_eq!(b.ingest_remote(&event).unwrap(), RemoteIngest::Applied);
    assert_eq!(*live.recv().await.unwrap(), sent);
    assert_eq!(b.ingest_remote(&event).unwrap(), RemoteIngest::Duplicate);
    assert!(b.read_after("live", None, 10).unwrap().is_empty());
    assert!(matches!(
        echoes.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

#[test]
fn identities_and_dedup_survive_reopen_and_replay_eviction() {
    let temp = tempfile::tempdir().unwrap();
    let a_path = temp.path().join("a.sqlite");
    let b_path = temp.path().join("b.sqlite");
    let a = ChannelStore::open(&a_path).unwrap();
    let b = ChannelStore::open_sqlite_with_limits(
        &b_path,
        SqliteReplayLimits {
            max_messages: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let mut events = a.publications();
    a.append("events", &payload(1)).unwrap();
    let first = next_event(&mut events);
    b.ingest_remote(&first).unwrap();
    a.publish_ephemeral("live", &payload(2)).unwrap();
    let ephemeral = next_event(&mut events);
    b.ingest_remote(&ephemeral).unwrap();
    drop(a);
    drop(b);
    let a = ChannelStore::open(&a_path).unwrap();
    let b = ChannelStore::open(&b_path).unwrap();
    let mut events = a.publications();
    a.append("events", &payload(3)).unwrap();
    let third = next_event(&mut events);
    assert_eq!(third.id.origin, first.id.origin);
    assert_eq!(third.id.sequence, first.id.sequence + 2);
    assert_eq!(b.ingest_remote(&third).unwrap(), RemoteIngest::Applied);
    assert_eq!(b.ingest_remote(&first).unwrap(), RemoteIngest::Duplicate);
    assert_eq!(
        b.ingest_remote(&ephemeral).unwrap(),
        RemoteIngest::Duplicate
    );
    let messages = b.read_after("events", None, 10).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].event_id.as_ref(), Some(&third.id));
    assert_eq!(
        b.raw_query_i64("SELECT COUNT(*) FROM channel_message_events", []),
        1
    );
}

#[test]
fn transport_gaps_are_out_of_band_and_scoped_resets_do_not_publish() {
    let store = ChannelStore::open_in_memory().unwrap();
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
    let event = ChannelEvent {
        id: ChannelEventId {
            origin: "remote".into(),
            sequence: 2,
        },
        channel: "a".into(),
        payload: payload(2),
        ephemeral: false,
    };
    assert_eq!(store.ingest_remote(&event).unwrap(), RemoteIngest::Applied);
    assert!(a.has_changed().unwrap());
    assert!(b.has_changed().unwrap());
    a.borrow_and_update();
    b.borrow_and_update();
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
    assert_eq!(store.read_after("a", None, 100).unwrap().len(), 1);
    assert!(store.read_after("b", None, 100).unwrap().is_empty());
    assert!(events.try_recv().is_err());
}

#[test]
fn concurrent_handles_publish_in_persisted_sequence_order() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("a.sqlite");
    let a = ChannelStore::open(&path).unwrap();
    let b = ChannelStore::open(&path).unwrap();
    let reader = ChannelStore::open(&path).unwrap();
    let mut events = reader.publications();
    let threads: Vec<_> = [a, b]
        .into_iter()
        .map(|store| {
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
    for sequence in 1..=40 {
        assert_eq!(next_event(&mut events).id.sequence, sequence);
    }
    assert_eq!(reader.read_after("events", None, 100).unwrap().len(), 20);
}

#[test]
fn failed_publish_or_ingest_does_not_advance_identity_or_dedup() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChannelStore::open_sqlite_with_limits(
        &temp.path().join("a.sqlite"),
        SqliteReplayLimits {
            max_bytes: 20,
            max_bytes_per_stream: 20,
            ..Default::default()
        },
    )
    .unwrap();
    let mut events = store.publications();
    let oversized = ChannelPublishPayload {
        r#type: "event".into(),
        data: serde_json::json!("x".repeat(100)),
    };
    assert!(store.append("a", &oversized).is_err());
    store.append("a", &payload(1)).unwrap();
    assert_eq!(next_event(&mut events).id.sequence, 1);
    let mut event = ChannelEvent {
        id: ChannelEventId {
            origin: "remote".into(),
            sequence: 2,
        },
        channel: "a".into(),
        payload: oversized,
        ephemeral: false,
    };
    let resets = store.resets("a");
    assert!(store.ingest_remote(&event).is_err());
    assert!(!resets.has_changed().unwrap());
    assert_eq!(
        store.raw_query_i64("SELECT COUNT(*) FROM channel_remote_origins", []),
        0
    );
    event.payload = payload(2);
    assert_eq!(store.ingest_remote(&event).unwrap(), RemoteIngest::Applied);
    assert!(resets.has_changed().unwrap());
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn reset_ends_ephemeral_subscription_before_queued_business_events() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("a.sqlite");
    let writer = ChannelStore::open(&path).unwrap();
    let reader = ChannelStore::open(&path).unwrap();
    let mut reset = reader.subscribe_ephemeral("reset").unwrap();
    let mut other = reader.subscribe_ephemeral("other").unwrap();
    writer.publish_ephemeral("reset", &payload(1)).unwrap();
    writer.publish_ephemeral("other", &payload(2)).unwrap();
    writer.signal_gap(Some("reset"));
    assert!(reset.recv().await.is_none());
    assert!(reset.recv().await.is_none());
    assert_eq!(other.recv().await.unwrap().data, 2);
    assert_eq!(
        reader.raw_query_i64("SELECT COUNT(*) FROM channel_messages", []),
        0
    );
}

#[cfg(unix)]
#[tokio::test]
async fn async_appends_and_symlinked_handles_share_the_publication_queue() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("a.sqlite");
    let writer = std::sync::Arc::new(ChannelStore::open(&path).unwrap());
    let alias = temp.path().join("alias.sqlite");
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    let reader = ChannelStore::open(&alias).unwrap();
    let changes = reader.changes("a");
    let mut events = reader.publications();
    writer.append_async("a", &payload(1)).await.unwrap();
    assert!(changes.has_changed().unwrap());
    assert_eq!(next_event(&mut events).payload, payload(1));
}

#[test]
fn lagged_publication_queue_is_bounded_and_can_reset_connected_streams() {
    let store = ChannelStore::open_in_memory().unwrap();
    let mut events = store.publications();
    let resets = store.resets("a");
    for n in 0..300 {
        store.publish_ephemeral("a", &payload(n)).unwrap();
    }
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(_))
    ));
    store.signal_gap(None);
    assert!(resets.has_changed().unwrap());
    assert_eq!(
        store.raw_query_i64("SELECT COUNT(*) FROM channel_messages", []),
        0
    );
    assert_eq!(
        store.raw_query_i64("SELECT COUNT(*) FROM channel_metadata", []),
        0
    );
}

#[test]
fn postgres_mesh_uses_wake_hints_without_reinserting_remote_events() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let app = format!("mesh-wake-test/{}", std::process::id());
    let writer = ChannelStore::open_postgres(&url, &app).unwrap();
    let reader = ChannelStore::open_postgres(&url, &app).unwrap();
    let mut events = writer.publications();
    let mut echoes = reader.publications();
    let after = reader.replay_cursor("a", None).unwrap();
    let changes = reader.changes("a");
    let message = writer.append("a", &payload(1)).unwrap();
    assert_eq!(
        events.try_recv().unwrap(),
        ChannelPublication::Wake {
            channel: "a".into()
        }
    );
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

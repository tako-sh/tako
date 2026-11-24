use super::*;
use crate::test_db::TestDb;

#[test]
fn store_config_debug_redacts_binding_credentials() {
    let token = "private-binding-token";
    let debug = format!(
        "{:?}",
        ChannelStoreConfig::hosted("http://db/?key=secret", token, "app/prod")
    );
    assert!(debug.contains("app/prod"));
    assert!(!debug.contains("secret"));
    assert!(!debug.contains(token));
}

#[tokio::test]
async fn hosted_reset_resets_live_subscribers_whose_heads_move_back() {
    let db = TestDb::start();
    let (writer, reader) = (db.open().await, db.open_reader().await);
    let mut changes = reader.changes("room");
    let mut gap = reader.resets("room");
    let payload = ChannelPublishPayload {
        r#type: "event".into(),
        data: 1.into(),
    };
    let publish = |count: usize| {
        let payload = payload.clone();
        writer.run(move |writer| {
            for _ in 0..count {
                writer.append("room", &payload)?;
            }
            Ok(())
        })
    };
    async fn wait_for(changes: &mut tokio::sync::watch::Receiver<Option<i64>>, head: i64) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while *changes.borrow_and_update() != Some(head) {
                changes.changed().await.unwrap();
            }
        })
        .await
        .expect("reader did not observe the head");
    }
    publish(3).await.unwrap();
    wait_for(&mut changes, 3).await;
    gap.borrow_and_update();
    // `tako db reset` replaces the file under both open handles.
    db.execute(
        "DROP VIEW tako_replay_bounds; DROP TABLE tako_replay_messages;
         DROP TABLE tako_replay_streams; DROP TABLE tako_replay_apps;",
    );
    publish(1).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), gap.changed())
        .await
        .expect("reader did not signal a gap after the reset")
        .unwrap();
    // Position 1 of the new epoch is below the head seen before the reset.
    wait_for(&mut changes, 1).await;
}

#[tokio::test]
async fn live_delivery_and_presence_never_touch_the_database() {
    let db = TestDb::start();
    let (writer, reader) = (db.open().await, db.open_reader().await);
    let mut live = reader.subscribe_ephemeral("room").unwrap();
    let mut events = writer.ephemeral_publications();
    let payload = ChannelPublishPayload {
        r#type: "event".into(),
        data: 1.into(),
    };
    let message = writer.publish_ephemeral("room", &payload).unwrap();
    let mut event = events.try_recv().unwrap();
    assert!(event.ephemeral);
    event.ephemeral = false;
    assert!(writer.ingest_remote(&event).is_err());
    reader.notify_committed(&message, true);
    let received = tokio::time::timeout(std::time::Duration::from_secs(1), live.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*received, message);
    let member = crate::PresenceMember {
        connection_id: "conn".into(),
        subject: Some("alice".into()),
        data: serde_json::Value::Null,
    };
    writer.update_presence("room", &member, 0).unwrap();
    assert_eq!(
        writer.presence("room", 0).unwrap(),
        std::slice::from_ref(&member)
    );
    // Membership belongs to the handle that serves the connections.
    assert!(reader.update_presence("room", &member, 0).is_err());
    assert!(reader.presence("room", 0).unwrap().is_empty());
    assert!(
        reader
            .read_after_async("room", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_streams"), 0);
}

#[tokio::test]
async fn hosted_reader_recovers_a_missed_wake_and_keeps_its_cursor_across_mesh_loss() {
    let db = TestDb::start();
    let (writer, reader) = (db.open().await, db.open_reader().await);
    let mut changes = reader.changes("room");
    let head = reader
        .run(move |reader| {
            let mut publications = writer.publications();
            let gap = writer.resets("room");
            let after = reader.replay_cursor("room", None)?;
            let closed = writer.close("room")?;
            assert_eq!(
                publications.try_recv().unwrap(),
                ChannelPublication::Wake {
                    channel: "room".into()
                }
            );
            assert_eq!(writer.close("room")?, closed);
            assert!(publications.try_recv().is_err());
            writer.invalidate_replay()?;
            assert!(gap.has_changed().unwrap());
            let cursor = closed.id.parse::<ReplayCursor>().unwrap();
            assert_eq!(reader.replay_cursor("room", Some(cursor))?, Some(cursor));
            assert_eq!(reader.read_after("room", after, 10)?, [closed]);
            assert!(reader.close("room").is_err());
            Ok(cursor.position)
        })
        .await
        .unwrap();
    // The reader's worker must recover without receiving the mesh Wake.
    tokio::time::timeout(std::time::Duration::from_secs(5), changes.changed())
        .await
        .expect("reader did not poll a remote commit")
        .unwrap();
    assert_eq!(*changes.borrow(), Some(head));
}

#[test]
fn fanout_state_is_released_with_the_last_subscriber_of_a_stream() {
    let db = TestDb::start();
    let _writer = db.writer();
    let reader = db.reader();
    let streams: Vec<crate::ChannelStream> = (0..100)
        .map(|n| crate::ChannelStream::new("chat", &serde_json::json!({ "room": n })))
        .collect();
    let held = (reader.changes("kept"), reader.resets("kept"));
    for stream in &streams {
        let _subscriber = (
            reader.changes(stream.key()),
            reader.resets(stream.key()),
            reader.subscribe_ephemeral(stream.key()).unwrap(),
        );
    }
    // The next subscription releases every stream nobody listens to.
    let _next = (reader.changes("next"), reader.resets("next"));
    assert_eq!(reader.delivery.changes.lock().len(), 2);
    assert_eq!(reader.delivery.resets.lock().len(), 2);
    drop(held);
}

#[test]
fn delayed_poll_or_publisher_cannot_regress_the_committed_head() {
    let db = TestDb::start();
    let store = db.without_maintenance();
    let mut receiver = store.changes("events");
    store.delivery.wake("events", Some(20));
    assert_eq!(*receiver.borrow_and_update(), Some(20));
    store.delivery.wake("events", Some(10));
    store.delivery.wake("events", Some(20));
    assert!(!receiver.has_changed().unwrap());
    assert_eq!(*receiver.borrow(), Some(20));
    store.delivery.wake("events", Some(21));
    assert!(receiver.has_changed().unwrap());
    for _ in 0..2 {
        assert_eq!(*receiver.borrow_and_update(), Some(21));
        store.wake("events");
        assert!(receiver.has_changed().unwrap());
    }
}

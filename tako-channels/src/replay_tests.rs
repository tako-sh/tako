use crate::test_db::TestDb;
use crate::*;

fn payload(value: &str) -> ChannelPublishPayload {
    ChannelPublishPayload {
        r#type: "event".into(),
        data: serde_json::json!(value),
    }
}

fn cursor(message: &ChannelMessage) -> ReplayCursor {
    message.id.parse().unwrap()
}

#[test]
fn replay_cursor_fences_app_stream_epoch_and_future() {
    let db = TestDb::start();
    let a = db.writer_for("a");
    let b = db.writer_for("b");
    let start = a.replay_cursor("events", None).unwrap();
    let message = a.append("events", &payload("one")).unwrap();
    b.append("events", &payload("two")).unwrap();
    assert_eq!(
        a.read_after("events", start, 10).unwrap(),
        vec![message.clone()]
    );
    for (store, stream, value) in [
        (&b, "events", cursor(&message)),
        (&a, "other", cursor(&message)),
        (&a, "events", ReplayCursor::unscoped(1)),
        (
            &a,
            "events",
            ReplayCursor {
                position: 100,
                ..cursor(&message)
            },
        ),
        (
            &a,
            "events",
            ReplayCursor::scoped([0; 16], "events", cursor(&message).position),
        ),
    ] {
        assert!(matches!(
            store.replay_cursor(stream, Some(value)),
            Err(ChannelError::StaleCursor)
        ));
        assert!(matches!(
            store.read_after(stream, Some(value), 10),
            Err(ChannelError::StaleCursor)
        ));
    }
    assert_eq!(
        message.id.parse::<ReplayCursor>().unwrap().to_string(),
        message.id
    );
    drop(a);
    let reopened = db.writer_for("a");
    assert_eq!(
        reopened
            .replay_cursor("events", Some(cursor(&message)))
            .unwrap(),
        Some(cursor(&message))
    );
}

#[test]
fn pruning_between_pages_detects_a_gap_and_keeps_exact_stream_floor() {
    let db = TestDb::start();
    let store = db.without_maintenance();
    let start = store.replay_cursor("a", None).unwrap();
    let first = store.append("a", &payload("one")).unwrap();
    store.append("b", &payload("interleaved")).unwrap();
    let second = store.append("a", &payload("two")).unwrap();
    assert_eq!(
        store.read_after("a", start, 1).unwrap(),
        vec![first.clone()]
    );
    let third = store.append("a", &payload("three")).unwrap();
    // Deleting the already-consumed row does not lose an unseen event.
    db.execute(&format!(
        "DELETE FROM tako_replay_messages WHERE id = {}",
        cursor(&first).position
    ));
    assert_eq!(
        store.read_after("a", Some(cursor(&first)), 1).unwrap(),
        vec![second.clone()]
    );
    db.execute(&format!(
        "DELETE FROM tako_replay_messages WHERE id = {}",
        cursor(&second).position
    ));
    assert!(matches!(
        store.read_after("a", Some(cursor(&first)), 1),
        Err(ChannelError::StaleCursor)
    ));
    assert!(matches!(
        store.replay_cursor("a", start),
        Err(ChannelError::StaleCursor)
    ));
    assert_eq!(store.read_after("a", None, 100).unwrap(), vec![third]);
    assert_eq!(store.read_after("b", None, 100).unwrap().len(), 1);
}

#[test]
fn window_expiry_hides_history_but_keeps_the_stream_head() {
    let db = TestDb::start();
    let store = db.without_maintenance();
    store
        .sync_channel(
            "a",
            &serde_json::from_value(serde_json::json!({"ok":true,"replayWindowMs":100})).unwrap(),
        )
        .unwrap();
    let start = store.replay_cursor("a", None).unwrap();
    let message = store.append("a", &payload("old")).unwrap();
    db.age(1_000);
    assert!(matches!(
        store.read_after("a", start, 10),
        Err(ChannelError::StaleCursor)
    ));
    assert!(store.read_after("a", None, 10).unwrap().is_empty());
    assert_eq!(
        store.replay_cursor("a", None).unwrap(),
        Some(cursor(&message))
    );
    store.prune();
    assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_messages"), 0);
    assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_streams"), 1);
    assert_eq!(
        store.replay_cursor("a", Some(cursor(&message))).unwrap(),
        Some(cursor(&message))
    );
}

#[test]
fn replay_parser_rejects_malformed_inputs_without_losing_the_cursor() {
    let db = TestDb::start();
    let store = db.writer();
    let message = store.append("a", &payload("one")).unwrap();
    let cursor = cursor(&message);
    assert_eq!(
        parse_message_id_cursor(Some(&message.id), "lastMessageId").unwrap(),
        Some(cursor)
    );
    assert_eq!(
        parse_ws_last_message_id(Some(&format!("last_message_id={}", message.id))).unwrap(),
        Some(cursor)
    );
    let frame = serde_json::json!({"type":"tako.auth", "lastMessageId":message.id}).to_string();
    assert_eq!(
        crate::websocket::parse_first_frame(&frame)
            .unwrap()
            .last_message_id,
        Some(cursor)
    );
    for invalid in ["s1.invalid", "s1...1", "-1", "9223372036854775808", "bogus"] {
        assert!(parse_message_id_cursor(Some(invalid), "lastMessageId").is_err());
        let frame = serde_json::json!({"type":"tako.auth", "lastMessageId":invalid}).to_string();
        assert!(crate::websocket::parse_first_frame(&frame).is_err());
    }
}

#[test]
fn floor_tracks_deleted_stream_positions_despite_gaps_in_global_row_ids() {
    let db = TestDb::start();
    let store = db.writer();
    let first = store.append("a", &payload("one")).unwrap();
    for _ in 0..3 {
        store.append("b", &payload("interleaved")).unwrap();
    }
    let next = store.append("a", &payload("two")).unwrap();
    db.execute(&format!(
        "DELETE FROM tako_replay_messages WHERE channel = 'a' AND id = {}",
        cursor(&first).position
    ));
    assert_eq!(
        store.read_after("a", Some(cursor(&first)), 10).unwrap(),
        vec![next]
    );
}

#[test]
fn budget_counters_match_retained_rows_across_pruning_and_reopen() {
    let db = TestDb::start();
    let store = db.without_maintenance();
    let size = "SUM(length(CAST(channel AS BLOB)) + length(CAST(type AS BLOB)) + length(CAST(data_json AS BLOB)))";
    let counted = || {
        let app = |column: &str| db.query_i64(&format!("SELECT {column} FROM tako_replay_apps"));
        let count = app("message_count");
        assert_eq!(
            count,
            db.query_i64("SELECT COUNT(*) FROM tako_replay_messages")
        );
        assert_eq!(
            app("message_bytes"),
            db.query_i64(&format!(
                "SELECT COALESCE({size}, 0) FROM tako_replay_messages"
            ))
        );
        for channel in ["a", "b"] {
            let stream = |column: &str| {
                db.query_i64(&format!(
                    "SELECT {column} FROM tako_replay_streams WHERE channel = '{channel}'"
                ))
            };
            assert_eq!(
                stream("message_count"),
                db.query_i64(&format!(
                    "SELECT COUNT(*) FROM tako_replay_messages WHERE channel = '{channel}'"
                ))
            );
            assert_eq!(
                stream("message_bytes"),
                db.query_i64(&format!(
                    "SELECT COALESCE({size}, 0) FROM tako_replay_messages WHERE channel = '{channel}'"
                ))
            );
        }
        count
    };
    store
        .sync_channel(
            "b",
            &serde_json::from_value(serde_json::json!({"ok":true,"replayWindowMs":60_000}))
                .unwrap(),
        )
        .unwrap();
    for n in 0..5 {
        store.append("a", &payload(&n.to_string())).unwrap();
        store.append("b", &payload(&"🦀".repeat(n))).unwrap();
        counted();
    }
    let other = db.writer();
    other.append("a", &payload("other")).unwrap();
    assert_eq!(counted(), 11);
    drop((store, other));
    let reopened = db.without_maintenance();
    db.execute("UPDATE tako_replay_messages SET created_ms = 0 WHERE channel = 'b'");
    assert!(reopened.read_after("b", None, 10).unwrap().is_empty());
    reopened.prune();
    assert_eq!(counted(), 6);
    // A later append deletes its own stream's expired rows in its transaction.
    reopened.append("b", &payload("fresh")).unwrap();
    db.execute("UPDATE tako_replay_messages SET created_ms = 0 WHERE channel = 'b'");
    reopened.append("b", &payload("fresher")).unwrap();
    assert_eq!(counted(), 7);
}

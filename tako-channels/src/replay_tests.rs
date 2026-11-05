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

fn limits() -> SqliteReplayLimits {
    SqliteReplayLimits {
        max_messages_per_stream: 2,
        max_bytes_per_stream: 1024,
        max_messages: 3,
        max_bytes: 4096,
        max_age_ms: 0,
    }
}

fn unaged(store: &ChannelStore, channel: &str) {
    store
        .sync_channel(
            channel,
            &serde_json::from_value(serde_json::json!({"ok":true,"replayWindowMs":0})).unwrap(),
        )
        .unwrap();
}

#[test]
fn replay_cursor_fences_database_stream_epoch_and_future() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("a.sqlite");
    let a = ChannelStore::open(&path).unwrap();
    let b = ChannelStore::open(&temp.path().join("b.sqlite")).unwrap();
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
        (&a, "events", ReplayCursor::postgres(1)),
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
            ReplayCursor {
                scope: Some(([0; 16], cursor(&message).scope.unwrap().1)),
                ..cursor(&message)
            },
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
    let reopened = ChannelStore::open(&path).unwrap();
    assert_eq!(
        reopened
            .replay_cursor("events", Some(cursor(&message)))
            .unwrap(),
        Some(cursor(&message))
    );
}

#[test]
fn pruning_between_pages_detects_a_gap_and_keeps_exact_stream_floor() {
    let temp = tempfile::tempdir().unwrap();
    let store =
        ChannelStore::open_sqlite_with_limits(&temp.path().join("a.sqlite"), limits()).unwrap();
    unaged(&store, "a");
    let start = store.replay_cursor("a", None).unwrap();
    let first = store.append("a", &payload("one")).unwrap();
    store.append("b", &payload("interleaved")).unwrap();
    let second = store.append("a", &payload("two")).unwrap();
    assert_eq!(
        store.read_after("a", start, 1).unwrap(),
        vec![first.clone()]
    );
    store.append("a", &payload("three")).unwrap();
    // Deleting the already-consumed row does not lose an unseen event.
    assert_eq!(
        store.read_after("a", Some(cursor(&first)), 1).unwrap(),
        vec![second]
    );
    store.append("a", &payload("four")).unwrap();
    assert!(matches!(
        store.read_after("a", Some(cursor(&first)), 1),
        Err(ChannelError::StaleCursor)
    ));
    assert!(matches!(
        store.replay_cursor("a", start),
        Err(ChannelError::StaleCursor)
    ));
    assert_eq!(store.read_after("a", None, 100).unwrap().len(), 2);
    assert!(store.raw_query_i64("SELECT COUNT(*) FROM channel_messages", []) <= 3);
}

#[test]
fn byte_limits_are_utf8_hard_bounds_shared_by_reopened_handles() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("a.sqlite");
    let one_size = "a".len()
        + "event".len()
        + serde_json::to_string(&serde_json::json!("🦀"))
            .unwrap()
            .len();
    let limits = SqliteReplayLimits {
        max_bytes_per_stream: (one_size * 2) as u64,
        max_bytes: (one_size * 3) as u64,
        max_messages_per_stream: 100,
        max_messages: 100,
        max_age_ms: 0,
    };
    let writer = ChannelStore::open_sqlite_with_limits(&path, limits).unwrap();
    let other = ChannelStore::open(&path).unwrap();
    for stream in ["a", "b"] {
        unaged(&writer, stream);
    }
    for _ in 0..4 {
        other.append("a", &payload("🦀")).unwrap();
    }
    for _ in 0..2 {
        other.append("b", &payload("🦀")).unwrap();
    }
    assert_eq!(writer.read_after("a", None, 100).unwrap().len(), 1);
    assert_eq!(writer.raw_query_i64("SELECT SUM(length(CAST(channel AS BLOB)) + length(CAST(type AS BLOB)) + length(CAST(data_json AS BLOB))) FROM channel_messages", []), (3 * one_size) as i64);
    let head = writer.replay_cursor("a", None).unwrap();
    assert!(matches!(
        other.append("a", &payload(&"x".repeat(100))),
        Err(ChannelError::BadRequest(_))
    ));
    assert_eq!(writer.replay_cursor("a", None).unwrap(), head);
    let invalid = SqliteReplayLimits {
        max_messages: 0,
        ..limits
    };
    assert!(ChannelStore::open_sqlite_with_limits(&path, invalid).is_err());
}

#[test]
fn age_and_inactivity_pruning_preserve_metadata_and_presence() {
    let temp = tempfile::tempdir().unwrap();
    let store = ChannelStore::open_sqlite_with_limits(
        &temp.path().join("a.sqlite"),
        SqliteReplayLimits {
            max_age_ms: 100,
            ..limits()
        },
    )
    .unwrap();
    unaged(&store, "a");
    let start = store.replay_cursor("a", None).unwrap();
    let message = store.append("a", &payload("old")).unwrap();
    store.raw_execute("UPDATE channel_messages SET created_at_unix_ms = 0", []);
    store.raw_execute(
        "INSERT INTO presence VALUES ('a','connection','{}',9223372036854775807)",
        [],
    );
    assert!(matches!(
        store.read_after("a", start, 10),
        Err(ChannelError::StaleCursor)
    ));
    assert!(store.read_after("a", None, 10).unwrap().is_empty());
    assert_eq!(
        store.replay_cursor("a", None).unwrap(),
        Some(cursor(&message))
    );
    assert_eq!(
        store.raw_query_i64("SELECT COUNT(*) FROM channel_metadata", []),
        1
    );
    assert_eq!(store.raw_query_i64("SELECT COUNT(*) FROM presence", []), 1);
}

#[test]
fn sqlite_replay_parser_rejects_malformed_inputs_without_losing_the_cursor() {
    let store = ChannelStore::open_in_memory().unwrap();
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
    let store = ChannelStore::open_in_memory().unwrap();
    let first = store.append("a", &payload("one")).unwrap();
    for _ in 0..3 {
        store.append("b", &payload("interleaved")).unwrap();
    }
    let next = store.append("a", &payload("two")).unwrap();
    store.raw_execute(
        "DELETE FROM channel_messages WHERE channel = 'a' AND id = ?1",
        [cursor(&first).position],
    );
    assert_eq!(
        store.read_after("a", Some(cursor(&first)), 10).unwrap(),
        vec![next]
    );
}

#[test]
fn postgres_replay_bounds_keep_numeric_ids_and_reject_future_or_expired_positions() {
    assert_eq!(
        crate::store::resolve_replay_cursor(None, Some(5), Some(10)).unwrap(),
        Some(10)
    );
    assert_eq!(
        crate::store::resolve_replay_cursor(Some(4), Some(5), Some(10)).unwrap(),
        Some(4)
    );
    assert_eq!(
        crate::store::resolve_replay_cursor(Some(10), None, Some(10)).unwrap(),
        Some(10)
    );
    for requested in [3, 11] {
        assert!(matches!(
            crate::store::resolve_replay_cursor(Some(requested), Some(5), Some(10)),
            Err(ChannelError::StaleCursor)
        ));
    }
    assert_eq!("42".parse::<ReplayCursor>().unwrap().to_string(), "42");
}

#[test]
fn tightening_limits_prunes_before_open_returns_and_preserves_cursor_epoch() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("a.sqlite");
    let first = ChannelStore::open(&path).unwrap();
    let start = first.replay_cursor("a", None).unwrap();
    for _ in 0..4 {
        first.append("a", &payload("one")).unwrap();
    }
    let head = first.replay_cursor("a", None).unwrap();
    let configured = ChannelStore::open_sqlite_with_limits(&path, limits()).unwrap();
    assert_eq!(
        first.raw_query_i64("SELECT COUNT(*) FROM channel_messages", []),
        2
    );
    assert_eq!(configured.replay_cursor("a", head).unwrap(), head);
    assert!(matches!(
        first.read_after("a", start, 10),
        Err(ChannelError::StaleCursor)
    ));
    first.append("a", &payload("two")).unwrap();
    assert_eq!(
        configured.raw_query_i64("SELECT COUNT(*) FROM channel_messages", []),
        2
    );
}

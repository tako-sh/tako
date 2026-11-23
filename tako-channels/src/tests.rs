use super::*;
use crate::test_db::TestDb;

fn event(data: serde_json::Value) -> ChannelPublishPayload {
    ChannelPublishPayload {
        r#type: "event".into(),
        data,
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn replay_expires_without_subscribers_or_further_publishes() {
    let db = TestDb::start();
    let store = db.writer();
    let auth: ChannelAuthResponse = serde_json::from_value(serde_json::json!({
        "ok": true, "replayWindowMs": 20
    }))
    .unwrap();
    store.sync_channel("idle", &auth).unwrap();
    let first = store.append("idle", &event(1.into())).unwrap();
    let last = store.append("idle", &event(2.into())).unwrap();
    db.age(1_000);
    assert!(store.read_after("idle", None, 100).unwrap().is_empty());
    assert!(matches!(
        store.replay_cursor("idle", first.id.parse().ok()),
        Err(ChannelError::StaleCursor)
    ));
    assert_eq!(
        store.replay_cursor("idle", last.id.parse().ok()).unwrap(),
        last.id.parse().ok()
    );
    assert_eq!(
        store.replay_cursor("idle", None).unwrap(),
        last.id.parse().ok()
    );
    assert!(matches!(
        store.replay_cursor("unknown", Some(ReplayCursor::unscoped(1))),
        Err(ChannelError::StaleCursor)
    ));
}

#[test]
fn parse_channel_route_rejects_invalid_paths() {
    for path in [
        "/_tako/channels/",
        "/_tako/channels/chat",
        "/_tako/channels/sse",
        "/_tako/channels/sse/",
        "/_tako/channels/ws/",
        "/_tako/channels/chat/messages",
        "/_tako/channels/sse/chat/messages",
        "/_tako/channels/ws/chat/messages",
        // Stream keys are internal storage names, never routes.
        "/_tako/channels/sse/tako.stream%3Aabc",
        "/_tako/channels/ws/tako.stream%3Aabc",
    ] {
        assert!(
            matches!(parse_channel_route(path), Err(ChannelError::InvalidPath)),
            "{path}"
        );
    }
    assert!(parse_channel_route("/channels/chat").unwrap().is_none());
    assert!(parse_channel_route(crate::shared::PATH).unwrap().is_none());
}

#[test]
fn parse_channel_route_accepts_exact_channel_names() {
    for (transport, operation) in [
        ("sse", ChannelOperation::Subscribe),
        ("ws", ChannelOperation::Connect),
    ] {
        for name in ["chat", "ws", "sse"] {
            let route = parse_channel_route(&format!("/_tako/channels/{transport}/{name}"))
                .unwrap()
                .unwrap();
            assert_eq!(route.channel, name);
            assert_eq!(route.operation, operation);
        }
    }
}

#[test]
fn parse_channel_route_decodes_percent_encoded_segment() {
    let route = parse_channel_route("/_tako/channels/ws/chat%3Aroom-123")
        .unwrap()
        .unwrap();
    assert_eq!(route.channel, "chat:room-123");
}

#[test]
fn parse_ws_last_message_id_reads_cursor() {
    let after = parse_ws_last_message_id(Some("last_message_id=42&noop=1")).unwrap();
    assert_eq!(after, Some(ReplayCursor::unscoped(42)));
}

#[test]
fn header_value_splits_on_first_space() {
    assert_eq!(
        ChannelHeaderValue::parse("Bearer abc 123"),
        ChannelHeaderValue {
            scheme: Some("Bearer".to_string()),
            value: "abc 123".to_string(),
        },
    );
    assert_eq!(
        ChannelHeaderValue::parse("plain-token"),
        ChannelHeaderValue {
            scheme: None,
            value: "plain-token".to_string(),
        },
    );
}

#[test]
fn verify_request_serializes_with_optional_credentials() {
    let req = ChannelAuthVerifyRequest {
        channel: "chat".to_string(),
        operation: "subscribe".to_string(),
        params: serde_json::json!({ "roomId": "room-9" }),
        header: Some(ChannelHeaderValue {
            scheme: Some("Bearer".to_string()),
            value: "abc123".to_string(),
        }),
        cookie: None,
    };

    let value = serde_json::to_value(&req).unwrap();
    assert_eq!(value["channel"], "chat");
    assert_eq!(value["params"]["roomId"], "room-9");
    assert_eq!(value["header"]["scheme"], "Bearer");
    assert!(value.get("cookie").is_none());
}

#[test]
fn registry_path_constant_exposed() {
    assert_eq!(INTERNAL_CHANNEL_REGISTRY_PATH, "/channels/registry");
}

#[test]
fn auth_scheme_serializes_false_for_public() {
    let public = ChannelAuthScheme::Public;
    assert_eq!(
        serde_json::to_value(&public).unwrap(),
        serde_json::json!(false)
    );

    let header_only = ChannelAuthScheme::Required {
        header_name: Some("authorization".into()),
        cookie_name: None,
    };
    let value = serde_json::to_value(&header_only).unwrap();
    assert_eq!(value["headerName"], "authorization");
    assert!(value.get("cookieName").is_none());
}

#[test]
fn auth_scheme_deserializes_false_or_object() {
    assert_eq!(
        serde_json::from_value::<ChannelAuthScheme>(serde_json::json!(false)).unwrap(),
        ChannelAuthScheme::Public,
    );
    assert_eq!(
        serde_json::from_value::<ChannelAuthScheme>(serde_json::json!({
            "headerName": "authorization",
            "cookieName": "sid"
        }))
        .unwrap(),
        ChannelAuthScheme::Required {
            header_name: Some("authorization".into()),
            cookie_name: Some("sid".into()),
        },
    );
}

#[test]
fn channel_def_meta_serializes_params_schema_inline() {
    let meta = ChannelDefinitionMeta {
        lifecycle: Default::default(),
        channel: "chat".into(),
        params_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "roomId": { "type": "string" }
            }
        }),
        auth: ChannelAuthScheme::Required {
            header_name: Some("authorization".into()),
            cookie_name: None,
        },
        transport: Some(ChannelTransport::Ws),
    };

    let value = serde_json::to_value(&meta).unwrap();
    assert_eq!(value["channel"], "chat");
    assert_eq!(value["paramsSchema"]["type"], "object");
    assert_eq!(value["transport"], "ws");
}

#[test]
fn channel_store_appends_and_reads_messages() {
    let db = TestDb::start();
    let store = db.writer();
    let first = store
        .append("chat:room-123", &event(serde_json::json!({ "text": "hi" })))
        .unwrap();
    let second = store
        .append(
            "chat:room-123",
            &event(serde_json::json!({ "text": "there" })),
        )
        .unwrap();

    assert_eq!(first.id.parse::<ReplayCursor>().unwrap().position, 1);
    assert_eq!(second.id.parse::<ReplayCursor>().unwrap().position, 2);

    let messages = store
        .read_after("chat:room-123", first.id.parse().ok(), 100)
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, second.id);
    assert_eq!(messages[0].data, serde_json::json!({ "text": "there" }));
}

/// Writers on several servers share one ordered history, and its retention.
#[test]
fn concurrent_publishers_share_changes_order_and_retention() {
    let db = TestDb::start();
    let writers: Vec<_> = (0..2).map(|_| db.writer()).collect();
    let reader = db.reader();
    let mut changes = reader.changes("events");
    let initial = reader.replay_cursor("events", None).unwrap();
    let handles: Vec<_> = writers
        .into_iter()
        .enumerate()
        .map(|(writer, store)| {
            std::thread::spawn(move || {
                for index in 0..100 {
                    store
                        .append("events", &event(serde_json::json!([writer, index])))
                        .unwrap();
                }
                store
            })
        })
        .collect();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut after = initial;
    let mut received = Vec::new();
    while received.len() < 200 {
        assert!(
            std::time::Instant::now() < deadline,
            "concurrent publisher replay lost messages"
        );
        for message in reader.read_after("events", after, 25).unwrap() {
            let id = message.id.parse::<ReplayCursor>().unwrap();
            assert!(after.is_none_or(|previous| id.position > previous.position));
            after = Some(id);
            received.push(message.data);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let writer = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .next()
        .unwrap();
    wait_until("the reader was not notified", || {
        changes.has_changed().unwrap()
    });
    changes.borrow_and_update();
    drop(changes);
    let auth: ChannelAuthResponse =
        serde_json::from_value(serde_json::json!({ "ok": true, "replayWindowMs": 20 })).unwrap();
    writer.sync_channel("expires", &auth).unwrap();
    let expired = writer.append("expires", &event(1.into())).unwrap();
    let latest = writer.append("expires", &event(2.into())).unwrap();
    db.age(1_000);
    assert!(reader.read_after("expires", None, 100).unwrap().is_empty());
    assert!(matches!(
        reader.replay_cursor("expires", expired.id.parse().ok()),
        Err(ChannelError::StaleCursor)
    ));
    assert_eq!(
        reader
            .replay_cursor("expires", latest.id.parse().ok())
            .unwrap(),
        latest.id.parse().ok()
    );
}

#[test]
fn channel_store_append_updates_channel_activity() {
    let db = TestDb::start();
    let store = db.writer();
    store
        .sync_channel(
            "chat:room-123",
            &ChannelAuthResponse {
                ok: true,
                subject: None,
                transport: None,
                ephemeral: false,
                replay_window_ms: DEFAULT_REPLAY_WINDOW_MS,
                inactivity_ttl_ms: 0,
                keepalive_interval_ms: DEFAULT_KEEPALIVE_INTERVAL_MS,
                max_connection_lifetime_ms: DEFAULT_MAX_CONNECTION_LIFETIME_MS,
            },
        )
        .unwrap();
    let activity = "SELECT activity_ms FROM tako_replay_streams WHERE channel = 'chat:room-123'";
    db.age(1_000);
    let before = db.query_i64(activity);
    store
        .append("chat:room-123", &event(serde_json::json!({ "text": "hi" })))
        .unwrap();
    let persisted: String = db
        .sql()
        .query_row(
            "SELECT data_json FROM tako_replay_messages WHERE channel = 'chat:room-123'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(db.query_i64(activity) >= before + 1_000);
    assert_eq!(persisted, r#"{"text":"hi"}"#);
}

#[test]
fn channel_store_defaults_missing_cursor_to_latest_message() {
    let db = TestDb::start();
    let store = db.writer();
    store
        .append("chat:room-123", &event(serde_json::json!({ "text": "hi" })))
        .unwrap();
    store
        .append(
            "chat:room-123",
            &event(serde_json::json!({ "text": "there" })),
        )
        .unwrap();
    assert_eq!(
        store
            .replay_cursor("chat:room-123", None)
            .unwrap()
            .map(|cursor| cursor.position),
        Some(2)
    );
}

#[test]
fn channel_store_rejects_stale_cursors() {
    let db = TestDb::start();
    let store = db.writer();
    let first = store
        .append("chat:room-123", &event(serde_json::json!({ "text": "hi" })))
        .unwrap();
    store
        .append(
            "chat:room-123",
            &event(serde_json::json!({ "text": "there" })),
        )
        .unwrap();
    let start = ReplayCursor {
        position: 0,
        ..first.id.parse::<ReplayCursor>().unwrap()
    };
    assert_eq!(
        store.replay_cursor("chat:room-123", Some(start)).unwrap(),
        Some(start)
    );
    db.execute("DELETE FROM tako_replay_messages WHERE id = 1");
    assert!(matches!(
        store.replay_cursor("chat:room-123", Some(start)),
        Err(ChannelError::StaleCursor)
    ));
    assert!(matches!(
        store.replay_cursor("chat:room-123", Some(ReplayCursor::unscoped(1))),
        Err(ChannelError::StaleCursor)
    ));
}

#[test]
fn channel_store_persists_lifecycle_and_expires_inactive_channels() {
    let db = TestDb::start();
    let store = db.writer();
    let auth = |inactivity_ttl_ms, transport| ChannelAuthResponse {
        ok: true,
        subject: None,
        transport,
        ephemeral: false,
        replay_window_ms: DEFAULT_REPLAY_WINDOW_MS,
        inactivity_ttl_ms,
        keepalive_interval_ms: DEFAULT_KEEPALIVE_INTERVAL_MS,
        max_connection_lifetime_ms: DEFAULT_MAX_CONNECTION_LIFETIME_MS,
    };
    store.sync_channel("chat:room-123", &auth(1, None)).unwrap();
    let payload = event(1.into());
    store.append("chat:room-123", &payload).unwrap();
    db.age(1_200);
    store
        .sync_channel("chat:room-456", &auth(0, Some(ChannelTransport::Ws)))
        .unwrap();
    store.append("chat:room-456", &payload).unwrap();
    assert!(
        store
            .read_after("chat:room-123", None, 100)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.read_after("chat:room-456", None, 100).unwrap().len(),
        1
    );
    // Publishing again uses the known lifecycle even after inactivity eviction.
    store.append("chat:room-123", &payload).unwrap();
    db.age(1_200);
    assert!(
        store
            .read_after("chat:room-123", None, 100)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.read_after("chat:room-456", None, 100).unwrap().len(),
        1
    );
}

#[test]
fn subscribers_observe_external_writers_and_recover_batched_replay() {
    let db = TestDb::start();
    let writer = db.writer();
    let reader = db.reader();
    let mut subscribers: Vec<_> = (0..32).map(|_| reader.changes("chat")).collect();
    for index in 0..150 {
        writer.append("chat", &event(index.into())).unwrap();
    }
    wait_until("subscribers did not observe external publishes", || {
        subscribers
            .iter()
            .all(|subscriber| subscriber.has_changed().unwrap())
    });
    for subscriber in &mut subscribers {
        subscriber.borrow_and_update();
    }
    let first = reader.read_after("chat", None, 100).unwrap();
    let second = reader
        .read_after("chat", first.last().unwrap().id.parse().ok(), 100)
        .unwrap();
    assert_eq!(first.len(), 100);
    assert_eq!(second.len(), 50);
    assert_eq!(first[0].data, serde_json::json!(0));
    assert_eq!(second[49].data, serde_json::json!(149));
}

#[test]
fn committed_local_publications_notify_before_returning() {
    let db = TestDb::start();
    let store = db.writer();
    let mut active = store.changes("active");
    let quiet = store.changes("quiet");
    for value in 0..20 {
        let message = store.append("active", &event(value.into())).unwrap();
        assert!(active.has_changed().unwrap());
        assert_eq!(
            *active.borrow_and_update(),
            Some(message.id.parse::<ReplayCursor>().unwrap().position)
        );
        assert!(!quiet.has_changed().unwrap());
    }
}

#[test]
fn failed_append_neither_notifies_nor_leaves_replay() {
    let db = TestDb::start();
    let store = db.writer();
    let changes = store.changes("events");
    let mut publications = store.publications();
    db.execute(
        "CREATE TRIGGER reject_publish BEFORE INSERT ON tako_replay_messages
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    );
    assert!(store.append("events", &event(1.into())).is_err());
    assert!(!changes.has_changed().unwrap());
    assert!(publications.try_recv().is_err());
    assert!(store.read_after("events", None, 100).unwrap().is_empty());
}

#[test]
fn external_publications_only_wake_subscribers_to_the_changed_stream() {
    let db = TestDb::start();
    let writer = db.writer();
    let reader = db.reader();
    let mut active = reader.changes("active");
    let mut quiet = reader.changes("quiet");
    let payload = event(serde_json::json!({}));
    writer.append("active", &payload).unwrap();
    wait_until("active stream was not notified", || {
        active.has_changed().unwrap()
    });
    assert!(
        !quiet.has_changed().unwrap(),
        "unrelated stream was notified"
    );
    active.borrow_and_update();
    writer.append("quiet", &payload).unwrap();
    wait_until("quiet stream's publication was missed", || {
        quiet.has_changed().unwrap()
    });
    quiet.borrow_and_update();
    assert!(
        !active.has_changed().unwrap(),
        "unchanged stream was notified again"
    );
}

#[test]
fn channel_store_reopen_preserves_existing_messages() {
    // A restarted server opens the same history again.
    let db = TestDb::start();
    db.writer()
        .append("chat:room-123", &event(serde_json::json!({ "text": "hi" })))
        .unwrap();
    let reopened = db.writer();
    let messages = reopened.read_after("chat:room-123", None, 100).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].data, serde_json::json!({ "text": "hi" }));
}

use super::*;

#[test]
fn replay_expires_without_subscribers_or_further_publishes() {
    let store = ChannelStore::open_in_memory().unwrap();
    let auth: ChannelAuthResponse = serde_json::from_value(serde_json::json!({
        "ok": true, "replayWindowMs": 20
    }))
    .unwrap();
    store.sync_channel("idle", &auth).unwrap();
    let first = store
        .append(
            "idle",
            &ChannelPublishPayload {
                r#type: "event".into(),
                data: serde_json::json!(1),
            },
        )
        .unwrap();
    let last = store
        .append(
            "idle",
            &ChannelPublishPayload {
                r#type: "event".into(),
                data: serde_json::json!(2),
            },
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1200));
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
        store.replay_cursor("unknown", Some(1)),
        Err(ChannelError::StaleCursor)
    ));
}

#[test]
fn parse_channel_route_rejects_invalid_paths() {
    assert!(matches!(
        parse_channel_route("/_tako/channels/"),
        Err(ChannelError::InvalidPath)
    ));
    assert!(matches!(
        parse_channel_route("/_tako/channels/chat/messages"),
        Err(ChannelError::InvalidPath)
    ));
    assert!(parse_channel_route("/channels/chat").unwrap().is_none());
}

#[test]
fn parse_channel_route_accepts_exact_channel_names() {
    let route = parse_channel_route("/_tako/channels/chat")
        .unwrap()
        .unwrap();
    assert_eq!(route.channel, "chat");
}

#[test]
fn parse_channel_route_decodes_percent_encoded_segment() {
    let route = parse_channel_route("/_tako/channels/chat%3Aroom-123")
        .unwrap()
        .unwrap();
    assert_eq!(route.channel, "chat:room-123");
}

#[test]
fn parse_ws_last_message_id_reads_cursor() {
    let after = parse_ws_last_message_id(Some("last_message_id=42&noop=1")).unwrap();
    assert_eq!(after, Some(42));
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
    let temp = tempfile::TempDir::new().unwrap();
    let store = ChannelStore::open(&temp.path().join("channels.sqlite")).unwrap();

    let first = store
        .append(
            "chat:room-123",
            &ChannelPublishPayload {
                r#type: "message".to_string(),
                data: serde_json::json!({ "text": "hi" }),
            },
        )
        .unwrap();
    let second = store
        .append(
            "chat:room-123",
            &ChannelPublishPayload {
                r#type: "message".to_string(),
                data: serde_json::json!({ "text": "there" }),
            },
        )
        .unwrap();

    assert_eq!(first.id, "1");
    assert_eq!(second.id, "2");

    let messages = store.read_after("chat:room-123", Some(1), 100).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, "2");
    assert_eq!(messages[0].data, serde_json::json!({ "text": "there" }));
}

#[test]
fn channel_store_config_names_postgres_schema() {
    assert_eq!(POSTGRES_CHANNELS_SCHEMA, "tako_channels");
    assert_eq!(
        ChannelStoreConfig::postgres("postgres://example", "chat-app/production").clone(),
        ChannelStoreConfig::Postgres {
            url: "postgres://example".to_string(),
            schema: "tako_channels".to_string(),
            app_id: "chat-app/production".to_string(),
        },
    );
}

#[test]
fn postgres_channel_store_round_trips_when_url_is_set() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let app_id = format!(
        "channel-test/{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let store = ChannelStore::open_postgres(&url, &app_id).unwrap();

    store
        .sync_channel(
            "chat",
            &ChannelAuthResponse {
                ok: true,
                subject: None,
                transport: None,
                replay_window_ms: DEFAULT_REPLAY_WINDOW_MS,
                inactivity_ttl_ms: 0,
                keepalive_interval_ms: DEFAULT_KEEPALIVE_INTERVAL_MS,
                max_connection_lifetime_ms: DEFAULT_MAX_CONNECTION_LIFETIME_MS,
            },
        )
        .unwrap();
    let first = store
        .append(
            "chat",
            &ChannelPublishPayload {
                r#type: "message".to_string(),
                data: serde_json::json!({ "text": "hello" }),
            },
        )
        .unwrap();
    let second = store
        .append(
            "chat",
            &ChannelPublishPayload {
                r#type: "message".to_string(),
                data: serde_json::json!({ "text": "there" }),
            },
        )
        .unwrap();

    let messages = store
        .read_after("chat", first.id.parse::<i64>().ok(), 100)
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, second.id);
    assert_eq!(messages[0].data, serde_json::json!({ "text": "there" }));
}

#[test]
fn postgres_concurrent_publishers_share_changes_order_and_retention() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let app = format!("concurrent-channel-test/{}", std::process::id());
    let reader = ChannelStore::open_postgres(&url, &app).unwrap();
    let writers: Vec<_> = (0..2)
        .map(|_| ChannelStore::open_postgres(&url, &app).unwrap())
        .collect();
    let mut changes = reader.changes("events");
    let initial = reader.replay_cursor("events", None).unwrap();
    let handles: Vec<_> = writers
        .into_iter()
        .enumerate()
        .map(|(writer, store)| {
            std::thread::spawn(move || {
                for index in 0..100 {
                    store
                        .append(
                            "events",
                            &ChannelPublishPayload {
                                r#type: "event".into(),
                                data: serde_json::json!([writer, index]),
                            },
                        )
                        .unwrap();
                }
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
            let id = message.id.parse::<i64>().unwrap();
            assert!(after.is_none_or(|previous| id > previous));
            after = Some(id);
            received.push(message.data);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    while !changes.has_changed().unwrap() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    changes.borrow_and_update();
    drop(changes);
    let auth: ChannelAuthResponse =
        serde_json::from_value(serde_json::json!({ "ok": true, "replayWindowMs": 20 })).unwrap();
    reader.sync_channel("expires", &auth).unwrap();
    let expired = reader
        .append(
            "expires",
            &ChannelPublishPayload {
                r#type: "event".into(),
                data: serde_json::json!(1),
            },
        )
        .unwrap();
    let latest = reader
        .append(
            "expires",
            &ChannelPublishPayload {
                r#type: "event".into(),
                data: serde_json::json!(2),
            },
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1200));
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
    let temp = tempfile::TempDir::new().unwrap();
    let store = ChannelStore::open(&temp.path().join("channels.sqlite")).unwrap();

    store
        .sync_channel(
            "chat:room-123",
            &ChannelAuthResponse {
                ok: true,
                subject: None,
                transport: None,
                replay_window_ms: DEFAULT_REPLAY_WINDOW_MS,
                inactivity_ttl_ms: 0,
                keepalive_interval_ms: DEFAULT_KEEPALIVE_INTERVAL_MS,
                max_connection_lifetime_ms: DEFAULT_MAX_CONNECTION_LIFETIME_MS,
            },
        )
        .unwrap();
    let before = store.raw_query_i64(
        "SELECT last_activity_unix_ms FROM channel_metadata WHERE channel = ?1",
        ("chat:room-123",),
    );

    std::thread::sleep(std::time::Duration::from_millis(2));
    let message = store
        .append(
            "chat:room-123",
            &ChannelPublishPayload {
                r#type: "message".to_string(),
                data: serde_json::json!({ "text": "hi" }),
            },
        )
        .unwrap();

    let after = store.raw_query_i64(
        "SELECT last_activity_unix_ms FROM channel_metadata WHERE channel = ?1",
        ("chat:room-123",),
    );
    let persisted = store.raw_query_string(
        "SELECT data_json FROM channel_messages WHERE id = ?1",
        (message.id.parse::<i64>().unwrap(),),
    );

    assert!(after >= before);
    assert_eq!(persisted, r#"{"text":"hi"}"#);
}

#[test]
fn channel_store_defaults_missing_cursor_to_latest_message() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = ChannelStore::open(&temp.path().join("channels.sqlite")).unwrap();

    store
        .append(
            "chat:room-123",
            &ChannelPublishPayload {
                r#type: "message".to_string(),
                data: serde_json::json!({ "text": "hi" }),
            },
        )
        .unwrap();
    store
        .append(
            "chat:room-123",
            &ChannelPublishPayload {
                r#type: "message".to_string(),
                data: serde_json::json!({ "text": "there" }),
            },
        )
        .unwrap();

    assert_eq!(store.replay_cursor("chat:room-123", None).unwrap(), Some(2));
}

#[test]
fn channel_store_rejects_stale_cursors() {
    let temp = tempfile::TempDir::new().unwrap();
    let db_path = temp.path().join("channels.sqlite");
    let store = ChannelStore::open(&db_path).unwrap();

    store
        .append(
            "chat:room-123",
            &ChannelPublishPayload {
                r#type: "message".to_string(),
                data: serde_json::json!({ "text": "hi" }),
            },
        )
        .unwrap();
    store
        .append(
            "chat:room-123",
            &ChannelPublishPayload {
                r#type: "message".to_string(),
                data: serde_json::json!({ "text": "there" }),
            },
        )
        .unwrap();
    store.raw_execute("DELETE FROM channel_messages WHERE id = 1", ());

    assert!(matches!(
        store.replay_cursor("chat:room-123", Some(0)),
        Err(ChannelError::StaleCursor)
    ));
}

#[test]
fn channel_store_persists_lifecycle_and_prunes_inactive_channels() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = ChannelStore::open(&temp.path().join("channels.sqlite")).unwrap();

    store
        .sync_channel(
            "chat:room-123",
            &ChannelAuthResponse {
                ok: true,
                subject: None,
                transport: None,
                replay_window_ms: DEFAULT_REPLAY_WINDOW_MS,
                inactivity_ttl_ms: 1,
                keepalive_interval_ms: DEFAULT_KEEPALIVE_INTERVAL_MS,
                max_connection_lifetime_ms: DEFAULT_MAX_CONNECTION_LIFETIME_MS,
            },
        )
        .unwrap();
    let payload = ChannelPublishPayload {
        r#type: "event".into(),
        data: serde_json::json!(1),
    };
    store.append("chat:room-123", &payload).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1200));
    store
        .sync_channel(
            "chat:room-456",
            &ChannelAuthResponse {
                ok: true,
                subject: None,
                transport: Some(ChannelTransport::Ws),
                replay_window_ms: DEFAULT_REPLAY_WINDOW_MS,
                inactivity_ttl_ms: 0,
                keepalive_interval_ms: DEFAULT_KEEPALIVE_INTERVAL_MS,
                max_connection_lifetime_ms: DEFAULT_MAX_CONNECTION_LIFETIME_MS,
            },
        )
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
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(
        store
            .read_after("chat:room-123", None, 100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn subscribers_observe_external_writers_and_recover_batched_replay() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("channels.sqlite");
    let reader = ChannelStore::open(&path).unwrap();
    let writer = ChannelStore::open(&path).unwrap();
    let mut subscribers: Vec<_> = (0..32).map(|_| reader.changes("chat")).collect();
    for index in 0..150 {
        writer
            .append(
                "chat",
                &ChannelPublishPayload {
                    r#type: "event".into(),
                    data: serde_json::json!(index),
                },
            )
            .unwrap();
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while subscribers
        .iter()
        .any(|subscriber| !subscriber.has_changed().unwrap())
    {
        assert!(
            std::time::Instant::now() < deadline,
            "subscribers did not observe external publishes"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
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
fn external_publications_only_wake_subscribers_to_the_changed_stream() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("channels.sqlite");
    let reader = ChannelStore::open(&path).unwrap();
    let writer = ChannelStore::open(&path).unwrap();
    assert_scoped_notifications(reader, writer);
}

#[test]
fn postgres_publications_only_wake_subscribers_to_the_changed_stream() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let app = format!("scoped-notification-test/{}", std::process::id());
    let schema = format!("scoped_notifications_{}", std::process::id());
    assert_scoped_notifications(
        ChannelStore::open_postgres_with_schema(&url, &schema, &app).unwrap(),
        ChannelStore::open_postgres_with_schema(&url, &schema, &app).unwrap(),
    );
}

fn assert_scoped_notifications(reader: ChannelStore, writer: ChannelStore) {
    let mut active = reader.changes("active");
    let mut quiet = reader.changes("quiet");
    let payload = ChannelPublishPayload {
        r#type: "event".into(),
        data: serde_json::json!({}),
    };
    writer.append("active", &payload).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !active.has_changed().unwrap() {
        assert!(
            std::time::Instant::now() < deadline,
            "active stream was not notified"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !quiet.has_changed().unwrap(),
        "unrelated stream was notified"
    );
    active.borrow_and_update();
    writer.append("quiet", &payload).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !quiet.has_changed().unwrap() {
        assert!(
            std::time::Instant::now() < deadline,
            "quiet stream's publication was missed"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    quiet.borrow_and_update();
    assert!(
        !active.has_changed().unwrap(),
        "unchanged stream was notified again"
    );
}

#[test]
fn channel_store_reopen_preserves_existing_messages() {
    // Guards the invariant that data persists to disk: opening the same
    // path again (e.g. after a process restart) must see the prior rows.
    let temp = tempfile::TempDir::new().unwrap();
    let db_path = temp.path().join("channels.sqlite");

    {
        let store = ChannelStore::open(&db_path).unwrap();
        store
            .append(
                "chat:room-123",
                &ChannelPublishPayload {
                    r#type: "message".to_string(),
                    data: serde_json::json!({ "text": "hi" }),
                },
            )
            .unwrap();
    }

    let reopened = ChannelStore::open(&db_path).unwrap();
    let messages = reopened.read_after("chat:room-123", None, 100).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].data, serde_json::json!({ "text": "hi" }));
}

fn test_payload(text: &str) -> ChannelPublishPayload {
    ChannelPublishPayload {
        r#type: "message".to_string(),
        data: serde_json::json!({ "text": text }),
    }
}

fn test_auth() -> ChannelAuthResponse {
    ChannelAuthResponse {
        ok: true,
        subject: None,
        transport: None,
        replay_window_ms: DEFAULT_REPLAY_WINDOW_MS,
        inactivity_ttl_ms: 0,
        keepalive_interval_ms: DEFAULT_KEEPALIVE_INTERVAL_MS,
        max_connection_lifetime_ms: DEFAULT_MAX_CONNECTION_LIFETIME_MS,
    }
}

/// Child half of `channel_store_supports_two_processes_writing_interleaved`.
/// No-ops unless spawned by the parent test with the DB path in the env.
#[test]
fn multiprocess_child_writer() {
    let Ok(path) = std::env::var("TAKO_TEST_MULTIPROCESS_DB") else {
        return;
    };
    let store = ChannelStore::open(std::path::Path::new(&path)).unwrap();
    // The parent's 10 rows must be visible to this process.
    assert_eq!(store.read_after("mp", None, 100).unwrap().len(), 10);
    for i in 0..10 {
        store
            .append("mp", &test_payload(&format!("child-{i}")))
            .unwrap();
    }
}

/// During a zero-downtime reload, both server processes can write to the same
/// channel database. Every write must remain visible and persisted.
#[test]
fn channel_store_supports_two_processes_writing_interleaved() {
    let temp = tempfile::TempDir::new().unwrap();
    let db_path = temp.path().join("channels.sqlite");
    let store = ChannelStore::open(&db_path).unwrap();
    store.sync_channel("mp", &test_auth()).unwrap();
    for i in 0..10 {
        store
            .append("mp", &test_payload(&format!("parent-{i}")))
            .unwrap();
    }

    // Spawn a second process against the same DB while we hold it open.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["tests::multiprocess_child_writer", "--exact", "--nocapture"])
        .env("TAKO_TEST_MULTIPROCESS_DB", db_path.to_str().unwrap())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "child writer process failed: stdout={stdout}; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The child's writes must be visible here, and this process must still
    // be able to write after the child exits.
    assert_eq!(store.read_after("mp", None, 100).unwrap().len(), 20);
    store.append("mp", &test_payload("parent-after")).unwrap();
    assert_eq!(store.read_after("mp", None, 100).unwrap().len(), 21);
}

/// A write issued while another connection holds a live read cursor must
/// either persist or fail.
#[test]
fn write_with_live_read_cursor_persists() {
    let temp = tempfile::TempDir::new().unwrap();
    let db_path = temp.path().join("channels.sqlite");
    let store = ChannelStore::open(&db_path).unwrap();
    for i in 0..5 {
        store
            .append("cursor", &test_payload(&format!("m-{i}")))
            .unwrap();
    }

    let reader = tako_sqlite::open_local(&db_path).unwrap();
    let mut stmt = reader.prepare("SELECT id FROM channel_messages").unwrap();
    let mut rows = stmt.query([]).unwrap();
    let _first = rows.next().unwrap();
    let write_result = store.append("cursor", &test_payload("live"));
    drop(rows);
    drop(stmt);
    drop(reader);

    let count = store.raw_query_i64(
        "SELECT COUNT(*) FROM channel_messages WHERE channel = 'cursor'",
        [],
    );
    match write_result {
        Ok(_) => assert_eq!(count, 6, "write returned Ok but did not persist"),
        Err(_) => assert_eq!(count, 5, "write failed loudly; nothing should persist"),
    }
}

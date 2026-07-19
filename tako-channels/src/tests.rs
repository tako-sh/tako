use super::*;

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
    std::thread::sleep(std::time::Duration::from_millis(5));
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

    let channels =
        store.raw_query_strings("SELECT channel FROM channel_metadata ORDER BY channel ASC");

    assert_eq!(channels, vec!["chat:room-456".to_string()]);
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

/// Gating test for the turso migration: during a zero-downtime server
/// reload the old and new tako-server processes hold the same channel DB
/// simultaneously and both write. Every write from both processes must be
/// visible and persisted — this is the scenario that broke turso 0.6.1
/// (process-exclusive file lock) and must keep working.
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

/// Gating test for the turso migration: in turso 0.6.1 a write issued on a
/// connection holding an unconsumed read cursor returned Ok but silently
/// never persisted. Guard against that regressing in future turso upgrades:
/// the write must either persist or fail loudly.
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

    let write_result = {
        let conn = store.sqlite_conn();
        crate::store::block_on(async {
            // Open a cursor and consume only the first row, keeping it live.
            let mut rows = conn
                .query("SELECT id FROM channel_messages", ())
                .await
                .unwrap();
            let _first = rows.next().await.unwrap();
            conn.execute(
                "INSERT INTO channel_messages (channel, type, data_json) VALUES ('cursor', 'message', '{}')",
                (),
            )
            .await
        })
    };

    let count = store.raw_query_i64(
        "SELECT COUNT(*) FROM channel_messages WHERE channel = 'cursor'",
        (),
    );
    match write_result {
        Ok(_) => assert_eq!(count, 6, "write returned Ok but did not persist"),
        Err(_) => assert_eq!(count, 5, "write failed loudly; nothing should persist"),
    }
}

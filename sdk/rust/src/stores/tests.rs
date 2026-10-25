use super::*;

fn definition() -> StoreDefinition {
    StoreDefinition::new(|input| async move {
        Ok(json!({"params":input.params,"subject":input.subject}))
    })
}

fn request(name: &str, operation: &str) -> PresenceVerifyInput {
    PresenceVerifyInput {
        channel: format!("{STORE_CHANNEL_PREFIX}{name}"),
        operation: operation.into(),
        params: json!({"chatId":"chat-a"}),
        header: None,
        cookie: None,
    }
}

#[tokio::test]
async fn store_metadata_read_only_authorization_and_loader_identity() {
    let registry = StoreRegistry::new();
    let store = registry
        .register(
            "messages",
            definition().offline(60_000).offline_version("2"),
        )
        .unwrap();
    assert_eq!(store.name(), "messages");
    let metadata = registry.metadata();
    assert_eq!(metadata[0]["channel"], "tako.store:messages");
    assert_eq!(metadata[0]["auth"], false);
    assert!(metadata[0].get("transport").is_none());
    assert!(
        registry
            .authorize(request("messages", "subscribe"))
            .await
            .unwrap()
            .ok
    );
    for operation in ["connect", "publish", "unknown"] {
        assert!(
            !registry
                .authorize(request("messages", operation))
                .await
                .unwrap()
                .ok
        );
    }
    assert!(
        registry
            .authorize(request("missing", "subscribe"))
            .await
            .is_none()
    );
    let snapshot = registry
        .load(StoreLoadInput {
            store: "messages".into(),
            params: json!({"chatId":"chat-a"}),
            subject: Some("trusted-user".into()),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.data["subject"], "trusted-user");
    assert_eq!(snapshot.data["params"]["chatId"], "chat-a");
    let offline = snapshot.offline.unwrap();
    assert_eq!(offline.max_age_ms, 60_000);
    assert_eq!(offline.version, "2");
}

#[tokio::test]
async fn store_private_verifier_gets_public_name_and_header_or_cookie() {
    let registry = Arc::new(StoreRegistry::new());
    let verifier_registry = registry.clone();
    registry
        .register(
            "private",
            definition().auth(StoreAuth::default(), move |input| {
                // Registering within the callback proves the registry lock is not held while awaiting.
                let registry = verifier_registry.clone();
                async move {
                    registry.register("other", definition()).unwrap();
                    assert_eq!(input.store, "private");
                    (input.params["chatId"] == "chat-a"
                        && (input.cookie.as_deref() == Some("valid")
                            || input.header.is_some_and(|header| header.value == "valid")))
                    .then_some(StoreGrant::new("verified"))
                }
            }),
        )
        .unwrap();
    assert!(
        !registry
            .authorize(request("private", "subscribe"))
            .await
            .unwrap()
            .ok
    );
    let mut input = request("private", "subscribe");
    input.cookie = Some("valid".into());
    assert_eq!(
        registry
            .authorize(input.clone())
            .await
            .unwrap()
            .subject
            .as_deref(),
        Some("verified")
    );
    input.params["chatId"] = json!("forbidden");
    let denied = registry.authorize(input).await.unwrap();
    assert!(!denied.ok);
    assert!(denied.subject.is_none());
}

#[tokio::test]
async fn store_load_releases_registry_lock_and_rejects_oversized_results() {
    let registry = Arc::new(StoreRegistry::new());
    let loader_registry = registry.clone();
    registry
        .register(
            "first",
            StoreDefinition::new(move |_| {
                let registry = loader_registry.clone();
                async move {
                    registry.register("second", definition()).unwrap();
                    Ok(json!("x".repeat(MAX_STORE_BYTES)))
                }
            }),
        )
        .unwrap();
    let input = StoreLoadInput {
        store: "first".into(),
        params: json!({}),
        subject: None,
    };
    assert!(matches!(
        registry.load(input).await.unwrap(),
        Err(StoreError::TooLarge)
    ));
    assert!(
        registry
            .load(StoreLoadInput {
                store: "unknown".into(),
                params: json!({}),
                subject: None
            })
            .await
            .is_none()
    );
    assert!(matches!(
        registry
            .load(StoreLoadInput {
                store: "first".into(),
                params: Value::Null,
                subject: None
            })
            .await
            .unwrap(),
        Err(StoreError::InvalidParams)
    ));
}

#[test]
fn store_configuration_rejects_invalid_names_and_offline_policy() {
    let registry = StoreRegistry::new();
    for name in ["", "bad\nname", "a/b", "_leading", "é", "name with space"] {
        assert!(registry.register(name, definition()).is_err());
    }
    assert!(registry.register("valid", definition().offline(0)).is_err());
    assert!(
        registry
            .register("valid", definition().offline(10).offline_version(""))
            .is_err()
    );
    assert!(
        registry
            .register("valid", definition().params_schema(Value::Null))
            .is_err()
    );
    assert!(
        registry
            .register("valid", definition().max_connection_lifetime_ms(0))
            .is_err()
    );
    assert!(
        registry
            .register(
                "valid",
                definition().max_connection_lifetime_ms(9_007_199_254_740_992)
            )
            .is_err()
    );
    assert!(
        registry
            .register(
                "valid",
                definition().keepalive_interval_ms(9_007_199_254_740_992)
            )
            .is_err()
    );
    assert!(
        registry
            .register("a.B-1_2", definition().offline_version("2").offline(10))
            .is_ok()
    );
}

#[cfg(unix)]
fn socket_path() -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    format!(
        "/tmp/tako-store-rust-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    )
}

#[cfg(unix)]
#[test]
fn invalidation_sends_scoped_internal_command_and_reads_response() {
    use std::io::{BufRead, BufReader};
    let path = socket_path();
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let command: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            command,
            json!({"command":"channel_publish","app":"myapp","channel":"tako.store:messages","payload":{"type":"tako.store.invalidate","data":{},"scopeParams":{"chatId":"chat-a"}}})
        );
        stream
            .write_all(b"{\"status\":\"ok\",\"data\":{}}\n")
            .unwrap();
    });
    let handle = StoreHandle {
        name: "messages".into(),
    };
    handle
        .invalidate_at(
            &path,
            "myapp",
            json!({"chatId":"chat-a"}),
            Duration::from_secs(2),
        )
        .unwrap();
    server.join().unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(matches!(
        handle.command("app", Value::Null),
        Err(StoreError::InvalidParams)
    ));
    assert!(matches!(
        handle.command("app", json!({"id":"x".repeat(MAX_SCOPE_BYTES)})),
        Err(StoreError::ScopeTooLarge)
    ));
}

#[cfg(unix)]
#[test]
fn invalidation_rejects_malformed_denied_oversized_and_stalled_responses() {
    use std::io::{BufRead, BufReader};
    for (reply, expected) in [
        (
            b"{\"status\":\"error\",\"message\":\"secret\"}\n".to_vec(),
            "denied",
        ),
        (b"not json\n".to_vec(), "malformed"),
        (vec![b'x'; MAX_STORE_BYTES + 1], "large"),
        (Vec::new(), "closed"),
    ] {
        let path = socket_path();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let _ = stream.write_all(&reply);
        });
        let result = StoreHandle {
            name: "messages".into(),
        }
        .invalidate_at(&path, "app", json!({}), Duration::from_secs(2));
        assert!(match expected {
            "denied" => matches!(result, Err(StoreError::Rejected)),
            "large" => matches!(result, Err(StoreError::TooLarge)),
            _ => matches!(result, Err(StoreError::Protocol)),
        });
        server.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }
    let path = socket_path();
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_millis(100));
    });
    assert!(matches!(
        StoreHandle {
            name: "messages".into()
        }
        .invalidate_at(&path, "app", json!({}), Duration::from_millis(20)),
        Err(StoreError::Timeout)
    ));
    server.join().unwrap();
    std::fs::remove_file(path).unwrap();
}

#[cfg(all(feature = "tokio", unix))]
#[tokio::test]
async fn async_invalidation_does_not_block_the_runtime_and_obeys_deadline() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let path = socket_path();
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut line = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut line)
            .await
            .unwrap();
        let command: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(command["payload"]["scopeParams"]["chatId"], "chat-a");
        stream.write_all(b"{\"status\":\"ok\"}\n").await.unwrap();
    });
    StoreHandle {
        name: "messages".into(),
    }
    .invalidate_async_at(
        &path,
        "app",
        json!({"chatId":"chat-a"}),
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    server.await.unwrap();
    std::fs::remove_file(path).unwrap();
    let path = socket_path();
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    assert!(matches!(
        StoreHandle {
            name: "messages".into()
        }
        .invalidate_async_at(&path, "app", json!({}), Duration::from_millis(20))
        .await,
        Err(StoreError::Timeout)
    ));
    server.abort();
    std::fs::remove_file(path).unwrap();
}

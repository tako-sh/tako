use super::*;

fn host() -> (tempfile::TempDir, KvHost) {
    let dir = tempfile::tempdir().unwrap();
    let host = KvHost::new(HostConfig::default());
    (dir, host)
}

fn get(key: &str) -> KvRequest {
    KvRequest::Get { key: key.into() }
}

fn put(key: &str, json: &str) -> KvRequest {
    KvRequest::Put {
        key: key.into(),
        json: json.into(),
        ttl_ms: None,
    }
}

#[test]
fn apps_reach_their_own_bindings_only() {
    let (dir, host) = host();
    let sessions = StoreId::parse("sessions").unwrap();
    host.register(sessions.clone(), dir.path().join("sessions.redb"))
        .unwrap();
    host.set_bindings(
        "shop/production",
        vec![Binding {
            token_hash: hash_token("token"),
            store: sessions.clone(),
            home: Home::Local,
        }],
    );
    // A release command's temporary scope belongs to the same environment.
    host.set_bindings(
        "shop/production#release",
        vec![Binding {
            token_hash: hash_token("release-token"),
            store: sessions.clone(),
            home: Home::Local,
        }],
    );
    assert_eq!(
        host.call("shop/production", "token", put("k", r#"{"a":1}"#))
            .unwrap(),
        serde_json::Value::Null
    );
    assert_eq!(
        host.call("shop/production", "release-token", get("k"))
            .unwrap(),
        serde_json::json!(r#"{"a":1}"#)
    );
    // Another app cannot use a token it learned, and unknown tokens fail.
    for (app, token) in [("other/production", "token"), ("shop/production", "nope")] {
        let error = host.call(app, token, get("k")).unwrap_err();
        assert_eq!(error.code, Some(ErrorCode::Unavailable));
    }
    // Invalid requests are refused before anything runs.
    let error = host.call("shop/production", "token", put("k", "not json")).unwrap_err();
    assert_eq!(error.code, Some(ErrorCode::InvalidRequest));
    let error = host
        .call("shop/production", "token", get(&"k".repeat(1025)))
        .unwrap_err();
    assert_eq!(error.code, Some(ErrorCode::InvalidRequest));
    // Revoking the binding ends access.
    host.set_bindings("shop/production", Vec::new());
    assert!(host.call("shop/production", "token", get("k")).is_err());
    assert_eq!(host.admin(&sessions, get("k")).unwrap(), serde_json::json!(r#"{"a":1}"#));
}

#[test]
fn full_stores_report_their_own_code() {
    let dir = tempfile::tempdir().unwrap();
    let host = KvHost::new(HostConfig {
        max_store_bytes: 100,
        ..HostConfig::default()
    });
    let id = StoreId::parse("small").unwrap();
    host.register(id.clone(), dir.path().join("small.redb")).unwrap();
    let error = host
        .admin(&id, put("k", &format!("\"{}\"", "x".repeat(100))))
        .unwrap_err();
    assert_eq!(error.code, Some(ErrorCode::KvStoreFull));
    host.close_all();
    let error = host.admin(&id, get("k")).unwrap_err();
    assert_eq!(error.code, Some(ErrorCode::Unavailable));
}

#[tokio::test(flavor = "multi_thread")]
async fn other_servers_forward_with_their_grant() {
    let (dir, home) = host();
    let id = StoreId::parse("sessions").unwrap();
    home.register(id.clone(), dir.path().join("sessions.redb")).unwrap();
    home.set_remote_grants("shop/production", BTreeSet::from([id.clone()]), "grant-key");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    tokio::spawn(crate::serve_peers(
        home.clone(),
        listener,
        Arc::new(|ip: std::net::IpAddr| ip.is_loopback()),
    ));

    let edge = KvHost::new(HostConfig::default());
    let bind = |key: &str| {
        vec![Binding {
            token_hash: hash_token("edge-token"),
            store: id.clone(),
            home: Home::Remote {
                upstream,
                key: key.into(),
            },
        }]
    };
    edge.set_bindings("shop/production#release", bind("grant-key"));
    let call = {
        let edge = edge.clone();
        move |request: KvRequest| {
            let edge = edge.clone();
            async move {
                tokio::task::spawn_blocking(move || edge.call("shop/production", "edge-token", request))
                    .await
                    .unwrap()
            }
        }
    };
    call(put("challenge", r#""xyz""#)).await.unwrap();
    assert_eq!(call(get("challenge")).await.unwrap(), serde_json::json!(r#""xyz""#));
    // Reused connections keep working, and the data lives on the home server.
    for _ in 0..20 {
        call(get("challenge")).await.unwrap();
    }
    assert_eq!(home.admin(&id, get("challenge")).unwrap(), serde_json::json!(r#""xyz""#));
    call(KvRequest::Delete {
        key: "challenge".into(),
    })
    .await
    .unwrap();
    assert_eq!(call(get("challenge")).await.unwrap(), serde_json::Value::Null);

    // A wrong grant key, or a store outside the grant, is refused.
    edge.set_bindings("shop/production#release", bind("wrong"));
    let error = call(get("challenge")).await.unwrap_err();
    assert_eq!(error.message, REFUSED);
    // A server that cannot be reached reports so.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gone = closed.local_addr().unwrap();
    drop(closed);
    edge.set_bindings(
        "shop/production",
        vec![Binding {
            token_hash: hash_token("edge-token"),
            store: id.clone(),
            home: Home::Remote {
                upstream: gone,
                key: "grant-key".into(),
            },
        }],
    );
    let error = call(get("challenge")).await.unwrap_err();
    assert_eq!(error.code, Some(ErrorCode::Unavailable));
    assert!(error.message.contains("not reachable"), "{}", error.message);
}

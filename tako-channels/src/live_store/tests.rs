use super::*;
use crate::{ChannelAuthScheme, ChannelLifecycle};
use pingora_core::protocols::l4::stream::Stream;
use tokio::net::TcpListener;

fn query(params: Value) -> String {
    let mut url = reqwest::Url::parse("http://localhost").unwrap();
    url.query_pairs_mut()
        .append_pair("params", &params.to_string());
    url.query().unwrap().to_owned()
}

#[test]
fn store_query_preserves_exact_types_and_rejects_ambiguous_envelopes() {
    let schema = json!({"type":"object","properties":{
        "value":{"anyOf":[{"type":"string"},{"type":"number"}]},
        "filter":{"type":"object"}, "ids":{"type":"array","items":{"type":"integer"}},
        "cursor":{"type":"null"}
    },"additionalProperties":false});
    for params in [
        json!({"value":1}),
        json!({"value":"1"}),
        json!({"filter":{"unread":true},"ids":[1,2],"cursor":null}),
    ] {
        assert_eq!(
            validate_store_query(&schema, &query(params.clone())).unwrap(),
            params
        );
    }
    let numeric = validate_store_query(&schema, &query(json!({"value":1}))).unwrap();
    let text = validate_store_query(&schema, &query(json!({"value":"1"}))).unwrap();
    assert_ne!(
        store_scope("tako.store:messages", &numeric),
        store_scope("tako.store:messages", &text)
    );
    for bad in [
        "value=1",
        "params=not-json",
        "params=%5B%5D",
        "params=null",
        "params=%FF",
        "params=%7B%7D&params=%7B%7D",
        "params=%7B%7D&other=1",
        "params=%7B%7D&",
    ] {
        assert!(
            matches!(
                validate_store_query(&schema, bad),
                Err(ChannelError::BadRequest(_))
            ),
            "accepted {bad}"
        );
    }
    assert_eq!(validate_store_query(&schema, "").unwrap(), json!({}));
    assert!(validate_store_query(&schema, &query(json!({"ids":["wrong"]}))).is_err());
    assert!(
        validate_store_query(
            &schema,
            &query(json!({"value":"x".repeat(MAX_STORE_PARAMS_BYTES)}))
        )
        .is_err()
    );
    assert!(validate_store_query(&json!({"type":"object","required":["id"]}), "").is_err());
}

#[test]
fn store_stream_keys_are_scoped_canonical_bounded_and_numeric() {
    let name = "tako.store:messages";
    let first = store_scope(name, &json!({"room":"one", "limit": 1}));
    assert_eq!(
        first,
        store_scope(name, &json!({"limit":1.0, "room":"one"}))
    );
    assert_ne!(first, store_scope(name, &json!({"room":"two", "limit":1})));
    assert_ne!(
        first,
        store_scope("tako.store:other", &json!({"room":"one", "limit":1}))
    );
    assert_eq!(
        store_scope(name, &json!({"number":10_000_000_000_000_000_u64})),
        store_scope(name, &json!({"number":1e16}))
    );
    assert_eq!(
        store_scope(name, &json!({"number":-0.0})),
        store_scope(name, &json!({"number":0}))
    );
    assert_ne!(
        store_scope(name, &json!({"number":9_007_199_254_740_993_u64})),
        store_scope(name, &json!({"number":9_007_199_254_740_992_u64}))
    );
    assert_eq!(
        store_scope(name, &json!({"nested":{"z":1,"a":2}})),
        store_scope(name, &json!({"nested":{"a":2,"z":1}}))
    );
    assert_eq!(
        store_scope(name, &json!({"room":"x".repeat(20_000)})).len(),
        STORE_CHANNEL_PREFIX.len() + 64
    );
}

#[test]
fn store_publications_require_scope_and_only_invalidation_payload() {
    let payload = json!({"type":"tako.store.invalidate","data":{},"scopeParams":{"room":"one"}});
    assert_eq!(
        publication_channel("tako.store:messages", &payload).unwrap(),
        store_scope("tako.store:messages", &json!({"room":"one"}))
    );
    for bad in [
        json!({"type":"tako.store.invalidate","data":{}}),
        json!({"type":"tako.store.invalidate","data":{},"scopeParams":null}),
        json!({"type":"tako.store.invalidate","data":{},"scopeParams":[]}),
        json!({"type":"chat.send","data":{},"scopeParams":{}}),
        json!({"type":"tako.store.invalidate","data":{"secret":"value"},"scopeParams":{}}),
        json!({"type":"tako.store.invalidate","data":{},"scopeParams":{"room":"x".repeat(MAX_STORE_PARAMS_BYTES)}}),
    ] {
        assert!(
            publication_channel("tako.store:messages", &bad).is_err(),
            "accepted {bad}"
        );
    }
    assert_eq!(
        publication_channel("chat", &json!({"type":"custom","data":{"value":true}})).unwrap(),
        "chat"
    );
}

#[test]
fn public_store_routes_are_single_encoded_names() {
    assert_eq!(
        parse_store_route("/_tako/stores/chat-messages").unwrap(),
        Some("chat-messages".into())
    );
    assert_eq!(parse_store_route("/api/messages").unwrap(), None);
    assert!(parse_store_route("/_tako/stores/").is_err());
    assert!(parse_store_route("/_tako/stores/messages/extra").is_err());
    assert!(parse_store_route("/_tako/stores/%FF").is_err());
    assert!(parse_store_route("/_tako/stores/chat%20messages").is_err());
    for name in ["", ".hidden", "-name", "é", "chat:name", "a/b"] {
        assert!(!valid_store_name(name));
        assert!(
            publication_channel(
                &format!("{STORE_CHANNEL_PREFIX}{name}"),
                &json!({"type":"tako.store.invalidate","data":{},"scopeParams":{}})
            )
            .is_err()
        );
    }
    assert!(valid_store_name(&"a".repeat(256)));
    assert!(!valid_store_name(&"a".repeat(257)));
}

fn auth() -> ChannelAuthResponse {
    ChannelAuthResponse {
        ok: true,
        subject: Some("verified".into()),
        transport: None,
        replay_window_ms: 60_000,
        inactivity_ttl_ms: 0,
        keepalive_interval_ms: 25_000,
        max_connection_lifetime_ms: 60_000,
    }
}

#[test]
fn store_lifecycle_rejects_zero_and_unsafe_values_before_clock_arithmetic() {
    assert!(validate_store_lifecycle(&auth()).is_ok());
    for milliseconds in [0, MAX_SAFE_INTEGER + 1, u64::MAX] {
        let mut invalid = auth();
        invalid.keepalive_interval_ms = milliseconds;
        assert!(matches!(
            validate_store_lifecycle(&invalid),
            Err(ChannelError::AuthUnavailable)
        ));
        let mut invalid = auth();
        invalid.max_connection_lifetime_ms = milliseconds;
        assert!(matches!(
            validate_store_lifecycle(&invalid),
            Err(ChannelError::AuthUnavailable)
        ));
    }
}

async fn sse_server(store: Arc<ChannelStore>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let store = store.clone();
            tokio::spawn(async move {
                let mut session = Session::new_h1(Box::new(Stream::from(socket)));
                session.read_request().await.unwrap();
                let params = validate_store_query(
                    &json!({"type":"object","properties":{"last_message_id":{"type":"integer"}}}),
                    session.req_header().uri.query().unwrap_or_default(),
                )
                .unwrap();
                let _ = serve_store_events(
                    &mut session,
                    &store,
                    &store_scope("tako.store:messages", &params),
                    &auth(),
                )
                .await;
            });
        }
    });
    (format!("http://{address}"), task)
}

async fn next_frame(response: &mut reqwest::Response, buffered: &mut String) -> Value {
    loop {
        if let Some(end) = buffered.find("\n\n") {
            let frame = buffered[..end].to_owned();
            buffered.drain(..end + 2);
            if let Some(data) = frame.strip_prefix("data: ") {
                return serde_json::from_str(data).unwrap();
            }
        } else {
            let chunk = response.chunk().await.unwrap().expect("SSE stream closed");
            buffered.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    }
}

#[tokio::test]
async fn store_sse_ready_precedes_live_events_and_scopes_reconnects_across_instances() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("channels.sqlite");
    let writer = Arc::new(ChannelStore::open_sqlite(&path).unwrap());
    let reader = Arc::new(ChannelStore::open_sqlite(&path).unwrap());
    let first = store_scope(
        "tako.store:messages",
        &json!({"room":"one","last_message_id":42}),
    );
    let second = store_scope("tako.store:messages", &json!({"room":"two"}));
    for scope in [&first, &second] {
        writer.sync_channel_async(scope, &auth()).await.unwrap();
    }
    let payload = ChannelPublishPayload {
        r#type: "tako.store.invalidate".into(),
        data: json!({}),
    };
    writer.append_async(&first, &payload).await.unwrap();
    let (url, task) = sse_server(reader).await;
    let client = reqwest::Client::new();
    let mut response = client
        .get(format!(
            "{url}/?{}",
            query(json!({"room":"one","last_message_id":42}))
        ))
        .header("Last-Event-ID", "-999")
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let mut buffered = String::new();
    assert_eq!(
        next_frame(&mut response, &mut buffered).await,
        json!({"type":"tako.store.ready","data":{}})
    );
    writer.append_async(&second, &payload).await.unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(200),
            next_frame(&mut response, &mut buffered)
        )
        .await
        .is_err()
    );
    writer.append_async(&first, &payload).await.unwrap();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(2),
            next_frame(&mut response, &mut buffered)
        )
        .await
        .unwrap(),
        json!({"type":"tako.store.invalidate","data":{}})
    );
    drop(response);
    let mut response = client
        .get(format!(
            "{url}/?{}",
            query(json!({"room":"one","last_message_id":42}))
        ))
        .header("Last-Event-ID", "nonsense")
        .send()
        .await
        .unwrap();
    let mut buffered = String::new();
    assert_eq!(
        next_frame(&mut response, &mut buffered).await["type"],
        "tako.store.ready"
    );
    writer.append_async(&first, &payload).await.unwrap();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(2),
            next_frame(&mut response, &mut buffered)
        )
        .await
        .unwrap()["type"],
        "tako.store.invalidate"
    );
    task.abort();
}

async fn mock_app(status: u16, response_body: String) -> (String, tokio::task::JoinHandle<()>) {
    mock_app_with_echo(status, response_body, "secret-token").await
}

async fn mock_app_with_echo(
    status: u16,
    response_body: String,
    echo: &'static str,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let response_body = response_body.clone();
            tokio::spawn(async move {
                let mut session = Session::new_h1(Box::new(Stream::from(socket)));
                session.read_request().await.unwrap();
                assert_eq!(session.req_header().headers["host"], "app.tako");
                assert_eq!(
                    session.req_header().headers["x-tako-internal-token"],
                    "secret-token"
                );
                let mut body = Vec::new();
                while let Some(chunk) = session.read_request_body().await.unwrap() {
                    body.extend_from_slice(&chunk);
                }
                let path = session.req_header().uri.path();
                let input = if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice::<Value>(&body).unwrap()
                };
                let (status, data) = match path {
                    crate::INTERNAL_CHANNEL_REGISTRY_PATH => (200, serde_json::to_string(&vec![ChannelDefinitionMeta {
                        channel:"tako.store:messages".into(),
                        params_schema:json!({"type":"object","properties":{"room":{"type":"string"},"limit":{"type":"integer"}},"required":["room"]}),
                        auth:ChannelAuthScheme::Required{header_name:Some("authorization".into()),cookie_name:Some("session".into())},
                        transport:None, lifecycle:ChannelLifecycle::default(),
                    }]).unwrap()),
                    crate::INTERNAL_CHANNEL_AUTH_PATH => {
                        assert_eq!(input["operation"], "subscribe");
                        assert_eq!(input["params"], json!({"room":"one","limit":1}));
                        if input["header"]["value"] == "good" || input["cookie"] == "good" { (200, serde_json::to_string(&json!({"ok":true,"subject":"verified"})).unwrap()) }
                        else { (403, r#"{"error":"private detail"}"#.into()) }
                    },
                    INTERNAL_STORE_LOAD_PATH => {
                        assert_eq!(input["store"], "messages");
                        assert_eq!(input["subject"], "verified");
                        assert_eq!(input["params"], json!({"room":"one","limit":1}));
                        (status, response_body)
                    },
                    _ => panic!("Unexpected mock path {path}"),
                };
                let mut header = ResponseHeader::build(status, None).unwrap();
                header
                    .insert_header("Content-Length", data.len().to_string())
                    .unwrap();
                header
                    .insert_header("Content-Type", "application/json")
                    .unwrap();
                if !echo.is_empty() {
                    header.insert_header("x-tako-internal-token", echo).unwrap();
                }
                session
                    .write_response_header(Box::new(header), false)
                    .await
                    .unwrap();
                session
                    .write_response_body(Some(Bytes::from(data)), true)
                    .await
                    .unwrap();
            });
        }
    });
    (address.to_string(), task)
}

#[tokio::test]
async fn stores_validate_params_authorize_credentials_and_forward_only_verified_subject() {
    let (endpoint, task) = mock_app(
        200,
        json!({"data":[{"text":"hello"}],"offline":{"version":"v1","maxAgeMs":1000}}).to_string(),
    )
    .await;
    let headers = HashMap::from([("authorization".into(), "Bearer good".into())]);
    let (params, auth) = authorize_store(
        &endpoint,
        "app.tako",
        "secret-token",
        "tako.store:messages",
        &query(json!({"room":"one","limit":1})),
        &headers,
    )
    .await
    .unwrap();
    let data = load_store(
        &endpoint,
        "app.tako",
        "secret-token",
        "messages",
        params,
        auth.subject.as_deref(),
    )
    .await
    .unwrap();
    assert_eq!(data["data"][0]["text"], "hello");
    assert_eq!(data["offline"]["version"], "v1");
    assert!(matches!(
        authorize_store(
            &endpoint,
            "app.tako",
            "secret-token",
            "tako.store:messages",
            &query(json!({"room":"one","limit":"bad"})),
            &headers
        )
        .await,
        Err(ChannelError::BadRequest(_))
    ));
    assert!(matches!(
        authorize_store(
            &endpoint,
            "app.tako",
            "secret-token",
            "tako.store:messages",
            &query(json!({"room":"one","limit":1})),
            &HashMap::new()
        )
        .await,
        Err(ChannelError::Unauthorized)
    ));
    let headers = HashMap::from([("authorization".into(), "Bearer bad".into())]);
    assert!(matches!(
        authorize_store(
            &endpoint,
            "app.tako",
            "secret-token",
            "tako.store:messages",
            &query(json!({"room":"one","limit":1})),
            &headers
        )
        .await,
        Err(ChannelError::Forbidden)
    ));
    let headers = HashMap::from([("cookie".into(), "other=no; session=good".into())]);
    assert!(
        authorize_store(
            &endpoint,
            "app.tako",
            "secret-token",
            "tako.store:messages",
            &query(json!({"room":"one","limit":1})),
            &headers
        )
        .await
        .is_ok()
    );
    assert!(matches!(
        authorize_store(
            &endpoint,
            "app.tako",
            "secret-token",
            "tako.store:unknown",
            &query(json!({"room":"one","limit":1})),
            &headers
        )
        .await,
        Err(ChannelError::NotDefined)
    ));
    task.abort();
}

#[tokio::test]
async fn store_load_rejects_oversized_malformed_and_internal_errors() {
    for (status, body) in [
        (200, "{not-json".to_owned()),
        (200, json!({"other":true}).to_string()),
        (
            403,
            json!({"error":"private bootstrap credential"}).to_string(),
        ),
        (
            200,
            json!({"data":[],"offline":{"version":"v1","maxAgeMs":0}}).to_string(),
        ),
        (
            200,
            json!({"data":[],"offline":{"version":"v1","maxAgeMs":1.5}}).to_string(),
        ),
        (
            200,
            json!({"data":[],"offline":{"version":"é".repeat(65),"maxAgeMs":1000}}).to_string(),
        ),
        (
            200,
            json!({"data":"x".repeat(MAX_STORE_RESPONSE_BYTES)}).to_string(),
        ),
        (
            500,
            json!({"error":"private database connection secret"}).to_string(),
        ),
    ] {
        let (endpoint, task) = mock_app(status, body).await;
        let error = load_store(
            &endpoint,
            "app.tako",
            "secret-token",
            "messages",
            json!({"room":"one","limit":1}),
            Some("verified"),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ChannelError::AuthUnavailable));
        assert!(!error.to_string().contains("secret"));
        task.abort();
    }
}

#[tokio::test]
async fn store_internal_responses_require_matching_bootstrap_token() {
    for echo in ["", "wrong-token"] {
        let (endpoint, task) = mock_app_with_echo(200, json!({"data":[]}).to_string(), echo).await;
        assert!(matches!(
            load_store(
                &endpoint,
                "app.tako",
                "secret-token",
                "messages",
                json!({"room":"one","limit":1}),
                Some("verified")
            )
            .await,
            Err(ChannelError::AuthUnavailable)
        ));
        assert!(matches!(
            authorize_store(
                &endpoint,
                "app.tako",
                "secret-token",
                "tako.store:messages",
                &query(json!({"room":"one","limit":1})),
                &HashMap::new()
            )
            .await,
            Err(ChannelError::AuthUnavailable)
        ));
        task.abort();
    }
}

async fn publish_store_invalidation(store: &Arc<ChannelStore>, params: Value) {
    let wire_payload = json!({"type":"tako.store.invalidate","data":{},"scopeParams":params});
    let channel = publication_channel("tako.store:messages", &wire_payload).unwrap();
    let payload = serde_json::from_value::<ChannelPublishPayload>(wire_payload).unwrap();
    store.append_async(&channel, &payload).await.unwrap();
}

#[tokio::test]
async fn postgres_store_sse_shares_scoped_invalidations_across_instances_when_configured() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let app_id = format!(
        "store-test/{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    // postgres::Client owns a synchronous runtime, so open it on the same
    // blocking pool used by production Store connections.
    let (writer, reader_a, reader_b, other_app) = tokio::task::spawn_blocking(move || {
        let writer = Arc::new(ChannelStore::open_postgres(&url, &app_id).unwrap());
        let reader_a = Arc::new(ChannelStore::open_postgres(&url, &app_id).unwrap());
        let reader_b = Arc::new(ChannelStore::open_postgres(&url, &app_id).unwrap());
        let other_app =
            Arc::new(ChannelStore::open_postgres(&url, &format!("{app_id}-other")).unwrap());
        (writer, reader_a, reader_b, other_app)
    })
    .await
    .unwrap();
    let (url_a, server_a) = sse_server(reader_a).await;
    let (url_b, server_b) = sse_server(reader_b.clone()).await;
    let (url_other, server_other) = sse_server(other_app.clone()).await;
    let client = reqwest::Client::new();
    let one = query(json!({"room":"one"}));
    let two = query(json!({"room":"two"}));
    let mut first = client.get(format!("{url_a}/?{one}")).send().await.unwrap();
    let mut second = client.get(format!("{url_b}/?{one}")).send().await.unwrap();
    let mut other_scope = client.get(format!("{url_a}/?{two}")).send().await.unwrap();
    let mut other = client
        .get(format!("{url_other}/?{one}"))
        .send()
        .await
        .unwrap();
    let (mut first_buffer, mut second_buffer, mut scope_buffer, mut other_buffer) =
        (String::new(), String::new(), String::new(), String::new());
    for (response, buffer) in [
        (&mut first, &mut first_buffer),
        (&mut second, &mut second_buffer),
        (&mut other_scope, &mut scope_buffer),
        (&mut other, &mut other_buffer),
    ] {
        assert_eq!(
            next_frame(response, buffer).await["type"],
            "tako.store.ready"
        );
    }
    publish_store_invalidation(&writer, json!({"room":"one"})).await;
    let expected = json!({"type":"tako.store.invalidate","data":{}});
    for (response, buffer) in [
        (&mut first, &mut first_buffer),
        (&mut second, &mut second_buffer),
    ] {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), next_frame(response, buffer))
                .await
                .unwrap(),
            expected
        );
    }
    assert!(
        tokio::time::timeout(
            Duration::from_millis(150),
            next_frame(&mut other_scope, &mut scope_buffer)
        )
        .await
        .is_err()
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(150),
            next_frame(&mut other, &mut other_buffer)
        )
        .await
        .is_err()
    );
    publish_store_invalidation(&writer, json!({"room":"two"})).await;
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(3),
            next_frame(&mut other_scope, &mut scope_buffer)
        )
        .await
        .unwrap(),
        expected
    );
    // A second server can publish into the same scope using the same native
    // publication path, and both original connections observe the change.
    publish_store_invalidation(&reader_b, json!({"room":"one"})).await;
    for (response, buffer) in [
        (&mut first, &mut first_buffer),
        (&mut second, &mut second_buffer),
    ] {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), next_frame(response, buffer))
                .await
                .unwrap(),
            expected
        );
    }
    publish_store_invalidation(&other_app, json!({"room":"one"})).await;
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(3),
            next_frame(&mut other, &mut other_buffer)
        )
        .await
        .unwrap(),
        expected
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(150),
            next_frame(&mut first, &mut first_buffer)
        )
        .await
        .is_err()
    );
    server_a.abort();
    server_b.abort();
    server_other.abort();
}

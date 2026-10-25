use super::*;
use crate::{PresenceDefinition, StoreAuth, StoreDefinition, StoreError, StoreGrant};
use std::sync::atomic::{AtomicUsize, Ordering};

fn request(path: &str, method: &str, token: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(path)
        .method(method)
        .header("host", "app.tako");
    if let Some(token) = token {
        builder = builder.header(crate::INTERNAL_TOKEN_HEADER, token);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn json_body(response: Response) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_STORE_BYTES)
            .await
            .unwrap(),
    )
    .unwrap()
}

fn state() -> RealtimeState {
    RealtimeState {
        presence: Some(Arc::new(PresenceRegistry::new())),
        stores: Some(Arc::new(StoreRegistry::new())),
    }
}

#[tokio::test]
async fn unauthorized_requests_never_load_data_or_expose_metadata() {
    let state = state();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    state
        .stores
        .as_ref()
        .unwrap()
        .register(
            "messages",
            StoreDefinition::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Ok(json!("private data")) }
            }),
        )
        .unwrap();
    for path in [LOAD_PATH, REGISTRY_PATH, AUTHORIZE_PATH] {
        for token in [None, Some("wrong")] {
            let response = handle_realtime(
                request(path, "POST", token, json!({"store":"messages","params":{}})),
                &state,
                Some("secret"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(
                !response
                    .headers()
                    .contains_key(crate::INTERNAL_TOKEN_HEADER)
            );
            assert_eq!(json_body(response).await, json!({"error":"Forbidden"}));
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        handle_realtime(
            request(
                LOAD_PATH,
                "POST",
                None,
                json!({"store":"messages","params":{}})
            ),
            &state,
            None
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn loader_receives_trusted_params_and_subject_and_offline_policy() {
    let state = state();
    state
        .stores
        .as_ref()
        .unwrap()
        .register(
            "messages",
            StoreDefinition::new(|input| async move {
                assert_eq!(input.store, "messages");
                assert_eq!(input.params, json!({"chatId":"allowed","count":2}));
                assert_eq!(input.subject.as_deref(), Some("verified-user"));
                Ok(json!([{"id":"m1","text":"hi"}]))
            })
            .offline(60_000),
        )
        .unwrap();
    let response=handle_realtime(request(LOAD_PATH,"POST",Some("secret"),json!({"store":"messages","params":{"chatId":"allowed","count":2},"subject":"verified-user"})),&state,Some("secret")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()[crate::INTERNAL_TOKEN_HEADER], "secret");
    assert_eq!(
        json_body(response).await,
        json!({"data":[{"id":"m1","text":"hi"}],"offline":{"maxAgeMs":60_000,"version":"1"}})
    );
}

#[tokio::test]
async fn combined_registry_and_authorization_support_presence_and_stores() {
    let state = state();
    state
        .presence
        .as_ref()
        .unwrap()
        .register("chat", PresenceDefinition::new());
    state
        .stores
        .as_ref()
        .unwrap()
        .register(
            "messages",
            StoreDefinition::new(|_| async { Ok(Value::Null) }).auth(
                StoreAuth::default(),
                |input| async move {
                    assert_eq!(input.store, "messages");
                    (input.cookie.as_deref() == Some("valid")).then_some(StoreGrant::new("user"))
                },
            ),
        )
        .unwrap();
    let response = handle_realtime(
        request(REGISTRY_PATH, "GET", Some("secret"), Value::Null),
        &state,
        Some("secret"),
    )
    .await;
    let metadata = json_body(response).await;
    assert_eq!(metadata[0]["channel"], "tako.presence:chat");
    assert_eq!(metadata[1]["channel"], "tako.store:messages");
    for (name, operation, cookie, expected) in [
        ("tako.presence:chat", "connect", Value::Null, StatusCode::OK),
        (
            "tako.store:messages",
            "subscribe",
            json!("valid"),
            StatusCode::OK,
        ),
        (
            "tako.store:messages",
            "subscribe",
            json!("invalid"),
            StatusCode::FORBIDDEN,
        ),
        (
            "tako.store:messages",
            "publish",
            json!("valid"),
            StatusCode::FORBIDDEN,
        ),
    ] {
        let response = handle_realtime(
            request(
                AUTHORIZE_PATH,
                "POST",
                Some("secret"),
                json!({"channel":name,"operation":operation,"params":{},"cookie":cookie}),
            ),
            &state,
            Some("secret"),
        )
        .await;
        assert_eq!(response.status(), expected);
        let body = json_body(response).await;
        if expected == StatusCode::FORBIDDEN {
            assert_eq!(body, json!({"error":"Forbidden"}));
        }
    }
    let _: ::axum::Router =
        crate::axum::router_with_stores(::axum::Router::new(), state.stores.clone().unwrap());
    let _: ::axum::Router = crate::axum::router_with_realtime(
        ::axum::Router::new(),
        state.presence.unwrap(),
        state.stores.unwrap(),
    );
}

#[tokio::test]
async fn malformed_unknown_methods_and_private_loader_failures_are_sanitized() {
    let state = state();
    state
        .stores
        .as_ref()
        .unwrap()
        .register(
            "fails",
            StoreDefinition::new(|_| async {
                Err(StoreError::loader("postgres://private-password@db"))
            }),
        )
        .unwrap();
    state
        .stores
        .as_ref()
        .unwrap()
        .register(
            "huge",
            StoreDefinition::new(|_| async { Ok(json!("x".repeat(MAX_STORE_BYTES))) }),
        )
        .unwrap();
    for (name, expected) in [
        ("missing", StatusCode::NOT_FOUND),
        ("fails", StatusCode::INTERNAL_SERVER_ERROR),
        ("huge", StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let response = handle_realtime(
            request(
                LOAD_PATH,
                "POST",
                Some("secret"),
                json!({"store":name,"params":{}}),
            ),
            &state,
            Some("secret"),
        )
        .await;
        assert_eq!(response.status(), expected);
        let body = json_body(response).await;
        assert!(!body.to_string().contains("postgres"));
    }
    for body in [
        Value::Null,
        json!({"store":"fails","params":null}),
        json!({"store":"fails","params":{},"subject":12}),
        json!({"store":"","params":{}}),
        json!("x".repeat(MAX_STORE_BYTES)),
    ] {
        assert_eq!(
            handle_realtime(
                request(LOAD_PATH, "POST", Some("secret"), body),
                &state,
                Some("secret")
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        handle_realtime(
            request(LOAD_PATH, "GET", Some("secret"), Value::Null),
            &state,
            Some("secret")
        )
        .await
        .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[test]
fn public_host_never_exposes_internal_store_endpoints() {
    let mut public = request(LOAD_PATH, "POST", Some("secret"), json!({}));
    public
        .headers_mut()
        .insert("host", "example.com".parse().unwrap());
    assert!(!is_realtime_request(&public));
    assert!(is_realtime_request(&request(
        LOAD_PATH,
        "POST",
        None,
        Value::Null
    )));
    assert!(!is_realtime_request(&request(
        "/_tako/stores/messages",
        "GET",
        None,
        Value::Null
    )));
}

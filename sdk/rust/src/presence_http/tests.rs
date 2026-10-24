use super::*;
use crate::{PresenceAuth, PresenceDefinition, PresenceGrant};
use serde_json::{Value, json};
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
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn presence_internal_endpoints_require_the_bootstrap_token() {
    let registry = PresenceRegistry::new();
    registry.register("lobby", PresenceDefinition::new());
    for token in [None, Some("wrong")] {
        let response = handle_presence(
            request(REGISTRY_PATH, "GET", token, Value::Null),
            &registry,
            Some("secret"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let response = handle_presence(
        request(REGISTRY_PATH, "GET", Some("secret"), Value::Null),
        &registry,
        Some("secret"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[crate::INTERNAL_TOKEN_HEADER], "secret");
    assert_eq!(
        json_body(response).await[0]["channel"],
        "tako.presence:lobby"
    );
    let response = handle_presence(
        request(REGISTRY_PATH, "GET", None, Value::Null),
        &registry,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn authorization_denies_before_returning_identity_or_membership() {
    let registry = PresenceRegistry::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    registry.register(
        "chat",
        PresenceDefinition::new().auth(PresenceAuth::default(), move |input| {
            count.fetch_add(1, Ordering::SeqCst);
            async move {
                (input.params["chatId"] == "allowed"
                    && input
                        .header
                        .as_ref()
                        .is_some_and(|header| header.value == "good"))
                .then_some(PresenceGrant::new("user"))
            }
        }),
    );
    let mut body = json!({"channel":"tako.presence:chat", "operation":"connect", "params":{"chatId":"allowed"}, "header":{"scheme":"Bearer","value":"good"}});
    let response = handle_presence(
        request(AUTHORIZE_PATH, "POST", Some("wrong"), body.clone()),
        &registry,
        Some("secret"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let response = handle_presence(
        request(AUTHORIZE_PATH, "POST", Some("secret"), body.clone()),
        &registry,
        Some("secret"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["subject"], "user");
    body["params"]["chatId"] = json!("denied");
    let response = handle_presence(
        request(AUTHORIZE_PATH, "POST", Some("secret"), body),
        &registry,
        Some("secret"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(response).await, json!({"error":"Forbidden"}));
}

#[test]
fn public_requests_are_left_to_the_app() {
    let mut external = request(REGISTRY_PATH, "GET", None, Value::Null);
    *external.headers_mut().get_mut("host").unwrap() = "example.com".parse().unwrap();
    assert!(!is_presence_request(&external));
    assert!(!is_presence_request(&request(
        "/",
        "GET",
        None,
        Value::Null
    )));
    assert!(is_presence_request(&request(
        AUTHORIZE_PATH,
        "POST",
        None,
        Value::Null
    )));
    let _: ::axum::Router =
        crate::axum::router_with_presence(::axum::Router::new(), Arc::new(PresenceRegistry::new()));
}

#[tokio::test]
async fn rejects_malformed_unknown_and_oversized_internal_requests() {
    let registry = PresenceRegistry::new();
    let malformed = request(AUTHORIZE_PATH, "POST", Some("secret"), Value::Null);
    assert_eq!(
        handle_presence(malformed, &registry, Some("secret"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let unknown = request(
        AUTHORIZE_PATH,
        "POST",
        Some("secret"),
        json!({"channel":"missing", "operation":"connect", "params":{}}),
    );
    assert_eq!(
        handle_presence(unknown, &registry, Some("secret"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let oversized = request(
        AUTHORIZE_PATH,
        "POST",
        Some("secret"),
        json!("x".repeat((1 << 20) + 1)),
    );
    assert_eq!(
        handle_presence(oversized, &registry, Some("secret"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
}

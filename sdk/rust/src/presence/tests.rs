use super::*;
use serde_json::json;

#[tokio::test]
async fn public_presence_has_a_distinct_namespace_and_ws_transport() {
    let registry = PresenceRegistry::new();
    registry.register("chat", PresenceDefinition::new());
    let metadata = registry.metadata();
    assert_eq!(metadata[0]["channel"], "tako.presence:chat");
    assert_eq!(metadata[0]["auth"], false);
    assert_eq!(metadata[0]["transport"], "ws");
    let result = registry.authorize(input("chat", None, None)).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.subject, None);
    assert!(
        registry
            .authorize(input("missing", None, None))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn private_presence_verifies_chat_access_and_returns_trusted_identity() {
    let registry = PresenceRegistry::new();
    registry.register("chat", PresenceDefinition::new()
        .params_schema(json!({"type":"object", "properties":{"chatId":{"type":"string"}}, "required":["chatId"]}))
        .max_connection_lifetime_ms(60_000)
        .auth(PresenceAuth::default(), |input| async move {
            assert_eq!(input.channel, "chat");
            assert_eq!(input.operation, "connect");
            if input.params["chatId"] == "allowed" && input.header.as_ref().is_some_and(|header| header.value == "good") {
                Some(PresenceGrant { subject: Some("verified-user".into()) })
            } else { None }
        }));
    let mut request = input("chat", Some("good"), None);
    request.params = json!({"chatId":"allowed"});
    let result = registry.authorize(request.clone()).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.subject.as_deref(), Some("verified-user"));
    assert_eq!(result.max_connection_lifetime_ms, 60_000);
    request.params = json!({"chatId":"denied"});
    assert!(!registry.authorize(request).await.unwrap().ok);
    assert!(
        !registry
            .authorize(input("chat", None, None))
            .await
            .unwrap()
            .ok
    );
}

#[tokio::test]
async fn cookie_only_presence_does_not_require_an_authorization_header() {
    let registry = PresenceRegistry::new();
    registry.register(
        "private",
        PresenceDefinition::new().auth(
            PresenceAuth {
                header_name: None,
                cookie_name: Some("session".into()),
            },
            |input| async move {
                (input.cookie.as_deref() == Some("valid")).then_some(PresenceGrant {
                    subject: Some("cookie-user".into()),
                })
            },
        ),
    );
    assert_eq!(
        registry.metadata()[0]["auth"],
        json!({"cookieName":"session"})
    );
    assert!(
        registry
            .authorize(input("private", None, Some("valid")))
            .await
            .unwrap()
            .ok
    );
    assert!(
        !registry
            .authorize(input("private", None, Some("invalid")))
            .await
            .unwrap()
            .ok
    );
}

#[test]
fn member_wire_types_match_the_browser_protocol() {
    let snapshot: PresenceSnapshot<serde_json::Value> = serde_json::from_value(json!({
        "type":"tako.presence", "connectionId":"tab", "members":[
            {"connectionId":"tab", "subject":"user", "data":{"typing":false}},
            {"connectionId":"guest", "data":null}
        ]
    }))
    .unwrap();
    assert_eq!(snapshot.members[0].subject.as_deref(), Some("user"));
    assert_eq!(snapshot.members[1].subject, None);
    assert_eq!(
        serde_json::to_value(snapshot).unwrap()["type"],
        "tako.presence"
    );
}

fn input(name: &str, header: Option<&str>, cookie: Option<&str>) -> PresenceVerifyInput {
    PresenceVerifyInput {
        channel: format!("tako.presence:{name}"),
        operation: "connect".into(),
        params: json!({}),
        header: header.map(|value| PresenceHeaderValue {
            scheme: Some("Bearer".into()),
            value: value.into(),
        }),
        cookie: cookie.map(str::to_owned),
    }
}

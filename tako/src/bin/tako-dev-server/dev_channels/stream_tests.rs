use super::*;
use pingora_core::protocols::l4::stream::Stream;
use serde_json::json;
use std::time::Duration;
use tokio::net::TcpListener;

/// An app that only serves its channel registry.
async fn registry_app(definitions: serde_json::Value) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let body = definitions.to_string();
            tokio::spawn(async move {
                let mut session = Session::new_h1(Box::new(Stream::from(socket)));
                session.read_request().await.unwrap();
                assert_eq!(session.req_header().uri.path(), "/channels/registry");
                let mut header = ResponseHeader::build(200, None).unwrap();
                header.insert_header("Content-Length", body.len()).unwrap();
                header
                    .insert_header("Content-Type", "application/json")
                    .unwrap();
                header
                    .insert_header("x-tako-internal-token", "test-token")
                    .unwrap();
                session
                    .write_response_header(Box::new(header), false)
                    .await
                    .unwrap();
                session
                    .write_response_body(Some(body.into()), true)
                    .await
                    .unwrap();
            });
        }
    });
    port
}

#[tokio::test]
async fn params_select_the_stream_subscribers_and_publishers_share() {
    let app_port = registry_app(json!([{
        "channel": "chat",
        "paramsSchema": {
            "type": "object",
            "properties": {"room": {"type": "integer"}},
            "required": ["room"],
            "additionalProperties": false
        },
        "auth": false,
        "maxConnectionLifetimeMs": 2000,
    }]))
    .await;
    let routes = crate::proxy::Routes::default();
    routes.set_routes("app".into(), vec!["app.test".into()], app_port, true);
    routes.set_store_auth("app", "app.tako".into(), "test-token".into());
    let target = routes.lookup("app.test", "/").unwrap();
    let app_id = target.channel_store_key.clone();
    let dev_store = DevChannelStore::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    {
        let dev_store = dev_store.clone();
        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let (dev_store, target) = (dev_store.clone(), target.clone());
                tokio::spawn(async move {
                    let mut session = Session::new_h1(Box::new(Stream::from(socket)));
                    session.read_request().await.unwrap();
                    let path = session.req_header().uri.path().to_owned();
                    let _ = try_handle(&mut session, &dev_store, &target, &path, "GET").await;
                });
            }
        });
    }
    let subscribe =
        |query: &str| reqwest::get(format!("http://{endpoint}/_tako/channels/chat?{query}"));

    // Params are validated against the definition, as in production.
    assert_eq!(subscribe("room=one").await.unwrap().status(), 400);
    assert_eq!(subscribe("").await.unwrap().status(), 400);

    let mut one = subscribe("room=1").await.unwrap();
    let mut two = subscribe("room=2").await.unwrap();
    assert_eq!(one.status(), 200);
    assert_eq!(two.status(), 200);
    let publish = |params: serde_json::Value, data: i64| {
        dev_store
            .publish(
                &app_id,
                &TrustedPublication::parse("chat", &params, json!({"type": "msg", "data": data}))
                    .unwrap(),
            )
            .unwrap()
    };
    // `1.0` from a typed publisher and `1` from the query name one stream.
    let published = publish(json!({"room": 1.0}), 1);
    assert_eq!(published.channel, "chat");
    publish(json!({"room": 2}), 2);
    for (response, expected) in [(&mut one, 1), (&mut two, 2)] {
        let chunk = tokio::time::timeout(Duration::from_secs(2), response.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame = std::str::from_utf8(&chunk).unwrap();
        assert!(!frame.contains("tako.stream"), "{frame}");
        let data = frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        let message: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(message["channel"], "chat", "{frame}");
        assert_eq!(message["data"], expected, "{frame}");
    }
    // Nothing was published to the unparameterized stream.
    assert!(
        dev_store
            .store_for_app(&app_id)
            .read_after("chat", None, 10)
            .unwrap()
            .is_empty()
    );
}

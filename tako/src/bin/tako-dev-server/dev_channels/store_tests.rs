use super::*;
use bytes::Bytes;
use pingora_core::protocols::l4::stream::Stream;
use serde_json::{Value, json};
use tokio::net::TcpListener;

fn query(params: Value) -> String {
    let mut url = reqwest::Url::parse("http://localhost").unwrap();
    url.query_pairs_mut()
        .append_pair("params", &params.to_string());
    url.query().unwrap().to_owned()
}

async fn app() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut session = Session::new_h1(Box::new(Stream::from(socket)));
                session.read_request().await.unwrap();
                assert_eq!(session.req_header().headers["host"], "app.tako");
                assert_eq!(
                    session.req_header().headers["x-tako-internal-token"],
                    "private-bootstrap-token"
                );
                let mut body = Vec::new();
                while let Some(chunk) = session.read_request_body().await.unwrap() {
                    body.extend_from_slice(&chunk);
                }
                let input: Value = if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&body).unwrap()
                };
                let (status, data) = match session.req_header().uri.path() {
                    "/channels/registry" => (
                        200,
                        json!([{
                            "channel":"tako.store:messages", "paramsSchema":{"type":"object","properties":{"room":{"type":"integer"},"last_message_id":{"type":"integer"},"filter":{"type":"object","properties":{"unread":{"type":"boolean"}},"required":["unread"],"additionalProperties":false}},"required":["room","last_message_id"],"additionalProperties":false},
                            "auth":{"headerName":"authorization"}
                        }]),
                    ),
                    "/channels/authorize" if input["header"]["value"] == "good" => {
                        assert_eq!(input["operation"], "subscribe");
                        assert_eq!(input["params"]["room"], 7);
                        assert_eq!(input["params"]["last_message_id"], 42);
                        if let Some(filter) = input["params"].get("filter") {
                            assert_eq!(filter, &json!({"unread":true}));
                        }
                        (200, json!({"ok":true,"subject":"verified-user"}))
                    }
                    "/channels/authorize" => (403, json!({"error":"Forbidden"})),
                    "/stores/load" => {
                        assert_eq!(input["store"], "messages");
                        assert_eq!(input["params"]["room"], 7);
                        assert_eq!(input["params"]["last_message_id"], 42);
                        assert_eq!(input["subject"], "verified-user");
                        if let Some(filter) = input["params"].get("filter") {
                            assert_eq!(filter, &json!({"unread":true}));
                        }
                        (200, json!({"data":[{"text":"hello"}]}))
                    }
                    path => panic!("Unexpected path {path}"),
                };
                let body = data.to_string();
                let mut header = ResponseHeader::build(status, None).unwrap();
                header
                    .insert_header("Content-Type", "application/json")
                    .unwrap();
                header
                    .insert_header("Content-Length", body.len().to_string())
                    .unwrap();
                header
                    .insert_header("x-tako-internal-token", "private-bootstrap-token")
                    .unwrap();
                session
                    .write_response_header(Box::new(header), false)
                    .await
                    .unwrap();
                session
                    .write_response_body(Some(Bytes::from(body)), true)
                    .await
                    .unwrap();
            });
        }
    });
    (port, task)
}

#[tokio::test]
async fn store_public_routes_authorize_load_and_subscribe_without_exposing_bootstrap() {
    let (app_port, app_task) = app().await;
    let routes = crate::proxy::Routes::default();
    routes.set_routes(
        "registered-app".into(),
        vec!["app.test".into()],
        app_port,
        true,
    );
    routes.set_store_auth(
        "registered-app",
        "app.tako".into(),
        "private-bootstrap-token".into(),
    );
    let target = routes.lookup("app.test", "/").unwrap();
    let store = DevChannelStore::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let target = target.clone();
            let store = store.clone();
            tokio::spawn(async move {
                let mut session = Session::new_h1(Box::new(Stream::from(socket)));
                session.read_request().await.unwrap();
                let path = session.req_header().uri.path().to_owned();
                let _ = try_handle_store(&mut session, &store, &target, &path).await;
            });
        }
    });
    let client = reqwest::Client::new();
    let load = format!(
        "{url}/_tako/stores/messages?{}",
        query(json!({"room":7,"last_message_id":42}))
    );
    let response = client
        .get(&load)
        .header("Authorization", "Bearer good")
        .header("x-tako-subject", "forged")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(!response.headers().contains_key("x-tako-internal-token"));
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"data":[{"text":"hello"}]})
    );
    assert_eq!(client.get(&load).send().await.unwrap().status(), 401);
    let nested = client
        .get(format!(
            "{url}/_tako/stores/messages?{}",
            query(json!({"room":7,"last_message_id":42,"filter":{"unread":true}}))
        ))
        .header("Authorization", "Bearer good")
        .send()
        .await
        .unwrap();
    assert_eq!(nested.status(), 200);
    assert_eq!(
        nested.json::<Value>().await.unwrap()["data"][0]["text"],
        "hello"
    );
    assert_eq!(
        client
            .get(format!("{url}/_tako/stores/messages?params=not-json"))
            .header("Authorization", "Bearer good")
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        client
            .get(&load)
            .header("Authorization", "Bearer bad")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .get(format!("{load}&subject=forged"))
            .header("Authorization", "Bearer good")
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        client
            .get(format!(
                "{url}/_tako/stores/unknown?{}",
                query(json!({"room":7,"last_message_id":42}))
            ))
            .header("Authorization", "Bearer good")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(client.post(&load).send().await.unwrap().status(), 405);
    assert_eq!(
        client
            .get(&load)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    let events = format!(
        "{url}/_tako/channels/tako.store%3Amessages?{}",
        query(json!({"room":7,"last_message_id":42}))
    );
    assert_eq!(client.get(&events).send().await.unwrap().status(), 401);
    let mut response = client
        .get(&events)
        .header("Authorization", "Bearer good")
        .header("Last-Event-ID", "expired")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(!response.headers().contains_key("x-tako-internal-token"));
    let chunk = response.chunk().await.unwrap().unwrap();
    assert!(
        std::str::from_utf8(&chunk)
            .unwrap()
            .contains("tako.store.ready")
    );
    server.abort();
    app_task.abort();
}

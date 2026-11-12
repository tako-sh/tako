use super::*;
use serde_json::json;
use std::time::Duration;

/// An app whose authorizer allows every operation with its definition's lifecycle.
async fn registry_app(definitions: serde_json::Value) -> u16 {
    let lifecycle = test_app::grant(&definitions[0]);
    test_app::fake_app(definitions, move |_| (200, lifecycle.clone()))
        .await
        .0
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
    let dev_store = DevChannelStore::new();
    let (endpoint, app_id) = test_app::dev_proxy(app_port, &dev_store).await;
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

#[tokio::test]
async fn closed_streams_replay_from_start_and_end_readers_as_in_production() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let app_port = registry_app(json!([{
        "channel": "feed",
        "paramsSchema": {
            "type": "object",
            "properties": {"id": {"type": "integer"}},
            "required": ["id"],
            "additionalProperties": false
        },
        "auth": false,
        "transport": "ws",
        "maxConnectionLifetimeMs": 3000,
    }]))
    .await;
    let dev_store = DevChannelStore::new();
    let (endpoint, app_id) = test_app::dev_proxy(app_port, &dev_store).await;
    // The internal socket's close command, as main.rs applies it.
    let write = |write: tako_workflows::ChannelWrite| {
        let publication = trusted_publication(tako_workflows::ChannelPublishRequest {
            app: &app_id,
            channel: "feed",
            params: &json!({"id": 1}),
            build: None,
            write,
        })?;
        dev_store.publish(&app_id, &publication)
    };
    let token =
        |data: &str| tako_workflows::ChannelWrite::Publish(json!({"type": "token", "data": data}));
    let first = write(token("hel")).unwrap();
    write(token("lo")).unwrap();
    let close = || tako_workflows::ChannelWrite::Close { lifecycle: None };
    let marker = write(close()).unwrap();
    assert_eq!(marker.channel, "feed");
    assert_eq!(write(close()).unwrap().id, marker.id);
    assert!(matches!(
        write(token("late")),
        Err(ChannelError::StreamClosed)
    ));
    assert!(matches!(
        write(tako_workflows::ChannelWrite::Close {
            lifecycle: Some(json!({"ephemeral": true}))
        }),
        Err(ChannelError::BadRequest(_))
    ));

    let request = |query: &str, headers: &str| {
        let request = format!(
            "GET /_tako/channels/feed?{query} HTTP/1.1\r\nHost: app.test\r\n{headers}Connection: close\r\n\r\n"
        );
        async move {
            let mut client = tokio::net::TcpStream::connect(endpoint).await.unwrap();
            client.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
                .await
                .expect("a closed stream ends the response")
                .unwrap();
            response
        }
    };
    let close_event = format!("id: {}\nevent: tako.close\ndata: {{}}\n\n", marker.id);
    let data = |response: &[u8]| -> Vec<serde_json::Value> {
        std::str::from_utf8(response)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap())
            .collect()
    };
    let replay = request("id=1&from=start", "").await;
    let text = String::from_utf8_lossy(&replay).into_owned();
    assert_eq!(
        data(&replay)
            .iter()
            .map(|message| message.get("data").cloned())
            .collect::<Vec<_>>(),
        vec![Some(json!("hel")), Some(json!("lo")), None],
        "{text}"
    );
    assert!(text.contains(&close_event), "{text}");
    assert!(!text.contains("tako.stream"), "{text}");
    for (query, headers, count) in [
        (
            "id=1&from=start",
            format!("Last-Event-ID: {}\r\n", first.id),
            2,
        ),
        ("id=1", String::new(), 1),
    ] {
        let response = request(query, &headers).await;
        assert_eq!(data(&response).len(), count, "{query}");
        assert!(String::from_utf8_lossy(&response).contains(&close_event));
    }
    let invalid = request("id=1&from=end", "").await;
    assert!(invalid.starts_with(b"HTTP/1.1 400"));

    // WebSocket readers end with the stream-closed close frame.
    let upgrade = "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n";
    let mut close_frame = vec![0x88, 15];
    close_frame.extend_from_slice(&4200u16.to_be_bytes());
    close_frame.extend_from_slice(b"stream-closed");
    for (query, texts) in [
        ("id=1&from=start".to_owned(), 2),
        (format!("id=1&from=start&last_message_id={}", first.id), 1),
        ("id=1".to_owned(), 0),
    ] {
        let response = request(&query, upgrade).await;
        assert!(response.starts_with(b"HTTP/1.1 101"), "{query}");
        assert!(response.ends_with(&close_frame), "{query}");
        // Text frames start with 0x81, a byte no JSON payload or header contains.
        assert_eq!(
            response.iter().filter(|byte| **byte == 0x81).count(),
            texts,
            "{query}"
        );
    }
}

#[tokio::test]
async fn undeclared_params_are_rejected_as_in_production() {
    let (app_port, authorized) = test_app::fake_app(
        json!([{
            "channel": "open",
            "paramsSchema": {"type": "object", "properties": {"room": {"type": "integer"}}},
            "auth": false,
            "maxConnectionLifetimeMs": 500,
        }]),
        |_| (200, json!({"ok": true, "maxConnectionLifetimeMs": 500})),
    )
    .await;
    let dev_store = DevChannelStore::new();
    let (endpoint, _) = test_app::dev_proxy(app_port, &dev_store).await;
    let subscribe =
        |query: &str| reqwest::get(format!("http://{endpoint}/_tako/channels/open?{query}"));
    for query in ["room=1&junk=a", "junk=a"] {
        let response = subscribe(query).await.unwrap();
        assert_eq!(response.status(), 400, "{query}");
        assert!(response.text().await.unwrap().contains("unknown param"));
    }
    assert!(authorized.lock().unwrap().is_empty());
    assert_eq!(subscribe("room=1&from=start").await.unwrap().status(), 200);
}

use super::*;
use crate::{ChannelAuthResponse, ChannelStore};
use futures_util::{SinkExt, StreamExt};
use pingora_core::protocols::l4::stream::Stream;
use pingora_proxy::Session;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::test]
async fn presence_websockets_isolate_chats_and_track_verified_tabs() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Arc::new(ChannelStore::open_in_memory().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let store = store.clone();
            tokio::spawn(async move {
                let mut session = Session::new_h1(Box::new(Stream::from(socket)));
                session.read_request().await.unwrap();
                let query = session.req_header().uri.query().unwrap_or_default();
                let params =
                    crate::params::validate_query(&json!({"type":"object"}), query).unwrap();
                let auth: ChannelAuthResponse = serde_json::from_value(json!({
                    "ok": true, "subject": "verified-user", "transport": "ws",
                }))
                .unwrap();
                let response = build_websocket_upgrade_response(session.req_header()).unwrap();
                session
                    .write_response_header(Box::new(response), false)
                    .await
                    .unwrap();
                let _ = serve(
                    &mut session,
                    &store,
                    "tako.presence:chat",
                    None,
                    auth,
                    params,
                    Default::default(),
                )
                .await;
            });
        }
    });

    let (mut first, _) = connect_async(format!("ws://{addr}/?chatId=one"))
        .await
        .unwrap();
    assert!(
        first
            .next()
            .await
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap()
            .contains("tako.presence.ready")
    );
    first
        .send(Message::text(
            r#"{"type":"tako.presence","subject":"forged","data":{"typing":false}}"#,
        ))
        .await
        .unwrap();
    let snapshot: Value =
        serde_json::from_str(first.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(snapshot["members"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["members"][0]["subject"], "verified-user");
    let first_id = snapshot["connectionId"].clone();

    let (mut other_chat, _) = connect_async(format!("ws://{addr}/?chatId=two"))
        .await
        .unwrap();
    other_chat.next().await.unwrap().unwrap();
    other_chat
        .send(Message::text(r#"{"type":"tako.presence","data":{}}"#))
        .await
        .unwrap();
    let snapshot: Value =
        serde_json::from_str(other_chat.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(snapshot["members"].as_array().unwrap().len(), 1);
    assert_ne!(snapshot["connectionId"], first_id);

    let (mut second, _) = connect_async(format!("ws://{addr}/?chatId=one"))
        .await
        .unwrap();
    second.next().await.unwrap().unwrap();
    second
        .send(Message::text(
            r#"{"type":"tako.presence","data":{"typing":true}}"#,
        ))
        .await
        .unwrap();
    let snapshot: Value =
        serde_json::from_str(second.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(snapshot["members"].as_array().unwrap().len(), 2);
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(3), first.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let snapshot: Value = serde_json::from_str(snapshot.to_text().unwrap()).unwrap();
    assert_eq!(snapshot["members"].as_array().unwrap().len(), 2);
    second.close(None).await.unwrap();
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(3), first.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let snapshot: Value = serde_json::from_str(snapshot.to_text().unwrap()).unwrap();
    assert_eq!(snapshot["members"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["members"][0]["connectionId"], first_id);
    first.close(None).await.unwrap();
    other_chat.close(None).await.unwrap();
    server.abort();
}

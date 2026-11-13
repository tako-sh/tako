use super::*;
use crate::native_channel_client::tests::FakeBroker;
use std::sync::atomic::Ordering;

fn publication(
    channel: &str,
    params: serde_json::Value,
    payload: serde_json::Value,
) -> TrustedPublication {
    TrustedPublication::parse(channel, &params, payload).unwrap()
}

#[tokio::test]
async fn synchronous_publisher_works_inside_a_runtime_without_nested_block_on() {
    let link = Arc::new(FakeBroker::default());
    let publisher = BlockingPublisher::with_link(link.clone()).unwrap();
    let result = publisher
        .publish(
            "chat",
            &publication(
                "events",
                serde_json::Value::Null,
                serde_json::json!({
                    "type":"update", "data":{"value":1}, "lifecycle":{"ephemeral":true}
                }),
            ),
            None,
        )
        .unwrap();
    assert_eq!(result["channel"], "events");
    // The lifecycle rides on the publish itself; no separate sync touches the stream.
    assert_eq!(*link.calls.lock(), vec!["register", "publish"]);
    assert!(link.lifecycle.lock().as_ref().unwrap().ephemeral);
    assert_eq!(link.publishes.load(Ordering::SeqCst), 1);
}
#[test]
fn parameterized_publications_reach_the_broker_by_stream_key_and_ack_the_public_name() {
    let link = Arc::new(FakeBroker::default());
    let publisher = BlockingPublisher::with_link(link.clone()).unwrap();
    let params = serde_json::json!({"room": "a"});
    let result = publisher
        .publish(
            "chat",
            &publication(
                "events",
                params.clone(),
                serde_json::json!({"type":"update", "data":1}),
            ),
            None,
        )
        .unwrap();
    assert_eq!(result["channel"], "events");
    assert_eq!(
        *link.channels.lock(),
        vec![
            tako_channels::ChannelStream::new("events", &params)
                .key()
                .to_owned()
        ]
    );
}
#[test]
fn closes_reach_the_broker_as_markers_and_closed_streams_reject_by_name() {
    let link = Arc::new(FakeBroker::default());
    let publisher = BlockingPublisher::with_link(link.clone()).unwrap();
    let params = serde_json::json!({"room": "a"});
    let close = TrustedPublication::close(
        "events",
        &params,
        Some(serde_json::json!({"replayWindowMs": 1000})),
    )
    .unwrap();
    let marker = publisher.publish("chat", &close, None).unwrap();
    assert_eq!(marker["channel"], "events");
    assert_eq!(marker["type"], tako_channels::CLOSE_MESSAGE_TYPE);
    assert_eq!(*link.calls.lock(), vec!["register", "publish"]);
    assert_eq!(
        link.lifecycle.lock().as_ref().unwrap().replay_window_ms,
        1000
    );

    link.closed.store(true, Ordering::SeqCst);
    let error = publisher
        .publish(
            "chat",
            &publication(
                "events",
                params,
                serde_json::json!({"type":"update", "data":1}),
            ),
            None,
        )
        .unwrap_err();
    assert_eq!(
        error,
        tako_core::SocketError::new(
            tako_core::ErrorCode::ChannelClosed,
            "channel stream is closed"
        )
    );
}
#[test]
fn failed_registration_never_dispatches_publication_and_redacts_credentials() {
    let link = Arc::new(FakeBroker::default());
    link.reject.store(true, Ordering::SeqCst);
    let publisher = BlockingPublisher::with_link(link.clone()).unwrap();
    let error = publisher
        .publish(
            "chat",
            &publication(
                "events",
                serde_json::Value::Null,
                serde_json::json!({"type":"update", "data":null}),
            ),
            Some("postgres://secret".into()),
        )
        .unwrap_err();
    assert!(error.message.contains("registration"));
    assert!(!error.message.contains("secret"));
    assert_eq!(*link.calls.lock(), vec!["register"]);
    assert_eq!(link.publishes.load(Ordering::SeqCst), 0);
}
#[test]
fn oversized_payloads_are_refused_before_queueing() {
    let link = Arc::new(FakeBroker::default());
    let publisher = BlockingPublisher::with_link(link.clone()).unwrap();
    assert!(
        publisher
            .publish(
                "chat",
                &publication(
                    "events",
                    serde_json::Value::Null,
                    serde_json::json!({"type":"update", "data":"x".repeat(MAX_PAYLOAD)}),
                ),
                None
            )
            .is_err()
    );
    assert!(link.calls.lock().is_empty());
}

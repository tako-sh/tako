use super::*;
use crate::native_channel_client::tests::FakeBroker;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn synchronous_publisher_works_inside_a_runtime_without_nested_block_on() {
    let link = Arc::new(FakeBroker::default());
    let publisher = BlockingPublisher::with_link(link.clone()).unwrap();
    let result = publisher
        .publish(
            "chat",
            "events",
            serde_json::json!({
                "type":"update", "data":{"value":1}, "lifecycle":{"ephemeral":true}
            }),
            None,
        )
        .unwrap();
    assert_eq!(result["channel"], "events");
    assert_eq!(*link.calls.lock(), vec!["register", "sync", "publish"]);
    assert_eq!(link.publishes.load(Ordering::SeqCst), 1);
}
#[test]
fn failed_registration_never_dispatches_publication_and_redacts_credentials() {
    let link = Arc::new(FakeBroker::default());
    link.reject.store(true, Ordering::SeqCst);
    let publisher = BlockingPublisher::with_link(link.clone()).unwrap();
    let error = publisher
        .publish(
            "chat",
            "events",
            serde_json::json!({"type":"update", "data":null}),
            Some("postgres://secret".into()),
        )
        .unwrap_err();
    assert!(error.contains("registration"));
    assert!(!error.contains("secret"));
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
                "events",
                serde_json::json!({"type":"update", "data":"x".repeat(MAX_PAYLOAD)}),
                None
            )
            .is_err()
    );
    assert!(link.calls.lock().is_empty());
}

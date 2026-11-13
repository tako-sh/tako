use super::*;
use serde_json::json;
use std::time::Duration;

/// Ephemeral delivery comes from the app's grant, as in production, and is in
/// place before the first publish.
#[tokio::test]
async fn subscription_uses_the_granted_mode_before_first_publish() {
    let (app_port, calls) = test_app::fake_app(
        json!([{
            "channel":"progress", "paramsSchema":{"type":"object"},
            "auth":false, "ephemeral":true, "maxConnectionLifetimeMs":2000,
        }]),
        |_| {
            (
                200,
                json!({"ok": true, "ephemeral": true, "maxConnectionLifetimeMs": 2000}),
            )
        },
    )
    .await;
    let dev_store = DevChannelStore::new();
    let (endpoint, app_id) = test_app::dev_proxy(app_port, &dev_store).await;
    let mut response = reqwest::get(format!("http://{endpoint}/_tako/channels/progress"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let message = dev_store
        .publish(
            &app_id,
            &TrustedPublication::parse(
                "progress",
                &serde_json::Value::Null,
                json!({"type": "progress", "data": 42, "lifecycle": {"ephemeral": true}}),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(message.id.is_empty());
    let chunk = tokio::time::timeout(Duration::from_secs(1), response.chunk())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame = std::str::from_utf8(&chunk).unwrap();
    let payload = frame
        .strip_prefix("data: ")
        .and_then(|rest| rest.strip_suffix("\n\n"))
        .unwrap();
    let delivered: serde_json::Value = serde_json::from_str(payload).unwrap();
    // Ephemeral frames carry no replay id, only the origin event identity.
    assert_eq!(delivered["id"], "");
    assert_eq!(delivered["channel"], "progress");
    assert_eq!(delivered["type"], "progress");
    assert_eq!(delivered["data"], 42);
    assert_eq!(delivered["eventId"]["sequence"], 1);
    assert!(
        dev_store
            .store_for_app(&app_id)
            .read_after("progress", None, 100)
            .unwrap()
            .is_empty()
    );
    assert_eq!(calls.lock().unwrap()[0]["operation"], "subscribe");
}

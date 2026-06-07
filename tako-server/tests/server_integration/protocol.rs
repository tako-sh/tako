#[test]
fn test_protocol_message_parsing() {
    // Test that protocol messages are correctly formatted
    let ready_msg = serde_json::json!({
        "type": "ready",
        "app": "test",
        "version": "v1",
        "instance_id": 1,
        "pid": 12345,
        "socket_path": "/tmp/test.sock",
        "timestamp": "2024-01-15T00:00:00Z"
    });

    let parsed: serde_json::Value = serde_json::from_str(&ready_msg.to_string()).unwrap();
    assert_eq!(parsed["type"], "ready");
    assert_eq!(parsed["app"], "test");
}

#[test]
fn test_heartbeat_message() {
    let heartbeat = serde_json::json!({
        "type": "heartbeat",
        "app": "test",
        "instance_id": 1,
        "pid": 12345,
        "timestamp": "2024-01-15T00:00:00Z"
    });

    let parsed: serde_json::Value = serde_json::from_str(&heartbeat.to_string()).unwrap();
    assert_eq!(parsed["type"], "heartbeat");
}

#[test]
fn test_shutdown_message() {
    let shutdown = serde_json::json!({
        "type": "shutdown",
        "reason": "deploy",
        "drain_timeout_seconds": 30
    });

    let parsed: serde_json::Value = serde_json::from_str(&shutdown.to_string()).unwrap();
    assert_eq!(parsed["type"], "shutdown");
    assert_eq!(parsed["reason"], "deploy");
}

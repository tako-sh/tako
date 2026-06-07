use super::*;

#[tokio::test]
async fn subscribe_logs_streams_backlog_and_live_entries() {
    let (state, _tmp) = test_state();
    insert_test_app(&state, "/proj", "my-app");

    // Push some entries to the log buffer before subscribing.
    {
        let s = state.lock().unwrap();
        let app = s.apps.get("/proj/tako.toml").unwrap();
        app.log_buffer.push(
            r#"{"timestamp":"00:00:01","level":"Info","scope":"app","message":"line-1"}"#
                .to_string(),
        );
        app.log_buffer.push(
            r#"{"timestamp":"00:00:02","level":"Info","scope":"app","message":"line-2"}"#
                .to_string(),
        );
    }

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "SubscribeLogs",
        "config_path": "/proj/tako.toml",
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    // First response: LogsSubscribed
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert!(matches!(resp, Response::LogsSubscribed));

    // Next: two backlog entries
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::LogEntry { id, line } => {
            assert_eq!(id, 0);
            assert!(line.contains("line-1"));
        }
        other => panic!("expected LogEntry, got: {other:?}"),
    }

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::LogEntry { id, line } => {
            assert_eq!(id, 1);
            assert!(line.contains("line-2"));
        }
        other => panic!("expected LogEntry, got: {other:?}"),
    }

    // Push a live entry while subscribed.
    {
        let s = state.lock().unwrap();
        let app = s.apps.get("/proj/tako.toml").unwrap();
        app.log_buffer.push(
            r#"{"timestamp":"00:00:03","level":"Info","scope":"app","message":"line-3"}"#
                .to_string(),
        );
    }

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::LogEntry { id, line } => {
            assert_eq!(id, 2);
            assert!(line.contains("line-3"));
        }
        other => panic!("expected LogEntry, got: {other:?}"),
    }

    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn subscribe_logs_returns_error_for_unknown_app() {
    let (state, _tmp) = test_state();

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "SubscribeLogs",
        "config_path": "/nonexistent/tako.toml",
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert!(matches!(resp, Response::Error { .. }));

    drop(w);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn subscribe_logs_counts_as_control_client() {
    let (state, _tmp) = test_state();
    insert_test_app(&state, "/proj", "my-app");

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "SubscribeLogs",
        "config_path": "/proj/tako.toml",
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    assert!(line.contains("LogsSubscribed"));

    // While subscribed, control_clients should be 1.
    let clients = query_control_clients(state.clone()).await;
    assert_eq!(clients, 1);

    // Disconnect.
    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();

    // After disconnect, control_clients should be 0.
    let clients = query_control_clients(state).await;
    assert_eq!(clients, 0);
}

#[test]
fn push_user_action_emits_sdk_wire_format_with_kind() {
    let buf = state::LogBuffer::new();
    let (_backlog, mut rx, _truncated) = buf.subscribe(None);

    push_user_action(&buf, "restarted");

    let entry = rx.try_recv().expect("user-action line pushed");
    let v: serde_json::Value =
        serde_json::from_str(&entry.line).expect("user-action line is valid JSON");

    assert_eq!(v.get("scope").and_then(|x| x.as_str()), Some("tako"));
    assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("restarted"));
    assert_eq!(v.get("level").and_then(|x| x.as_str()), Some("info"));
    let ts = v.get("ts").and_then(|x| x.as_i64()).expect("numeric ts");
    assert!(ts > 0, "ts should be filled with unix millis, got {ts}");
}

#[test]
fn forward_child_log_line_passes_structured_json_through_verbatim() {
    let buf = state::LogBuffer::new();
    let (_backlog, mut rx, _truncated) = buf.subscribe(None);

    let line = r#"{"ts":42,"level":"info","scope":"worker","msg":"hi"}"#;
    forward_child_log_line(&buf, line.to_string(), "info", "worker");

    let entry = rx.try_recv().expect("line forwarded");
    assert_eq!(entry.line, line);
}

#[test]
fn forward_child_log_line_wraps_plain_text_as_scoped_log() {
    let buf = state::LogBuffer::new();
    let (_backlog, mut rx, _truncated) = buf.subscribe(None);

    forward_child_log_line(&buf, "raw worker output".to_string(), "warn", "worker");

    let entry = rx.try_recv().expect("line forwarded");
    let v: serde_json::Value = serde_json::from_str(&entry.line).expect("valid JSON");
    assert_eq!(v.get("scope").and_then(|x| x.as_str()), Some("worker"));
    assert_eq!(v.get("level").and_then(|x| x.as_str()), Some("warn"));
    assert_eq!(
        v.get("msg").and_then(|x| x.as_str()),
        Some("raw worker output")
    );
    let ts = v.get("ts").and_then(|x| x.as_i64()).expect("numeric ts");
    assert!(ts > 0);
}

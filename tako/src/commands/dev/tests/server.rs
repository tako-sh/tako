use super::*;

#[tokio::test]
async fn tcp_probe_detects_open_port() {
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        return;
    };
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = listener.accept().await;
    });

    assert!(tcp_probe(("127.0.0.1", port), 200).await);
}

#[tokio::test]
async fn tcp_probe_detects_closed_port() {
    assert!(!tcp_probe(("127.0.0.1", 0), 50).await);
}

#[test]
fn bootstrap_dev_events_marks_running_app_ready_when_pid_is_known() {
    let events = bootstrap_dev_events("running", Some(4242));

    assert_eq!(events.len(), 2);
    match &events[0] {
        DevEvent::AppPid(pid) => assert_eq!(pid, &4242),
        other => panic!("expected AppPid, got {other:?}"),
    }
    assert!(matches!(events[1], DevEvent::AppReady));
}

#[test]
fn bootstrap_dev_events_marks_idle_app_stopped() {
    let events = bootstrap_dev_events("idle", None);

    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], DevEvent::AppStopped));
}

#[test]
fn bootstrap_dev_events_waits_for_pid_before_marking_running() {
    let events = bootstrap_dev_events("running", None);

    assert!(events.is_empty());
}

#[tokio::test]
async fn tcp_probe_retries_until_port_is_open() {
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        return;
    };
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await else {
            return;
        };
        let _ = listener.accept().await;
    });

    let mut ok = false;
    for _ in 0..10 {
        if tcp_probe(("127.0.0.1", port), 10).await {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(ok);
}

#[tokio::test]
async fn tcp_probe_returns_false_for_closed_port() {
    assert!(!tcp_probe(("127.0.0.1", 0), 10).await);
}

#[tokio::test]
async fn wait_for_dev_server_stopped_waits_for_socket_path_to_disappear() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("dev-server.sock");
    std::fs::write(&socket_path, "stale socket path").unwrap();

    let remove_path = socket_path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = tokio::fs::remove_file(remove_path).await;
    });

    let start = std::time::Instant::now();
    prepare::wait_for_dev_server_stopped_with_socket_path("127.0.0.1:59091", Some(&socket_path))
        .await;

    assert!(
        start.elapsed() >= Duration::from_millis(150),
        "returned before socket path cleanup completed"
    );
}

#[test]
fn restart_not_required_when_no_existing_server() {
    assert!(!restart_required_for_requested_listen(
        None,
        "127.0.0.1:47831"
    ));
}

#[test]
fn restart_not_required_when_existing_listen_matches() {
    assert!(!restart_required_for_requested_listen(
        Some("127.0.0.1:47831"),
        "127.0.0.1:47831"
    ));
}

#[test]
fn restart_required_when_existing_listen_differs() {
    assert!(restart_required_for_requested_listen(
        Some("127.0.0.1:8443"),
        "127.0.0.1:47831"
    ));
}

#[test]
fn parse_port_from_listen_handles_valid_and_invalid_values() {
    assert_eq!(port_from_listen("127.0.0.1:47831"), Some(47831));
    assert_eq!(port_from_listen("localhost:443"), Some(443));
    assert_eq!(port_from_listen("bad-listen"), None);
    assert_eq!(port_from_listen("host:not-a-port"), None);
}

#[test]
fn host_and_port_parser_handles_default_and_explicit_ports() {
    assert_eq!(
        host_and_port_from_url("https://app.test/"),
        Some(("app.test".to_string(), 443))
    );
    assert_eq!(
        host_and_port_from_url("https://app.test:47831/"),
        Some(("app.test".to_string(), 47831))
    );
}

#[test]
fn doctor_omits_duplicate_port_line_when_listen_includes_same_port() {
    let lines = doctor_dev_server_lines("127.0.0.1:47831", 47831, false, false, true, 53535);
    assert!(
        !lines.iter().any(|line| line.starts_with("  port:")),
        "doctor output should not duplicate listen port: {lines:?}"
    );
}

#[test]
fn doctor_keeps_port_line_when_listen_does_not_include_port() {
    let lines = doctor_dev_server_lines("(unknown)", 47831, false, false, true, 53535);
    assert!(
        lines.iter().any(|line| line == "  port: 47831"),
        "doctor output should keep explicit port when listen does not include one: {lines:?}"
    );
}

#[test]
fn doctor_preflight_lines_show_proxy_not_loaded() {
    let lines = doctor_local_forwarding_preflight_lines("127.77.0.1", false, false, true);
    assert!(lines.iter().any(|line| line.contains("not loaded")));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("TCP 127.77.0.1:443 (unreachable)"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("TCP 127.77.0.1:80 (ok)"))
    );
}

#[test]
fn doctor_preflight_lines_show_proxy_loaded() {
    let lines = doctor_local_forwarding_preflight_lines("127.77.0.1", true, true, true);
    assert!(lines.iter().any(|line| line.contains("loaded")));
}

#[test]
fn unavailable_error_detection_matches_missing_or_stale_socket_errors() {
    assert!(is_dev_server_unavailable_error_message(
        "No such file or directory (os error 2)"
    ));
    assert!(is_dev_server_unavailable_error_message(
        "Connection refused (os error 61)"
    ));
    assert!(is_dev_server_unavailable_error_message(
        "Operation not permitted (os error 1)"
    ));
    assert!(is_dev_server_unavailable_error_message(
        "Permission denied (os error 13)"
    ));
    assert!(!is_dev_server_unavailable_error_message(
        "failed to parse response"
    ));
}

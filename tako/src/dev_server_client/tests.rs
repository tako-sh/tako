use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use tokio::net::UnixStream;

use super::connection::{DEV_SERVER_CONNECTION_CLOSED_MESSAGE, LineClient};
use super::daemon::{
    DEV_SERVER_STARTUP_WAIT_ATTEMPTS, DEV_SERVER_STARTUP_WAIT_INTERVAL_MS,
    format_dev_server_connect_error, format_missing_dev_server_spawn_error,
    read_dev_server_log_tail, repo_local_dev_server_build_args, repo_local_dev_server_build_needed,
    repo_local_dev_server_candidates,
};
use super::events::{DevServerEvent, parse_event_line};

#[test]
fn repo_local_dev_server_candidates_prefers_debug_then_release() {
    let root = std::path::Path::new("/tmp/tako");
    let candidates = repo_local_dev_server_candidates(root);
    assert_eq!(
        candidates[0],
        PathBuf::from("/tmp/tako/target/debug/tako-dev-server")
    );
    assert_eq!(
        candidates[1],
        PathBuf::from("/tmp/tako/target/release/tako-dev-server")
    );
}

#[test]
fn repo_local_dev_server_build_needed_when_binary_missing() {
    assert!(repo_local_dev_server_build_needed(
        Some(SystemTime::now()),
        None
    ));
}

#[test]
fn repo_local_dev_server_build_needed_when_daemon_is_older() {
    let now = SystemTime::now();
    let newer = now;
    let older = now.checked_sub(Duration::from_secs(1)).unwrap();
    assert!(repo_local_dev_server_build_needed(Some(newer), Some(older)));
}

#[test]
fn repo_local_dev_server_build_not_needed_when_daemon_is_newer() {
    let now = SystemTime::now();
    let older = now.checked_sub(Duration::from_secs(1)).unwrap();
    let newer = now;
    assert!(!repo_local_dev_server_build_needed(
        Some(older),
        Some(newer)
    ));
}

#[test]
fn repo_local_dev_server_build_uses_tako_package_binary() {
    assert_eq!(
        repo_local_dev_server_build_args(),
        ["build", "-p", "tako", "--bin", "tako-dev-server"]
    );
}

#[test]
fn daemon_startup_wait_is_15_seconds() {
    assert_eq!(
        (DEV_SERVER_STARTUP_WAIT_ATTEMPTS as u64) * DEV_SERVER_STARTUP_WAIT_INTERVAL_MS,
        15_000
    );
}

#[test]
fn parse_event_line_parses_request_started_event() {
    let line =
        r#"{"type":"Event","event":{"type":"RequestStarted","host":"a.test","path":"/api"}}"#;
    assert_eq!(
        parse_event_line(line),
        Some(DevServerEvent::RequestStarted {
            host: "a.test".to_string(),
            path: "/api".to_string(),
        })
    );
}

#[test]
fn parse_event_line_rejects_request_started_without_path() {
    let line = r#"{"type":"Event","event":{"type":"RequestStarted","host":"a.test"}}"#;
    assert_eq!(parse_event_line(line), None);
}

#[test]
fn missing_daemon_spawn_hint_for_source_checkout_recommends_build() {
    let err = std::io::Error::from(std::io::ErrorKind::NotFound);
    let msg = format_missing_dev_server_spawn_error(true, &err);
    assert!(msg.contains("build it with: cargo build -p tako-cli --bin tako-dev-server"));
    assert!(!msg.contains("Reinstall Tako CLI"));
}

#[test]
fn missing_daemon_spawn_hint_for_installed_cli_recommends_reinstall() {
    let err = std::io::Error::from(std::io::ErrorKind::NotFound);
    let msg = format_missing_dev_server_spawn_error(false, &err);
    assert!(msg.contains("Reinstall Tako CLI and retry"));
    assert!(msg.contains("curl -fsSL https://tako.sh/install.sh | sh"));
    assert!(!msg.contains("build it with: cargo build -p tako-cli --bin tako-dev-server"));
}

#[test]
fn read_dev_server_log_tail_returns_last_lines_only() {
    let tmp = std::env::temp_dir().join(format!(
        "tako-dev-server-log-tail-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&tmp, "l1\nl2\nl3\nl4\n").unwrap();
    let tail = read_dev_server_log_tail(&tmp, 2);
    let _ = std::fs::remove_file(&tmp);
    assert_eq!(tail, "l3\nl4");
}

#[test]
fn format_dev_server_connect_error_includes_log_tail_when_present() {
    let tmp = std::env::temp_dir().join(format!(
        "tako-dev-server-log-error-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&tmp, "boom\n").unwrap();
    let msg = format_dev_server_connect_error(&tmp, None);
    let _ = std::fs::remove_file(&tmp);
    assert!(msg.contains("could not connect to tako-dev-server"));
    assert!(msg.contains("boom"));
}

#[test]
fn format_dev_server_connect_error_without_log_is_brief() {
    let tmp = std::env::temp_dir().join(format!(
        "tako-dev-server-log-missing-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let msg = format_dev_server_connect_error(&tmp, None);
    assert_eq!(msg, "could not connect to tako-dev-server");
}

#[test]
fn format_dev_server_connect_error_without_log_includes_exit_status() {
    let tmp = std::env::temp_dir().join(format!(
        "tako-dev-server-log-status-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let status = std::process::Command::new("sh")
        .args(["-c", "exit 9"])
        .status()
        .unwrap();
    let msg = format_dev_server_connect_error(&tmp, Some(status));
    assert!(msg.contains("daemon exited"));
}

#[tokio::test]
async fn line_client_read_line_errors_when_peer_closes_without_response() {
    let (client_stream, server_stream) = UnixStream::pair().unwrap();
    drop(server_stream);

    let mut client = LineClient::new(client_stream);
    let err = client.read_line().await.unwrap_err();

    assert!(
        err.to_string()
            .contains(DEV_SERVER_CONNECTION_CLOSED_MESSAGE),
        "unexpected error: {err}"
    );
}

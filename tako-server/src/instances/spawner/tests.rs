use super::super::AppConfig;
use super::super::logger::noop_log_handle;
use super::readiness::{
    format_startup_exit_error, format_startup_timeout_error, truncate_chars, wait_for_ready,
};
use super::spawn_command::{
    app_child_parent_death_signal, build_instance_args, build_instance_env, create_bootstrap_pipe,
    spawn_child_process,
};
use super::*;
use crate::instances::INTERNAL_TOKEN_HEADER;
use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::ExitStatus;
use std::time::Duration;
use tokio::sync::mpsc;

#[test]
fn test_spawner_creation() {
    let _spawner = Spawner::new();
    // Just verify it creates without panic
}

#[test]
#[cfg(unix)]
fn resolve_app_user_returns_none_gracefully_for_missing_user() {
    use std::ffi::CString;
    let name = CString::new("this-user-definitely-does-not-exist-tako-test").unwrap();
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    assert!(pw.is_null(), "expected nonexistent user to return null");
    // resolve_app_user looks up "tako-app"; on dev machines it won't exist.
    // Calling Spawner::new() must not panic regardless.
    let _spawner = Spawner::new();
}

#[test]
#[cfg(unix)]
fn spawn_child_process_returns_permission_denied_when_app_user_switch_fails() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        path: dir.path().to_path_buf(),
        command: vec!["sh".to_string(), "-c".to_string(), "exit 0".to_string()],
        ..Default::default()
    };
    let result = spawn_child_process(
        &config,
        &HashMap::new(),
        &[],
        Some((0, 0)),
        "token",
        &HashMap::new(),
        "",
    );

    match result {
        Ok((mut child, _)) => {
            let _ = child.start_kill();
            panic!("spawn unexpectedly retried as the service user");
        }
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied),
    }
}

#[tokio::test]
#[cfg(unix)]
async fn spawn_child_process_does_not_inherit_server_env() {
    let parent_secret = EnvGuard::set("TAKO_SERVER_PARENT_SECRET", "should-not-leak");
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        path: dir.path().to_path_buf(),
        command: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf %s \"${TAKO_SERVER_PARENT_SECRET:-missing}\"".to_string(),
        ],
        ..Default::default()
    };
    let (child, readiness_fd) = spawn_child_process(
        &config,
        &HashMap::new(),
        &[],
        None,
        "token",
        &HashMap::new(),
        "",
    )
    .unwrap();
    drop(readiness_fd);

    let output = child.wait_with_output().await.unwrap();

    drop(parent_secret);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "missing");
}

#[test]
#[cfg(unix)]
fn startup_exit_error_prefers_stderr_and_includes_status() {
    use std::os::unix::process::ExitStatusExt;

    let status = ExitStatus::from_raw(2 << 8);
    let message = format_startup_exit_error(status, b"", b"missing wrapper");
    assert!(message.contains("exit code 2"));
    assert!(message.contains("missing wrapper"));
}

#[test]
#[cfg(unix)]
fn startup_exit_error_uses_stdout_when_stderr_empty() {
    use std::os::unix::process::ExitStatusExt;

    let status = ExitStatus::from_raw(0);
    let message = format_startup_exit_error(status, b"hello", b"");
    assert!(message.contains("hello"));
}

#[test]
fn startup_timeout_error_prefers_stderr() {
    let message = format_startup_timeout_error(Duration::from_millis(500), b"", b"startup boom");
    assert!(message.contains("exceeded 500ms"));
    assert!(message.contains("startup boom"));
}

#[test]
fn truncate_chars_adds_ellipsis_when_over_limit() {
    let text = "a".repeat(405);
    let truncated = truncate_chars(&text, 400);
    assert_eq!(truncated.len(), 403);
    assert!(truncated.ends_with("..."));
}

#[tokio::test]
async fn spawn_timeout_reports_startup_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let app = App::new(
        AppConfig {
            name: "test-app".to_string(),
            path: dir.path().to_path_buf(),
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo startup boom >&2; sleep 2".to_string(),
            ],
            startup_timeout: Duration::from_millis(500),
            ..Default::default()
        },
        instance_tx,
        noop_log_handle(),
    );

    let spawner = Spawner::new();
    let instance = app.allocate_instance();
    let err = spawner.spawn(&app, instance).await.unwrap_err();

    let message = err.to_string();
    assert!(message.contains("Instance startup timeout"));
}

#[test]
#[cfg(target_os = "linux")]
fn app_children_request_sigterm_when_server_dies() {
    assert_eq!(app_child_parent_death_signal(), Some(libc::SIGTERM));
}

#[test]
#[cfg(not(target_os = "linux"))]
fn app_child_parent_death_signal_is_linux_only() {
    assert_eq!(app_child_parent_death_signal(), None);
}

#[test]
#[cfg(unix)]
fn build_instance_env_only_has_app_vars() {
    use std::collections::HashMap;

    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let app = App::new(
        AppConfig {
            name: "test-app".to_string(),
            env_vars: HashMap::from([("FOO".to_string(), "bar".to_string())]),
            secrets: HashMap::from([("SECRET".to_string(), "shh".to_string())]),
            ..Default::default()
        },
        instance_tx,
        noop_log_handle(),
    );
    let instance = app.allocate_instance();
    instance.set_port(48_123);

    let env = build_instance_env(&app.config.read().clone(), &instance, None);
    assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
    assert_eq!(env.get("HOST").map(String::as_str), Some("127.0.0.1"));
    assert!(env.contains_key("PORT"));
    // Secrets + internal token travel on fd 3, not env. Guard the secret
    // case so `secrets.FOO` (from `tako.gen.ts`) can't be accidentally
    // replaced by `process.env.FOO` from a leaked var.
    assert!(!env.contains_key("SECRET"));
}

#[test]
#[cfg(unix)]
fn bootstrap_pipe_envelope_has_token_and_secrets() {
    use std::io::Read;
    use std::os::fd::IntoRawFd;

    let secrets = HashMap::from([
        ("DATABASE_URL".to_string(), "postgres://x".to_string()),
        ("API_KEY".to_string(), "sk-123".to_string()),
    ]);
    let token = "test-token-abc";

    let (read_end, writer) =
        create_bootstrap_pipe(token, &secrets, Some("img-secret")).expect("create pipe");

    let mut buf = String::new();
    let fd = read_end.into_raw_fd();
    // SAFETY: fd was just handed over by into_raw_fd; File::from_raw_fd owns it now.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.read_to_string(&mut buf).expect("read pipe");
    writer.join().expect("writer thread").expect("write ok");

    let parsed: serde_json::Value = serde_json::from_str(&buf).expect("valid JSON");
    assert_eq!(parsed["token"].as_str(), Some(token));
    assert_eq!(
        parsed["secrets"]["DATABASE_URL"].as_str(),
        Some("postgres://x")
    );
    assert_eq!(parsed["secrets"]["API_KEY"].as_str(), Some("sk-123"));
    assert_eq!(parsed["image_secret"].as_str(), Some("img-secret"));
}

#[test]
#[cfg(unix)]
fn bootstrap_pipe_is_created_even_with_empty_secrets() {
    use std::io::Read;
    use std::os::fd::IntoRawFd;

    let secrets: HashMap<String, String> = HashMap::new();
    let token = "still-has-a-token";

    let (read_end, writer) = create_bootstrap_pipe(token, &secrets, None).expect("create pipe");

    let mut buf = String::new();
    let fd = read_end.into_raw_fd();
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.read_to_string(&mut buf).expect("read pipe");
    writer.join().expect("writer thread").expect("write ok");

    let parsed: serde_json::Value = serde_json::from_str(&buf).expect("valid JSON");
    assert_eq!(parsed["token"].as_str(), Some(token));
    assert!(parsed["secrets"].is_object());
    assert_eq!(parsed["secrets"].as_object().unwrap().len(), 0);
}

#[test]
fn build_instance_args_has_instance_only() {
    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let app = App::new(
        AppConfig {
            name: "test-app".to_string(),
            version: "v42".to_string(),
            ..Default::default()
        },
        instance_tx,
        noop_log_handle(),
    );
    let instance = app.allocate_instance();

    let args = build_instance_args(&instance);
    assert!(args.contains(&"--instance".to_string()));
    assert!(args.contains(&instance.id));
    assert_eq!(args.len(), 2);
}

#[test]
fn build_instance_env_sets_port_zero_and_host_loopback() {
    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let app = App::new(
        AppConfig {
            name: "test-app".to_string(),
            ..Default::default()
        },
        instance_tx,
        noop_log_handle(),
    );
    let instance = app.allocate_instance();

    let env = build_instance_env(&app.config.read().clone(), &instance, None);
    assert_eq!(env.get("PORT").map(String::as_str), Some("0"));
    assert_eq!(env.get("HOST").map(String::as_str), Some("127.0.0.1"));
}

#[test]
fn build_instance_env_overwrites_user_host_with_loopback() {
    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let app = App::new(
        AppConfig {
            name: "test-app".to_string(),
            env_vars: HashMap::from([("HOST".to_string(), "0.0.0.0".to_string())]),
            ..Default::default()
        },
        instance_tx,
        noop_log_handle(),
    );
    let instance = app.allocate_instance();

    let env = build_instance_env(&app.config.read().clone(), &instance, None);
    assert_eq!(env.get("HOST").map(String::as_str), Some("127.0.0.1"));
}

#[test]
fn build_instance_env_sets_tako_runtime_vars_when_socket_available() {
    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let app = App::new(
        AppConfig {
            name: "my-app".to_string(),
            ..Default::default()
        },
        instance_tx,
        noop_log_handle(),
    );
    let instance = app.allocate_instance();
    let sock = std::path::Path::new("/tmp/tako.sock");

    let env = build_instance_env(&app.config.read().clone(), &instance, Some(sock));
    assert_eq!(
        env.get("TAKO_INTERNAL_SOCKET").map(String::as_str),
        Some("/tmp/tako.sock"),
    );
    assert!(
        env.get("TAKO_APP_NAME")
            .map(|v| !v.is_empty())
            .unwrap_or(false),
        "TAKO_APP_NAME must be set whenever the app is spawned"
    );
}

#[test]
fn build_instance_env_always_sets_app_name_even_without_socket() {
    // Apps may run with no internal socket in tests, but the app name
    // is always a known, required identity — set it regardless so
    // any tooling that reads `TAKO_APP_NAME` gets a valid value.
    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let app = App::new(
        AppConfig {
            name: "my-app".to_string(),
            ..Default::default()
        },
        instance_tx,
        noop_log_handle(),
    );
    let instance = app.allocate_instance();

    let env = build_instance_env(&app.config.read().clone(), &instance, None);
    assert!(
        env.get("TAKO_APP_NAME")
            .map(|v| !v.is_empty())
            .unwrap_or(false),
    );
    assert!(!env.contains_key("TAKO_INTERNAL_SOCKET"));
}

#[test]
fn build_instance_args_never_includes_socket_flag() {
    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let app = App::new(
        AppConfig {
            name: "test-app".to_string(),
            version: "v42".to_string(),
            ..Default::default()
        },
        instance_tx,
        noop_log_handle(),
    );
    let instance = app.allocate_instance();

    let args = build_instance_args(&instance);
    assert!(!args.contains(&"--socket".to_string()));
    assert!(args.contains(&"--instance".to_string()));
    assert_eq!(args.len(), 2);
}

#[cfg(unix)]
struct EnvGuard {
    name: &'static str,
    previous: Option<std::ffi::OsString>,
}

#[cfg(unix)]
impl EnvGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(name);
        unsafe { std::env::set_var(name, value) };
        Self { name, previous }
    }
}

#[cfg(unix)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(value) = self.previous.take() {
            unsafe { std::env::set_var(self.name, value) };
        } else {
            unsafe { std::env::remove_var(self.name) };
        }
    }
}

#[tokio::test]
async fn health_check_requires_matching_internal_token() {
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        return;
    };
    let port = listener.local_addr().expect("listener addr").port();
    let token = "spawner-health-token".to_string();
    let closure_token = token.clone();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request_buf = [0_u8; 2048];
        let n = tokio::io::AsyncReadExt::read(&mut socket, &mut request_buf)
            .await
            .expect("read request");
        let request = String::from_utf8_lossy(&request_buf[..n]);
        let is_internal_status = request.starts_with("GET /status ")
            && request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("host: tako"));
        let has_token = request.lines().any(|line| {
            line.eq_ignore_ascii_case(&format!("{INTERNAL_TOKEN_HEADER}: {closure_token}"))
        });

        let response = if is_internal_status && has_token {
            format!(
                "HTTP/1.1 200 OK\r\n{INTERNAL_TOKEN_HEADER}: {closure_token}\r\nContent-Length: 2\r\n\r\nok"
            )
        } else {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found".to_string()
        };

        let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
    });

    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let config = AppConfig {
        name: "test-app".to_string(),
        health_check_path: "/status".to_string(),
        health_check_host: "tako".to_string(),
        ..Default::default()
    };
    let app = App::new(config, instance_tx, noop_log_handle());
    let instance = app.allocate_instance();
    instance.set_port(port);
    let token_field = instance.internal_token().to_string();
    assert_ne!(token_field, token, "test should use the instance token");

    let spawner = Spawner::new();
    assert!(
        !spawner.health_check(&app, &instance).await,
        "mismatched token must fail"
    );
}

#[tokio::test]
async fn health_check_uses_loopback_tcp_with_matching_internal_token() {
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        return;
    };
    let port = listener.local_addr().expect("listener addr").port();

    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let config = AppConfig {
        name: "test-app".to_string(),
        health_check_path: "/status".to_string(),
        health_check_host: "tako".to_string(),
        ..Default::default()
    };
    let app = App::new(config, instance_tx, noop_log_handle());
    let instance = app.allocate_instance();
    instance.set_port(port);
    let token = instance.internal_token().to_string();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request_buf = [0_u8; 2048];
        let n = tokio::io::AsyncReadExt::read(&mut socket, &mut request_buf)
            .await
            .expect("read request");
        let request = String::from_utf8_lossy(&request_buf[..n]);
        let is_internal_status = request.starts_with("GET /status ")
            && request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("host: tako"));
        let has_token = request
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&format!("{INTERNAL_TOKEN_HEADER}: {token}")));

        let response = if is_internal_status && has_token {
            format!(
                "HTTP/1.1 200 OK\r\n{INTERNAL_TOKEN_HEADER}: {token}\r\nContent-Length: 2\r\n\r\nok"
            )
        } else {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found".to_string()
        };

        let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
    });

    let spawner = Spawner::new();
    assert!(spawner.health_check(&app, &instance).await);
}

#[tokio::test]
async fn health_check_reads_response_headers_across_multiple_chunks() {
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        return;
    };
    let port = listener.local_addr().expect("listener addr").port();

    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let config = AppConfig {
        name: "test-app".to_string(),
        health_check_path: "/status".to_string(),
        health_check_host: "tako".to_string(),
        ..Default::default()
    };
    let app = App::new(config, instance_tx, noop_log_handle());
    let instance = app.allocate_instance();
    instance.set_port(port);
    let token = instance.internal_token().to_string();

    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request_buf = [0_u8; 2048];
        let n = tokio::io::AsyncReadExt::read(&mut socket, &mut request_buf)
            .await
            .expect("read request");
        let request = String::from_utf8_lossy(&request_buf[..n]);
        let is_internal_status = request.starts_with("GET /status ")
            && request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("host: tako"));
        let has_token = request
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&format!("{INTERNAL_TOKEN_HEADER}: {token}")));

        if is_internal_status && has_token {
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nX-Tako-Internal-Token: ")
                .await
                .expect("write response prefix");
            tokio::time::sleep(Duration::from_millis(10)).await;
            socket
                .write_all(format!("{token}\r\nContent-Length: 2\r\n\r\nok").as_bytes())
                .await
                .expect("write response suffix");
        } else {
            socket
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found")
                .await
                .expect("write not found");
        }
    });

    let spawner = Spawner::new();
    assert!(spawner.health_check(&app, &instance).await);
}

#[tokio::test]
#[cfg(unix)]
async fn wait_for_ready_reads_port_from_fd4_pipe() {
    use std::io::Write;

    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);

    let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let app = App::new(
        AppConfig {
            name: "test-app".to_string(),
            ..Default::default()
        },
        instance_tx,
        noop_log_handle(),
    );
    let instance = app.allocate_instance();
    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn test child");
    instance.set_process(child);

    tokio::task::spawn_blocking(move || {
        let mut writer = std::fs::File::from(write_end);
        writer.write_all(b"43123\n").unwrap();
    })
    .await
    .unwrap();

    wait_for_ready(instance.clone(), Some(read_end))
        .await
        .unwrap();

    assert_eq!(instance.port(), Some(43123));
    assert_eq!(instance.state(), InstanceState::Ready);
    let _ = instance.kill().await;
}

#[tokio::test]
#[cfg(unix)]
async fn wait_for_ready_rejects_invalid_fd4_payload() {
    use std::io::Write;

    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);

    let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    let (instance_tx, _instance_rx) = mpsc::channel(4);
    let app = App::new(
        AppConfig {
            name: "test-app".to_string(),
            ..Default::default()
        },
        instance_tx,
        noop_log_handle(),
    );
    let instance = app.allocate_instance();
    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn test child");
    instance.set_process(child);

    tokio::task::spawn_blocking(move || {
        let mut writer = std::fs::File::from(write_end);
        writer.write_all(b"not-a-port\n").unwrap();
    })
    .await
    .unwrap();

    let err = wait_for_ready(instance.clone(), Some(read_end))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid port"));
    let _ = instance.kill().await;
}

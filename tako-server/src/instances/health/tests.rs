use super::*;
use crate::instances::logger::noop_log_handle;
use crate::instances::{AppConfig, AppLaunch};
use tokio::sync::mpsc;

fn create_test_app() -> Arc<App> {
    let (tx, _rx) = mpsc::channel(16);
    let config = AppConfig {
        name: "test-app".to_string(),
        ..Default::default()
    };
    Arc::new(App::new(config, tx, noop_log_handle()))
}

#[test]
fn test_health_config_defaults() {
    let config = HealthConfig::default();
    assert_eq!(
        config.check_interval,
        crate::defaults::HEALTH_CHECK_INTERVAL
    );
    assert_eq!(
        config.startup_check_interval,
        crate::defaults::HEALTH_STARTUP_CHECK_INTERVAL
    );
    assert!(
        config.startup_check_interval < config.check_interval,
        "startup probe must be faster than steady-state"
    );
    assert_eq!(config.unhealthy_threshold, 2);
    assert_eq!(config.dead_threshold, 3);
    assert_eq!(config.probe_timeout, crate::defaults::HEALTH_PROBE_TIMEOUT);
    assert_eq!(config.max_probe_concurrency, 16);
}

#[test]
fn test_app_has_starting_instance_detects_startup_states() {
    let app = create_test_app();
    let instance = app.allocate_instance();

    instance.set_state(InstanceState::Starting);
    assert!(app_has_starting_instance(&app));

    instance.set_state(InstanceState::Ready);
    assert!(app_has_starting_instance(&app));

    instance.set_state(InstanceState::Healthy);
    assert!(!app_has_starting_instance(&app));

    instance.set_state(InstanceState::Unhealthy);
    assert!(!app_has_starting_instance(&app));
}

#[test]
fn test_effective_probe_concurrency_never_zero() {
    assert_eq!(HealthChecker::effective_probe_concurrency(0), 1);
    assert_eq!(HealthChecker::effective_probe_concurrency(7), 7);
}

#[tokio::test]
async fn test_health_checker_creation() {
    let (tx, _rx) = mpsc::channel(16);
    let config = HealthConfig::default();
    let checker = HealthChecker::new(config, tx);

    // Verify failure counts start empty
    assert_eq!(checker.get_failure_count("test-app", "1"), 0);
}

#[tokio::test]
async fn test_health_checker_failure_tracking() {
    let (tx, _rx) = mpsc::channel(16);
    let config = HealthConfig::default();
    let checker = HealthChecker::new(config, tx);

    // Simulate failure count increment (this would normally happen in check_instance)
    let key = "test-app:1".to_string();
    checker.failure_counts.insert(key.clone(), 3);

    assert_eq!(checker.get_failure_count("test-app", "1"), 3);

    // Clear and verify
    checker.clear_failure_count("test-app", "1");
    assert_eq!(checker.get_failure_count("test-app", "1"), 0);
}

#[tokio::test]
async fn test_health_checker_skips_non_running_instances() {
    let (tx, mut rx) = mpsc::channel(16);
    let config = HealthConfig::default();
    let checker = HealthChecker::new(config, tx);

    let app = create_test_app();
    let instance = app.allocate_instance();

    // Instance in Starting state should be skipped
    instance.set_state(InstanceState::Starting);
    checker.check_instance(&app, &instance).await;

    // No events should be emitted
    assert!(rx.try_recv().is_err());

    // Instance in Draining state should be skipped
    instance.set_state(InstanceState::Draining);
    checker.check_instance(&app, &instance).await;
    assert!(rx.try_recv().is_err());
}

#[test]
fn test_health_event_types() {
    let healthy = HealthEvent::Healthy {
        app: "test".to_string(),
        instance_id: "abc123".to_string(),
    };
    let unhealthy = HealthEvent::Unhealthy {
        app: "test".to_string(),
        instance_id: "abc123".to_string(),
    };
    let dead = HealthEvent::Dead {
        app: "test".to_string(),
        instance_id: "abc123".to_string(),
    };
    let recovered = HealthEvent::Recovered {
        app: "test".to_string(),
        instance_id: "abc123".to_string(),
    };

    // Just verify they can be created and formatted
    assert!(format!("{:?}", healthy).contains("Healthy"));
    assert!(format!("{:?}", unhealthy).contains("Unhealthy"));
    assert!(format!("{:?}", dead).contains("Dead"));
    assert!(format!("{:?}", recovered).contains("Recovered"));
}

#[tokio::test]
async fn test_probe_uses_tcp_when_port_is_configured() {
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        return;
    };
    let port = listener.local_addr().expect("listener addr").port();

    let (tx, _rx) = mpsc::channel(16);
    let config = AppConfig {
        name: "test-app".to_string(),
        min_instances: 1,
        ..Default::default()
    };
    let app = App::new(config, tx, noop_log_handle());
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

    let healthy = probe_instance_health(
        &instance,
        "tako",
        "/status",
        true,
        Duration::from_millis(200),
    )
    .await;
    assert!(healthy.is_ok());
}

#[tokio::test]
async fn test_probe_reads_split_response_headers() {
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        return;
    };
    let port = listener.local_addr().expect("listener addr").port();

    let (tx, _rx) = mpsc::channel(16);
    let config = AppConfig {
        name: "test-app".to_string(),
        min_instances: 1,
        ..Default::default()
    };
    let app = App::new(config, tx, noop_log_handle());
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

    let healthy = probe_instance_health(
        &instance,
        "tako",
        "/status",
        true,
        Duration::from_millis(200),
    )
    .await;
    assert!(healthy.is_ok());
}

#[tokio::test]
async fn test_probe_reports_connect_failure_reason() {
    let (tx, _rx) = mpsc::channel(16);
    let app = App::new(AppConfig::default(), tx, noop_log_handle());
    let instance = app.allocate_instance();
    instance.set_port(0);

    let failure = probe_instance_health(
        &instance,
        "tako",
        "/status",
        true,
        Duration::from_millis(200),
    )
    .await
    .expect_err("closed port should fail");

    assert_eq!(failure.reason, "connect_failed");
    assert!(!failure.detail.is_empty());
}

#[tokio::test]
async fn test_probe_reports_missing_internal_token_reason() {
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        return;
    };
    let port = listener.local_addr().expect("listener addr").port();

    let (tx, _rx) = mpsc::channel(16);
    let app = App::new(AppConfig::default(), tx, noop_log_handle());
    let instance = app.allocate_instance();
    instance.set_port(port);

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request_buf = [0_u8; 2048];
        let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut request_buf).await;
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
    });

    let failure = probe_instance_health(
        &instance,
        "tako",
        "/status",
        true,
        Duration::from_millis(200),
    )
    .await
    .expect_err("response without echoed token should fail");

    assert_eq!(failure.reason, "missing_internal_token");
}

#[tokio::test]
async fn test_container_probe_requires_internal_token() {
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        return;
    };
    let port = listener.local_addr().expect("listener addr").port();

    let (tx, _rx) = mpsc::channel(16);
    let app = App::new(
        AppConfig {
            launch: AppLaunch::Container {
                image: "tako/my-app:v1".to_string(),
                port: 3000,
            },
            ..Default::default()
        },
        tx,
        noop_log_handle(),
    );
    let instance = app.allocate_instance();
    instance.set_port(port);

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request_buf = [0_u8; 2048];
        let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut request_buf).await;
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
    });

    let failure = probe_instance_health(
        &instance,
        "tako",
        "/status",
        true,
        Duration::from_millis(200),
    )
    .await;
    let failure = failure.expect_err("plain container status must not satisfy SDK health probe");
    assert_eq!(failure.reason, "missing_internal_token");
}

#[tokio::test]
async fn test_check_instance_detects_process_exit() {
    let (tx, mut rx) = mpsc::channel(16);
    let config = HealthConfig::default();
    let checker = HealthChecker::new(config, tx);

    let (app_tx, _app_rx) = mpsc::channel(16);
    let app_config = AppConfig {
        name: "test-app".to_string(),
        ..Default::default()
    };
    let app = Arc::new(App::new(app_config, app_tx, noop_log_handle()));
    let instance = app.allocate_instance();

    // Spawn a process that exits immediately.
    let child = tokio::process::Command::new("true")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    instance.set_process(child);
    instance.set_state(InstanceState::Healthy);

    // Wait for the process to actually exit.
    tokio::time::sleep(Duration::from_millis(100)).await;

    checker.check_instance(&app, &instance).await;

    // Should emit Dead event (process exited).
    let event = rx.try_recv().expect("should emit event");
    assert!(matches!(event, HealthEvent::Dead { .. }));
    assert_eq!(instance.state(), InstanceState::Stopped);
}

fn unused_loopback_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

#[tokio::test]
async fn test_single_probe_failure_keeps_instance_serving() {
    let (tx, mut rx) = mpsc::channel(16);
    let config = HealthConfig::default();
    let checker = HealthChecker::new(config, tx);

    let (app_tx, _app_rx) = mpsc::channel(16);
    let app_config = AppConfig {
        name: "test-app".to_string(),
        ..Default::default()
    };
    let app = Arc::new(App::new(app_config, app_tx, noop_log_handle()));
    let instance = app.allocate_instance();

    // Set instance as Healthy with a port nobody is listening on.
    instance.set_port(unused_loopback_port());
    instance.set_state(InstanceState::Healthy);

    // Spawn a long-running process so is_alive() returns true, forcing
    // the probe path (which will fail because nothing listens on 19999).
    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    instance.set_process(child);

    checker.check_instance(&app, &instance).await;

    assert!(rx.try_recv().is_err());
    assert_eq!(instance.state(), InstanceState::Healthy);
    assert_eq!(checker.get_failure_count(&app.name(), &instance.id), 1);

    // Clean up.
    let _ = instance.kill().await;
}

#[tokio::test]
async fn suppressed_rollout_instance_fails_on_first_probe_failure() {
    let (tx, mut rx) = mpsc::channel(16);
    let config = HealthConfig::default();
    let checker = HealthChecker::new(config, tx);

    let (app_tx, _app_rx) = mpsc::channel(16);
    let app_config = AppConfig {
        name: "test-app".to_string(),
        ..Default::default()
    };
    let app = Arc::new(App::new(app_config, app_tx, noop_log_handle()));
    let instance = app.allocate_instance();

    instance.set_port(unused_loopback_port());
    app.suppress_instance_routing(&instance.id);
    instance.set_state(InstanceState::Healthy);

    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    instance.set_process(child);

    checker.check_instance(&app, &instance).await;

    assert!(matches!(
        rx.try_recv()
            .expect("first suppressed rollout failure should emit unhealthy"),
        HealthEvent::Unhealthy { .. }
    ));
    assert_eq!(instance.state(), InstanceState::Unhealthy);
    assert_eq!(checker.get_failure_count(&app.name(), &instance.id), 1);

    let _ = instance.kill().await;
}

#[tokio::test]
async fn test_three_probe_failures_trigger_dead() {
    let (tx, mut rx) = mpsc::channel(16);
    let config = HealthConfig::default();
    let checker = HealthChecker::new(config, tx);

    let (app_tx, _app_rx) = mpsc::channel(16);
    let app_config = AppConfig {
        name: "test-app".to_string(),
        ..Default::default()
    };
    let app = Arc::new(App::new(app_config, app_tx, noop_log_handle()));
    let instance = app.allocate_instance();

    instance.set_port(unused_loopback_port());
    instance.set_state(InstanceState::Healthy);

    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    instance.set_process(child);

    checker.check_instance(&app, &instance).await;
    assert!(rx.try_recv().is_err());
    assert_eq!(instance.state(), InstanceState::Healthy);

    checker.check_instance(&app, &instance).await;
    assert!(matches!(
        rx.try_recv().expect("second failure should emit unhealthy"),
        HealthEvent::Unhealthy { .. }
    ));
    assert_eq!(instance.state(), InstanceState::Unhealthy);

    checker.check_instance(&app, &instance).await;
    assert!(matches!(
        rx.try_recv().expect("third failure should emit dead"),
        HealthEvent::Dead { .. }
    ));
    assert_eq!(instance.state(), InstanceState::Stopped);

    let _ = instance.kill().await;
}

use super::*;
use crate::control::EventsHub;
use crate::proxy::Routes;
use crate::state::RuntimeApp;
use tokio::sync::watch;

#[cfg(unix)]
#[tokio::test]
async fn kill_and_reap_app_process_waits_for_killed_child() {
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg("sleep 60").process_group(0);
    let mut child = command.spawn().unwrap();
    let pid = child.id().unwrap();

    let status = tokio::time::timeout(
        Duration::from_secs(2),
        kill_and_reap_app_process(&mut child, Some(pid)),
    )
    .await
    .expect("kill and reap should not hang");

    assert!(status.is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn readiness_times_out_when_route_becomes_active_without_fd4_signal() {
    let routes = Routes::default();
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);
    let state = Arc::new(Mutex::new(State::new(
        shutdown_tx,
        routes.clone(),
        EventsHub::default(),
        true,
        53535,
        8443,
        "127.0.0.1:8443".to_string(),
        "127.0.0.1".to_string(),
    )));
    let config_path = "/tmp/tako-readiness/tako.toml".to_string();
    let route_id = format!("reg:{config_path}");

    {
        let mut s = state.lock().unwrap();
        s.apps.insert(
            config_path.clone(),
            RuntimeApp {
                project_dir: "/tmp/tako-readiness".to_string(),
                name: "readiness".to_string(),
                variant: None,
                hosts: vec!["readiness.test".to_string()],
                upstream_port: 0,
                is_idle: false,
                command: vec!["node".to_string()],
                worker_command: None,
                env: std::collections::HashMap::new(),
                log_buffer: crate::state::LogBuffer::new(),
                pid: None,
                client_pid: None,
                tunnel: None,
                readiness_failure_hint: Some("custom readiness hint".to_string()),
                bootstrap_token: "dev-token".to_string(),
                secrets: std::collections::HashMap::new(),
                storages: std::collections::HashMap::new(),
            },
        );
        s.routes.set_routes(
            route_id.clone(),
            vec!["readiness.test".to_string()],
            0,
            false,
        );
    }

    let (read_fd, _write_fd) = create_readiness_pipe().unwrap();
    let routes_for_task = routes.clone();
    let route_for_task = route_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        routes_for_task.activate_with_port(&route_for_task, 4321);
    });
    let app = {
        let s = state.lock().unwrap();
        s.apps.get(&config_path).unwrap().clone()
    };

    tokio::time::timeout(
        Duration::from_millis(200),
        activate_after_readiness(&state, &config_path, &route_id, &app, Some(read_fd)),
    )
    .await
    .expect_err("route activation must not replace fd 4 readiness");
}

#[cfg(unix)]
#[tokio::test]
async fn spawn_app_exposes_bootstrap_envelope_on_fd3() {
    let tmp = tempfile::tempdir().unwrap();
    let bootstrap_out = tmp.path().join("bootstrap.json");
    let mut env = std::collections::HashMap::new();
    env.insert(
        "BOOTSTRAP_OUT".to_string(),
        bootstrap_out.display().to_string(),
    );
    let secrets = std::collections::HashMap::from([(
        "DATABASE_URL".to_string(),
        "postgres://localhost/dev".to_string(),
    )]);
    let app = RuntimeApp {
        project_dir: tmp.path().display().to_string(),
        name: "bootstrap-test".to_string(),
        variant: None,
        hosts: vec!["bootstrap-test.test".to_string()],
        upstream_port: 0,
        is_idle: false,
        command: vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat <&3 > \"$BOOTSTRAP_OUT\"; printf '54321\\n' >&4".to_string(),
        ],
        worker_command: None,
        env,
        log_buffer: crate::state::LogBuffer::new(),
        pid: None,
        client_pid: None,
        tunnel: None,
        readiness_failure_hint: None,
        bootstrap_token: "dev-token".to_string(),
        secrets: secrets.clone(),
        storages: std::collections::HashMap::new(),
    };

    let (mut child, readiness_fd) = spawn_app(&app.project_dir, &app, None).await.unwrap();
    let port = wait_for_readiness(readiness_fd.expect("readiness fd")).await;
    assert_eq!(port, Some(54321));
    let status = child.wait().await.unwrap();
    assert!(status.success());

    let raw = std::fs::read_to_string(bootstrap_out).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["token"], "dev-token");
    assert_eq!(
        parsed["secrets"]["DATABASE_URL"].as_str(),
        Some("postgres://localhost/dev")
    );
    assert_eq!(parsed["storages"], serde_json::json!({}));
}

#[test]
fn readiness_failure_message_uses_client_hint() {
    let app = RuntimeApp {
        project_dir: "/tmp/tako-readiness".to_string(),
        name: "readiness".to_string(),
        variant: None,
        hosts: vec!["readiness.test".to_string()],
        upstream_port: 0,
        is_idle: false,
        command: vec!["node".to_string()],
        worker_command: None,
        env: std::collections::HashMap::new(),
        log_buffer: crate::state::LogBuffer::new(),
        pid: None,
        client_pid: None,
        tunnel: None,
        readiness_failure_hint: Some("custom readiness hint".to_string()),
        bootstrap_token: "dev-token".to_string(),
        secrets: std::collections::HashMap::new(),
        storages: std::collections::HashMap::new(),
    };

    assert_eq!(readiness_failure_message(&app), "custom readiness hint");
}

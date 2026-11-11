use super::*;
use crate::control::EventsHub;
use crate::proxy::Routes;
use crate::state::RuntimeApp;
use tokio::sync::watch;

fn idle_wake_app(directory: &std::path::Path) -> (Arc<Mutex<State>>, String) {
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);
    let state = Arc::new(Mutex::new(State::new(
        shutdown_tx,
        Routes::default(),
        EventsHub::default(),
        true,
        53535,
        8443,
        "127.0.0.1:8443".to_string(),
        "127.0.0.1".to_string(),
    )));
    let config_path = directory.join("tako.toml").display().to_string();
    {
        let mut s = state.lock().unwrap();
        s.apps.insert(
            config_path.clone(),
            RuntimeApp {
                compose: None,
                project_dir: directory.display().to_string(),
                name: "wake".to_string(),
                variant: None,
                hosts: vec!["wake.test".to_string()],
                upstream_port: 0,
                is_idle: true,
                command: vec![directory.join("missing-command").display().to_string()],
                worker_command: None,
                workflow_retention_ms: Some(tako_workflows::DEFAULT_RETENTION_MS),
                env: std::collections::HashMap::new(),
                log_buffer: state::LogBuffer::new(),
                pid: None,
                client_pid: None,
                tunnel: None,
                readiness_failure_hint: None,
                bootstrap_token: "dev-token".to_string(),
                secrets: std::collections::HashMap::new(),
                storages: std::collections::HashMap::new(),
            },
        );
        s.routes.set_routes(
            format!("reg:{config_path}"),
            vec!["wake.test".into()],
            0,
            false,
        );
    }
    (state, config_path)
}

#[cfg(unix)]
#[tokio::test]
async fn private_wake_uses_registration_without_a_matching_public_route() {
    let directory = tempfile::tempdir().unwrap();
    let (state, config_path) = idle_wake_app(directory.path());
    {
        let mut s = state.lock().unwrap();
        let app = s.apps.get_mut(&config_path).unwrap();
        app.hosts = vec!["wake.test/api/*".into()];
        app.command = vec![
            "sh".into(),
            "-c".into(),
            "cat <&3 > /dev/null; printf '54321\\n' >&4; exec sleep 60".into(),
        ];
        s.routes.set_routes(
            format!("reg:{config_path}"),
            vec!["wake.test/api/*".into()],
            0,
            false,
        );
    }
    wake_registration(state.clone(), &config_path, None).await;
    let active = state
        .lock()
        .unwrap()
        .routes
        .lookup("wake.test", "/api/items")
        .unwrap()
        .active;
    kill_all_app_processes(&state);
    assert!(active);
}

#[cfg(unix)]
#[tokio::test]
async fn failed_wake_can_retry_and_concurrent_requests_spawn_once() {
    let directory = tempfile::tempdir().unwrap();
    let (state, config_path) = idle_wake_app(directory.path());
    handle_wake_on_request(state.clone(), "wake.test".into(), "/".into()).await;
    assert!(state.lock().unwrap().apps[&config_path].is_idle);
    state
        .lock()
        .unwrap()
        .apps
        .get_mut(&config_path)
        .unwrap()
        .command = vec![
        "sh".into(),
        "-c".into(),
        "cat <&3 > /dev/null; echo started >> starts; printf '54321\\n' >&4; exec sleep 60".into(),
    ];
    tokio::join!(
        handle_wake_on_request(state.clone(), "wake.test".into(), "/".into()),
        handle_wake_on_request(state.clone(), "wake.test".into(), "/".into()),
    );
    let (active, pid, idle) = {
        let s = state.lock().unwrap();
        (
            s.routes.lookup("wake.test", "/").unwrap().active,
            s.apps[&config_path].pid,
            s.apps[&config_path].is_idle,
        )
    };
    kill_all_app_processes(&state);
    assert!(active);
    assert!(pid.is_some());
    assert!(!idle);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("starts")).unwrap(),
        "started\n"
    );
}

#[tokio::test]
async fn wake_rechecks_registration_after_waiting_for_lifecycle_lock() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let directory = tempfile::tempdir().unwrap();
    let (state, config_path) = idle_wake_app(directory.path());
    let lock = crate::compose::app_lock(&state, &config_path);
    let guard = lock.lock().await;
    let mut waking = Box::pin(handle_wake_on_request(
        state.clone(),
        "wake.test".into(),
        "/".into(),
    ));
    assert!(
        poll_fn(|cx| Poll::Ready(waking.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    {
        let mut s = state.lock().unwrap();
        s.apps.remove(&config_path);
        s.routes.remove_app(&format!("reg:{config_path}"));
    }
    drop(guard);
    waking.await;
    assert!(!state.lock().unwrap().apps.contains_key(&config_path));
}

#[cfg(unix)]
#[tokio::test]
async fn wake_uses_the_same_path_route_as_the_proxy() {
    let directory = tempfile::tempdir().unwrap();
    let (state, config_path) = idle_wake_app(directory.path());
    let api_config = directory.path().join("api.toml").display().to_string();
    {
        let mut s = state.lock().unwrap();
        let mut api = s.apps[&config_path].clone();
        api.name = "api".into();
        api.hosts = vec!["wake.test/api/*".into()];
        api.command = vec![
            "sh".into(),
            "-c".into(),
            "cat <&3 > /dev/null; printf '54321\\n' >&4; exec sleep 60".into(),
        ];
        s.routes
            .set_routes(format!("reg:{api_config}"), api.hosts.clone(), 0, false);
        s.apps.insert(api_config.clone(), api);
    }
    handle_wake_on_request(state.clone(), "wake.test".into(), "/api/items".into()).await;
    let (api_active, root_active, root_idle) = {
        let s = state.lock().unwrap();
        (
            s.routes.lookup("wake.test", "/api/items").unwrap().active,
            s.routes.lookup("wake.test", "/").unwrap().active,
            s.apps[&config_path].is_idle,
        )
    };
    kill_all_app_processes(&state);
    assert!(api_active);
    assert!(!root_active);
    assert!(root_idle);
}

#[tokio::test]
async fn child_output_drains_both_pipes_into_the_log_buffer() {
    let mut child = tokio::process::Command::new("sh")
        .args(["-c", "i=0; while [ $i -lt 10000 ]; do echo 'web | output'; echo 'redis | diagnostic' >&2; i=$((i + 1)); done"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let buffer = state::LogBuffer::new();
    let (_, mut logs, _) = buffer.subscribe(None);
    forward_child_pipes(&mut child, &buffer);
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut info = 0;
        let mut warn = 0;
        let mut skipped = 0;
        while info + warn + skipped < 20000 {
            let entry = match logs.recv().await {
                Ok(entry) => entry,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    skipped += count;
                    continue;
                }
                Err(error) => panic!("log stream closed: {error}"),
            };
            let value: serde_json::Value = serde_json::from_str(&entry.line).unwrap();
            match value["level"].as_str().unwrap() {
                "info" => {
                    assert_eq!(value["msg"], "web | output");
                    info += 1;
                }
                "warn" => {
                    assert_eq!(value["msg"], "redis | diagnostic");
                    warn += 1;
                }
                level => panic!("unexpected level: {level}"),
            }
        }
        assert_eq!(info + warn + skipped, 20000);
        assert!(info <= 10000 && warn <= 10000);
        assert!(child.wait().await.unwrap().success());
    })
    .await
    .expect("both pipes must drain without blocking the child");
}

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
async fn readiness_keeps_waiting_when_route_becomes_active_without_fd4_signal() {
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
                compose: None,
                project_dir: "/tmp/tako-readiness".to_string(),
                name: "readiness".to_string(),
                variant: None,
                hosts: vec!["readiness.test".to_string()],
                upstream_port: 0,
                is_idle: false,
                command: vec!["node".to_string()],
                worker_command: None,
                workflow_retention_ms: Some(tako_workflows::DEFAULT_RETENTION_MS),
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
#[tokio::test(start_paused = true)]
async fn slow_startup_logs_elapsed_time_until_readiness_without_failing() {
    use std::future::{Future, poll_fn};
    use std::io::Write;
    use std::task::Poll;

    let directory = tempfile::tempdir().unwrap();
    let (state, config_path) = idle_wake_app(directory.path());
    let route_id = format!("reg:{config_path}");
    let app = state.lock().unwrap().apps[&config_path].clone();
    let (_, mut logs, _) = app.log_buffer.subscribe(None);
    let (read_fd, write_fd) = create_readiness_pipe().unwrap();
    let mut waiting = Box::pin(activate_after_readiness(
        &state,
        &config_path,
        &route_id,
        &app,
        Some(read_fd),
    ));
    assert!(
        poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert!(
        logs.try_recv().is_err(),
        "no notice before the first 30 seconds"
    );

    for elapsed in (30..=180).step_by(30) {
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(
            poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        let entry = logs.try_recv().expect("one notice every 30 seconds");
        let log: serde_json::Value = serde_json::from_str(&entry.line).unwrap();
        assert_eq!(log["level"], "info");
        assert_eq!(log["scope"], "tako");
        assert_eq!(
            log["msg"],
            format!("App has not started after {elapsed}s; still waiting…")
        );
        assert!(logs.try_recv().is_err(), "each notice is logged once");
        assert!(
            !state
                .lock()
                .unwrap()
                .routes
                .lookup("wake.test", "/")
                .unwrap()
                .active
        );
    }

    writeln!(std::fs::File::from(write_fd), "54321").unwrap();
    waiting.await.unwrap();
    assert!(
        state
            .lock()
            .unwrap()
            .routes
            .lookup("wake.test", "/")
            .unwrap()
            .active
    );
    assert_eq!(
        state.lock().unwrap().apps[&config_path].upstream_port,
        54321
    );
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(logs.try_recv().is_err(), "notices stop after readiness");
}

#[cfg(unix)]
#[tokio::test]
async fn closed_or_invalid_readiness_pipe_is_a_real_startup_failure() {
    use std::io::Write;

    let directory = tempfile::tempdir().unwrap();
    let (state, config_path) = idle_wake_app(directory.path());
    let app = state.lock().unwrap().apps[&config_path].clone();
    for signal in ["", "not a port\n", "0\n"] {
        let (read_fd, write_fd) = create_readiness_pipe().unwrap();
        std::fs::File::from(write_fd)
            .write_all(signal.as_bytes())
            .unwrap();
        let error = activate_after_readiness(
            &state,
            &config_path,
            &format!("reg:{config_path}"),
            &app,
            Some(read_fd),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            "app closed fd 4 without signaling a valid readiness port"
        );
        assert!(
            !state
                .lock()
                .unwrap()
                .routes
                .lookup("wake.test", "/")
                .unwrap()
                .active
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn startup_detects_exit_even_when_a_descendant_keeps_readiness_open() {
    let directory = tempfile::tempdir().unwrap();
    let (state, config_path) = idle_wake_app(directory.path());
    state
        .lock()
        .unwrap()
        .apps
        .get_mut(&config_path)
        .unwrap()
        .command = vec![
        "sh".into(),
        "-c".into(),
        "cat <&3 >/dev/null; sleep 60 & exit 7".into(),
    ];
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        spawn_and_monitor_app(state.clone(), &config_path),
    )
    .await;
    kill_all_app_processes(&state);
    let error = result
        .expect("child exit must interrupt readiness")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("app exited before signaling readiness"),
        "{error}"
    );
    assert!(error.to_string().contains('7'), "{error}");
    let s = state.lock().unwrap();
    assert!(s.apps[&config_path].pid.is_none());
    assert!(s.apps[&config_path].is_idle);
    assert!(!s.routes.lookup("wake.test", "/").unwrap().active);
}

#[cfg(unix)]
#[tokio::test]
async fn queued_startup_can_be_cancelled_before_it_acquires_the_lifecycle_lock() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let directory = tempfile::tempdir().unwrap();
    let (state, config_path) = idle_wake_app(directory.path());
    let lock = crate::compose::app_lock(&state, &config_path);
    let guard = lock.lock().await;
    let mut starting = Box::pin(spawn_and_monitor_app(state.clone(), &config_path));
    assert!(
        poll_fn(|cx| Poll::Ready(starting.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    cancel_app_startup(&state, &config_path);
    drop(guard);
    let error = starting.await.unwrap_err();
    assert!(error.is::<AppStartupCancelled>());
    assert!(state.lock().unwrap().apps[&config_path].pid.is_none());
}

#[cfg(unix)]
async fn control_request(state: Arc<Mutex<State>>, request: crate::protocol::Request) -> Response {
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let handler = tokio::spawn(crate::control::handle_client(server, state));
    let response = {
        let mut client = tokio::io::BufReader::new(client);
        tako_socket::write_json_line(client.get_mut(), &request)
            .await
            .unwrap();
        tako_socket::read_json_line(&mut client)
            .await
            .unwrap()
            .unwrap()
    };
    handler.await.unwrap().unwrap();
    response
}

#[cfg(unix)]
async fn wait_for_startup_event(events: &mut tokio::sync::mpsc::Receiver<Response>, ready: bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap() {
                Response::Event {
                    event: DevEvent::AppPid { .. },
                } if !ready => return,
                Response::Event {
                    event: DevEvent::AppReady { .. },
                } if ready => return,
                Response::Event {
                    event: DevEvent::AppError { message, .. },
                } => panic!("unexpected startup error: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("startup event should arrive promptly");
}

#[cfg(unix)]
#[tokio::test]
async fn restart_and_stop_interrupt_pending_readiness_without_error_logs() {
    use crate::protocol::Request;

    let directory = tempfile::tempdir().unwrap();
    let (state, config_path) = idle_wake_app(directory.path());
    let mut events = state.lock().unwrap().events.subscribe();
    state
        .lock()
        .unwrap()
        .apps
        .get_mut(&config_path)
        .unwrap()
        .command = vec![
        "sh".into(),
        "-c".into(),
        "cat <&3 >/dev/null; exec sleep 60".into(),
    ];
    let log_buffer = state.lock().unwrap().apps[&config_path].log_buffer.clone();
    let response = control_request(
        state.clone(),
        Request::RestartApp {
            config_path: config_path.clone(),
        },
    )
    .await;
    assert!(matches!(response, Response::AppRestarting { .. }));
    wait_for_startup_event(&mut events, false).await;
    let first_pid = state.lock().unwrap().apps[&config_path].pid.unwrap();

    // Restart while the first command is still waiting, then stop the next wait.
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        control_request(
            state.clone(),
            Request::RestartApp {
                config_path: config_path.clone(),
            },
        ),
    )
    .await
    .expect("restart must interrupt readiness");
    assert!(matches!(response, Response::AppRestarting { .. }));
    wait_for_startup_event(&mut events, false).await;
    assert_ne!(
        state.lock().unwrap().apps[&config_path].pid,
        Some(first_pid)
    );

    let response = tokio::time::timeout(
        Duration::from_secs(5),
        control_request(
            state.clone(),
            Request::UnregisterApp {
                config_path: config_path.clone(),
            },
        ),
    )
    .await
    .expect("stop must interrupt readiness");
    assert!(matches!(response, Response::AppUnregistered { .. }));
    assert!(!state.lock().unwrap().apps.contains_key(&config_path));
    for entry in log_buffer.subscribe(None).0 {
        let log: serde_json::Value = serde_json::from_str(&entry.line).unwrap();
        assert_ne!(
            log["level"], "error",
            "cancellation is not a startup failure"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_interrupts_pending_readiness() {
    let directory = tempfile::tempdir().unwrap();
    let (state, config_path) = idle_wake_app(directory.path());
    let mut events = state.lock().unwrap().events.subscribe();
    state
        .lock()
        .unwrap()
        .apps
        .get_mut(&config_path)
        .unwrap()
        .command = vec![
        "sh".into(),
        "-c".into(),
        "cat <&3 >/dev/null; exec sleep 60".into(),
    ];
    let starting = tokio::spawn({
        let state = state.clone();
        let config_path = config_path.clone();
        async move { spawn_and_monitor_app(state, &config_path).await }
    });
    wait_for_startup_event(&mut events, false).await;
    state.lock().unwrap().shutdown_tx.send_replace(true);
    let error = tokio::time::timeout(Duration::from_secs(5), starting)
        .await
        .expect("shutdown must interrupt readiness")
        .unwrap()
        .unwrap_err();
    assert!(error.is::<AppStartupCancelled>());
    assert!(state.lock().unwrap().apps[&config_path].pid.is_none());
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
        compose: None,
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
        workflow_retention_ms: Some(tako_workflows::DEFAULT_RETENTION_MS),
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
        compose: None,
        project_dir: "/tmp/tako-readiness".to_string(),
        name: "readiness".to_string(),
        variant: None,
        hosts: vec!["readiness.test".to_string()],
        upstream_port: 0,
        is_idle: false,
        command: vec!["node".to_string()],
        worker_command: None,
        workflow_retention_ms: Some(tako_workflows::DEFAULT_RETENTION_MS),
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

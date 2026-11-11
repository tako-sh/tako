use super::*;

async fn event_subscription(
    state: Arc<Mutex<State>>,
    config_path: Option<&str>,
) -> (
    tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    tokio::net::unix::OwnedWriteHalf,
    tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
) {
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let handler = tokio::spawn(async move { handle_client(a, state).await });
    let (r, mut w) = b.into_split();
    let request = serde_json::json!({"type": "SubscribeEvents", "config_path": config_path});
    w.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut lines = BufReader::new(r).lines();
    let response: Response =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(matches!(response, Response::Subscribed));
    (lines, w, handler)
}

#[tokio::test]
async fn overflow_recovery_snapshots_missed_exit_before_live_events() {
    let (state, _tmp) = test_state();
    let config = "/proj/tako.toml";
    insert_test_app(&state, "/proj", "my-app");
    let (mut lines, writer, handler) = event_subscription(state.clone(), None).await;
    {
        let s = state.lock().unwrap();
        // No async yield: deterministically exhaust the 512-entry hub queue.
        for _ in 0..513 {
            s.events.broadcast(Response::Event {
                event: protocol::DevEvent::RequestStarted {
                    host: "my-app.test".into(),
                    path: "/".into(),
                },
            });
        }
    }
    let count = tokio::time::timeout(Duration::from_secs(2), async {
        let mut count = 0;
        while lines.next_line().await.unwrap().is_some() {
            count += 1;
        }
        count
    })
    .await
    .unwrap();
    assert_eq!(count, 512);
    drop(writer);
    handler.await.unwrap().unwrap();

    let exited = protocol::DevEvent::AppProcessExited {
        config_path: config.into(),
        app_name: "my-app".into(),
        message: "App exited (exit code 42)".into(),
    };
    {
        let mut s = state.lock().unwrap();
        let app = s.apps.get_mut(config).unwrap();
        app.is_idle = true;
        app.pid = None;
        s.events.broadcast(Response::Event {
            event: exited.clone(),
        });
    }
    let (mut lines, writer, handler) = event_subscription(state.clone(), Some(config)).await;
    let live = protocol::DevEvent::AppLaunching {
        config_path: config.into(),
        app_name: "my-app".into(),
    };
    state.lock().unwrap().events.broadcast(Response::Event {
        event: live.clone(),
    });
    let mut events = Vec::new();
    for _ in 0..5 {
        let line = tokio::time::timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Response::Event { event } = serde_json::from_str(&line).unwrap() else {
            panic!("expected event")
        };
        events.push(event);
    }
    assert!(
        matches!(&events[0], protocol::DevEvent::AppStatusChanged { status, .. } if status == "idle")
    );
    assert_eq!(events[1], exited);
    assert!(matches!(
        events[2],
        protocol::DevEvent::LanModeChanged { enabled: false, .. }
    ));
    assert!(matches!(
        events[3],
        protocol::DevEvent::TunnelModeChanged { enabled: false, .. }
    ));
    assert_eq!(events[4], live);
    drop(lines);
    drop(writer);
    handler.await.unwrap().unwrap();
}

#[tokio::test]
async fn recovery_reports_app_removed_during_disconnect() {
    let (state, _tmp) = test_state();
    let (mut lines, writer, handler) = event_subscription(state, Some("/removed/tako.toml")).await;
    let response: Response =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(matches!(response, Response::Event {
        event: protocol::DevEvent::AppStatusChanged { config_path, status, .. }
    } if config_path == "/removed/tako.toml" && status == "stopped"));
    drop(lines);
    drop(writer);
    handler.await.unwrap().unwrap();
}

#[tokio::test]
async fn recovery_restores_pid_and_latest_lifecycle_without_replaying_history() {
    let (state, _tmp) = test_state();
    let config = "/proj/tako.toml";
    insert_test_app(&state, "/proj", "my-app");
    let ready = protocol::DevEvent::AppReady {
        config_path: config.into(),
        app_name: "my-app".into(),
    };
    {
        let mut s = state.lock().unwrap();
        s.apps.get_mut(config).unwrap().pid = Some(42);
        s.events.broadcast(Response::Event {
            event: protocol::DevEvent::AppError {
                config_path: config.into(),
                app_name: "my-app".into(),
                message: "old failure".into(),
            },
        });
        s.events.broadcast(Response::Event {
            event: ready.clone(),
        });
    }
    let (mut lines, writer, handler) = event_subscription(state.clone(), Some(config)).await;
    let mut events = Vec::new();
    for _ in 0..5 {
        let line = lines.next_line().await.unwrap().unwrap();
        let Response::Event { event } = serde_json::from_str(&line).unwrap() else {
            panic!("expected event")
        };
        events.push(event);
    }
    assert!(
        matches!(&events[0], protocol::DevEvent::AppStatusChanged { status, .. } if status == "running")
    );
    assert!(matches!(
        events[1],
        protocol::DevEvent::AppPid { pid: 42, .. }
    ));
    assert_eq!(events[2], ready);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, protocol::DevEvent::AppError { .. }))
    );
    drop(lines);
    drop(writer);
    handler.await.unwrap().unwrap();

    // Unregistration releases cached state and late readiness cannot recreate it.
    {
        let mut s = state.lock().unwrap();
        s.apps.remove(config);
        s.events.broadcast(Response::Event {
            event: protocol::DevEvent::AppStatusChanged {
                config_path: config.into(),
                app_name: "my-app".into(),
                status: "stopped".into(),
            },
        });
    }
    super::super::process::broadcast_dev_event(&state, ready);
    assert!(
        state
            .lock()
            .unwrap()
            .events
            .current_app_event(config)
            .is_none()
    );
}

#[tokio::test]
async fn info_reports_connected_control_clients() {
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);
    let st = State::new(
        shutdown_tx,
        proxy::Routes::default(),
        EventsHub::default(),
        true,
        53535,
        8443,
        "127.0.0.1:8443".to_string(),
        "127.0.0.1".to_string(),
    );
    let _ = test_workflows();
    let state = Arc::new(Mutex::new(st));
    let h = tokio::spawn({
        let state = state.clone();
        async move { handle_client(a, state).await }
    });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();
    w.write_all(b"{\"type\":\"SubscribeEvents\"}\n")
        .await
        .unwrap();

    let sub_line = lines.next_line().await.unwrap().unwrap();
    let sub_resp: Response = serde_json::from_str(&sub_line).unwrap();
    assert!(matches!(sub_resp, Response::Subscribed));

    let clients = query_control_clients(state.clone()).await;
    assert_eq!(clients, 1);

    drop(lines);
    drop(w);

    tokio::time::timeout(Duration::from_secs(1), h)
        .await
        .expect("subscribe handler should exit")
        .unwrap()
        .unwrap();

    let clients = query_control_clients(state).await;
    assert_eq!(clients, 0);
}

/// Helper: create a test State with a temp SQLite DB and return (state, _tmpdir).

#[tokio::test]
async fn restart_app_responds_with_app_restarting() {
    let (state, _tmp) = test_state();
    insert_test_app(&state, "/proj", "my-app");

    // Send RestartApp.
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let req = serde_json::json!({
        "type": "RestartApp",
        "config_path": "/proj/tako.toml",
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let mut lines = BufReader::new(r).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::AppRestarting { config_path } => {
            assert_eq!(config_path, "/proj/tako.toml");
        }
        other => panic!("expected AppRestarting, got: {other:?}"),
    }

    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn set_app_status_broadcasts_status_changed_event() {
    let (state, _tmp) = test_state();
    insert_test_app(&state, "/proj", "my-app");

    let mut ev_rx = {
        let s = state.lock().unwrap();
        s.events.subscribe()
    };

    // Send SetAppStatus.
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let req = serde_json::json!({
        "type": "SetAppStatus",
        "config_path": "/proj/tako.toml",
        "status": "idle",
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let mut lines = BufReader::new(r).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert!(matches!(resp, Response::AppStatusUpdated { .. }));

    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();

    let event = tokio::time::timeout(Duration::from_millis(100), ev_rx.recv())
        .await
        .expect("should not time out")
        .unwrap();

    match event {
        Response::Event {
            event:
                protocol::DevEvent::AppStatusChanged {
                    config_path,
                    app_name,
                    status,
                },
        } => {
            assert_eq!(config_path, "/proj/tako.toml");
            assert_eq!(app_name, "my-app");
            assert_eq!(status, "idle");
        }
        other => panic!("expected AppStatusChanged, got: {other:?}"),
    }
}

/// End-to-end test: client B subscribes to events via a real socket
/// handler, client A unregisters an app via a separate socket handler,
/// and client B must receive the AppStatusChanged{stopped} event over
/// the wire. This exercises the exact codepath that the connected dev
/// client uses to detect when the owner stops the app.
#[tokio::test]
async fn subscriber_receives_stopped_event_over_socket_when_app_unregistered() {
    let (state, _tmp) = test_state();
    insert_test_app(&state, "/proj", "my-app");

    // Client B: subscribe to events via a real socket handler.
    let (sub_a, sub_b) = tokio::net::UnixStream::pair().unwrap();
    let sub_handler = tokio::spawn({
        let state = state.clone();
        async move { handle_client(sub_a, state).await }
    });
    let (sub_r, mut sub_w) = sub_b.into_split();
    let mut sub_lines = BufReader::new(sub_r).lines();

    sub_w
        .write_all(b"{\"type\":\"SubscribeEvents\"}\n")
        .await
        .unwrap();
    let resp_line = sub_lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&resp_line).unwrap();
    assert!(matches!(resp, Response::Subscribed));

    // Client A: unregister the app via a separate socket handler.
    let (unreg_a, unreg_b) = tokio::net::UnixStream::pair().unwrap();
    let unreg_handler = tokio::spawn({
        let state = state.clone();
        async move { handle_client(unreg_a, state).await }
    });
    let (unreg_r, mut unreg_w) = unreg_b.into_split();

    let req = serde_json::json!({
        "type": "UnregisterApp",
        "config_path": "/proj/tako.toml",
    });
    unreg_w
        .write_all(format!("{}\n", req).as_bytes())
        .await
        .unwrap();

    let mut unreg_lines = BufReader::new(unreg_r).lines();
    let unreg_resp_line = unreg_lines.next_line().await.unwrap().unwrap();
    let unreg_resp: Response = serde_json::from_str(&unreg_resp_line).unwrap();
    assert!(matches!(unreg_resp, Response::AppUnregistered { .. }));

    // Clean up unregister handler.
    drop(unreg_w);
    drop(unreg_lines);
    unreg_handler.await.unwrap().unwrap();

    // Client B should receive the AppStatusChanged event.
    let event_line = tokio::time::timeout(Duration::from_millis(500), sub_lines.next_line())
        .await
        .expect("subscriber should receive event within 500ms")
        .unwrap()
        .unwrap();
    let event_resp: Response = serde_json::from_str(&event_line).unwrap();
    match event_resp {
        Response::Event {
            event:
                protocol::DevEvent::AppStatusChanged {
                    config_path,
                    app_name,
                    status,
                },
        } => {
            assert_eq!(config_path, "/proj/tako.toml");
            assert_eq!(app_name, "my-app");
            assert_eq!(status, "stopped");
        }
        other => panic!("expected AppStatusChanged stopped, got: {other:?}"),
    }

    // Clean up subscriber.
    drop(sub_w);
    drop(sub_lines);
    let _ = tokio::time::timeout(Duration::from_secs(1), sub_handler).await;
}

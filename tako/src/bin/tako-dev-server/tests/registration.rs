use super::*;

#[tokio::test]
async fn register_app_roundtrip() {
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
    let h = tokio::spawn(async move { handle_client(a, state).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": "/tmp/test-proj/tako.toml",
        "project_dir": "/tmp/test-proj",
        "app_name": "my-app",
        "hosts": ["my-app.test"],
        "command": ["node", "index.js"],
        "env": {}
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let reg_line = lines.next_line().await.unwrap().unwrap();
    let reg: Response = serde_json::from_str(&reg_line).unwrap();
    match reg {
        Response::AppRegistered {
            app_name,
            config_path,
            project_dir,
            url,
        } => {
            assert_eq!(app_name, "my-app");
            assert_eq!(config_path, "/tmp/test-proj/tako.toml");
            assert_eq!(project_dir, "/tmp/test-proj");
            assert!(url.contains("my-app.test"));
        }
        other => panic!("unexpected: {other:?}"),
    }

    drop(w);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn register_app_starts_workflow_engine_when_worker_command_provided() {
    // Project layout: workflows/ + a fake worker entrypoint + fake bun.
    // We use `true` (exits 0) as the worker command so `ensure()` succeeds
    // without actually running a real worker.
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join("workflows")).unwrap();
    std::fs::write(
        proj.path().join("workflows").join("broadcast.ts"),
        "export default () => {};",
    )
    .unwrap();

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);

    let workflows_dir = tempfile::TempDir::new().unwrap();
    let workflows = Arc::new(tako_workflows::WorkflowManager::new(workflows_dir.path()));
    workflows.start_socket().unwrap();
    let internal_socket = workflows.socket_path();

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
    let state = Arc::new(Mutex::new(st));
    {
        let mut s = state.lock().unwrap();
        s.workflows = Some(workflows.clone());
        s.internal_socket = Some(internal_socket);
    }
    let h = tokio::spawn({
        let state = state.clone();
        async move { handle_client(a, state).await }
    });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let config_path = proj.path().join("tako.toml").to_string_lossy().to_string();
    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": config_path,
        "project_dir": proj.path().to_string_lossy(),
        "app_name": "wf-app",
        "hosts": ["wf-app.test"],
        "command": ["node", "index.js"],
        "env": {},
        "worker_command": ["true"],
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let reg_line = lines.next_line().await.unwrap().unwrap();
    let reg: Response = serde_json::from_str(&reg_line).unwrap();
    assert!(matches!(reg, Response::AppRegistered { .. }));

    assert!(workflows.has("wf-app"));

    drop(w);
    h.await.unwrap().unwrap();
    workflows.shutdown_all(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn restart_app_reconfigures_workflow_runtime_when_worker_command_registered() {
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join("workflows")).unwrap();
    std::fs::write(
        proj.path().join("workflows").join("broadcast.ts"),
        "export default () => {};",
    )
    .unwrap();

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);

    let workflows_dir = tempfile::TempDir::new().unwrap();
    let workflows = Arc::new(tako_workflows::WorkflowManager::new(workflows_dir.path()));
    workflows.start_socket().unwrap();
    let internal_socket = workflows.socket_path();

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
    let state = Arc::new(Mutex::new(st));
    {
        let mut s = state.lock().unwrap();
        s.workflows = Some(workflows.clone());
        s.internal_socket = Some(internal_socket);
    }
    let h = tokio::spawn({
        let state = state.clone();
        async move { handle_client(a, state).await }
    });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let config_path = proj.path().join("tako.toml").to_string_lossy().to_string();
    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": config_path,
        "project_dir": proj.path().to_string_lossy(),
        "app_name": "wf-app",
        "hosts": ["wf-app.test"],
        "command": ["true"],
        "env": {},
        "worker_command": ["true"],
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let reg_line = lines.next_line().await.unwrap().unwrap();
    let reg: Response = serde_json::from_str(&reg_line).unwrap();
    assert!(matches!(reg, Response::AppRegistered { .. }));

    let first = workflows.supervisor_for("wf-app").unwrap();

    let req = serde_json::json!({
        "type": "RestartApp",
        "config_path": config_path,
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let restart_line = lines.next_line().await.unwrap().unwrap();
    let restart: Response = serde_json::from_str(&restart_line).unwrap();
    assert!(matches!(restart, Response::AppRestarting { .. }));

    let second = workflows.supervisor_for("wf-app").unwrap();
    assert!(!Arc::ptr_eq(&first, &second));

    drop(w);
    h.await.unwrap().unwrap();
    workflows.shutdown_all(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn register_app_without_worker_command_stops_existing_workflow_runtime() {
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join("workflows")).unwrap();

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);

    let workflows_dir = tempfile::TempDir::new().unwrap();
    let workflows = Arc::new(tako_workflows::WorkflowManager::new(workflows_dir.path()));
    workflows.start_socket().unwrap();
    let internal_socket = workflows.socket_path();

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
    let state = Arc::new(Mutex::new(st));
    {
        let mut s = state.lock().unwrap();
        s.workflows = Some(workflows.clone());
        s.internal_socket = Some(internal_socket);
    }
    let h = tokio::spawn({
        let state = state.clone();
        async move { handle_client(a, state).await }
    });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();
    let config_path = proj.path().join("tako.toml").to_string_lossy().to_string();

    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": config_path,
        "project_dir": proj.path().to_string_lossy(),
        "app_name": "wf-app",
        "hosts": ["wf-app.test"],
        "command": ["true"],
        "env": {},
        "worker_command": ["true"],
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    let reg_line = lines.next_line().await.unwrap().unwrap();
    let reg: Response = serde_json::from_str(&reg_line).unwrap();
    assert!(matches!(reg, Response::AppRegistered { .. }));
    assert!(workflows.has("wf-app"));

    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": config_path,
        "project_dir": proj.path().to_string_lossy(),
        "app_name": "wf-app",
        "hosts": ["wf-app.test"],
        "command": ["true"],
        "env": {},
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    let reg_line = lines.next_line().await.unwrap().unwrap();
    let reg: Response = serde_json::from_str(&reg_line).unwrap();
    assert!(matches!(reg, Response::AppRegistered { .. }));
    assert!(!workflows.has("wf-app"));

    drop(w);
    h.await.unwrap().unwrap();
    workflows.shutdown_all(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn register_app_with_new_name_stops_previous_workflow_runtime() {
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join("workflows")).unwrap();

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);

    let workflows_dir = tempfile::TempDir::new().unwrap();
    let workflows = Arc::new(tako_workflows::WorkflowManager::new(workflows_dir.path()));
    workflows.start_socket().unwrap();
    let internal_socket = workflows.socket_path();

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
    let state = Arc::new(Mutex::new(st));
    {
        let mut s = state.lock().unwrap();
        s.workflows = Some(workflows.clone());
        s.internal_socket = Some(internal_socket);
    }
    let h = tokio::spawn({
        let state = state.clone();
        async move { handle_client(a, state).await }
    });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();
    let config_path = proj.path().join("tako.toml").to_string_lossy().to_string();

    for app_name in ["old-wf-app", "new-wf-app"] {
        let req = serde_json::json!({
            "type": "RegisterApp",
            "config_path": config_path,
            "project_dir": proj.path().to_string_lossy(),
            "app_name": app_name,
            "hosts": [format!("{app_name}.test")],
            "command": ["true"],
            "env": {},
            "worker_command": ["true"],
        });
        w.write_all(req.to_string().as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
        let reg_line = lines.next_line().await.unwrap().unwrap();
        let reg: Response = serde_json::from_str(&reg_line).unwrap();
        assert!(matches!(reg, Response::AppRegistered { .. }));
    }

    assert!(!workflows.has("old-wf-app"));
    assert!(workflows.has("new-wf-app"));

    drop(w);
    h.await.unwrap().unwrap();
    workflows.shutdown_all(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn unregister_app_stops_workflow_runtime() {
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join("workflows")).unwrap();

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);

    let workflows_dir = tempfile::TempDir::new().unwrap();
    let workflows = Arc::new(tako_workflows::WorkflowManager::new(workflows_dir.path()));
    workflows.start_socket().unwrap();
    let internal_socket = workflows.socket_path();

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
    let state = Arc::new(Mutex::new(st));
    {
        let mut s = state.lock().unwrap();
        s.workflows = Some(workflows.clone());
        s.internal_socket = Some(internal_socket);
    }
    let h = tokio::spawn({
        let state = state.clone();
        async move { handle_client(a, state).await }
    });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();
    let config_path = proj.path().join("tako.toml").to_string_lossy().to_string();

    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": config_path,
        "project_dir": proj.path().to_string_lossy(),
        "app_name": "wf-app",
        "hosts": ["wf-app.test"],
        "command": ["true"],
        "env": {},
        "worker_command": ["true"],
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    let reg_line = lines.next_line().await.unwrap().unwrap();
    let reg: Response = serde_json::from_str(&reg_line).unwrap();
    assert!(matches!(reg, Response::AppRegistered { .. }));
    assert!(workflows.has("wf-app"));

    let req = serde_json::json!({
        "type": "UnregisterApp",
        "config_path": config_path,
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    let unregister_line = lines.next_line().await.unwrap().unwrap();
    let unregister: Response = serde_json::from_str(&unregister_line).unwrap();
    assert!(matches!(unregister, Response::AppUnregistered { .. }));
    assert!(!workflows.has("wf-app"));

    drop(w);
    h.await.unwrap().unwrap();
    workflows.shutdown_all(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn register_app_skips_workflow_engine_when_no_workflows_dir() {
    let proj = tempfile::TempDir::new().unwrap();

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);

    let workflows_dir = tempfile::TempDir::new().unwrap();
    let workflows = Arc::new(tako_workflows::WorkflowManager::new(workflows_dir.path()));
    workflows.start_socket().unwrap();

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
    let state = Arc::new(Mutex::new(st));
    let h = tokio::spawn(async move { handle_client(a, state).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let config_path = proj.path().join("tako.toml").to_string_lossy().to_string();
    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": config_path,
        "project_dir": proj.path().to_string_lossy(),
        "app_name": "plain-app",
        "hosts": ["plain.test"],
        "command": ["node", "index.js"],
        "env": {}
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let _reg_line = lines.next_line().await.unwrap().unwrap();
    assert!(!workflows.has("plain-app"));

    drop(w);
    h.await.unwrap().unwrap();
    workflows.shutdown_all(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn unregister_app_broadcasts_stopped_event_to_subscriber() {
    let (state, _tmp) = test_state();
    insert_test_app(&state, "/proj", "my-app");

    // Subscribe to events.
    let mut ev_rx = {
        let s = state.lock().unwrap();
        s.events.subscribe()
    };

    // Unregister the app on a client connection.
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let req = serde_json::json!({
        "type": "UnregisterApp",
        "config_path": "/proj/tako.toml",
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let mut lines = BufReader::new(r).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert!(matches!(resp, Response::AppUnregistered { .. }));

    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();

    // The subscriber should have received an AppStatusChanged event.
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
            assert_eq!(status, "stopped");
        }
        other => panic!("expected AppStatusChanged, got: {other:?}"),
    }
}

#[tokio::test]
async fn register_app_with_variant_roundtrip() {
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (state, _tmp) = test_state();
    let h = tokio::spawn(async move { handle_client(a, state).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": "/proj/my-app/preview.toml",
        "project_dir": "/proj/my-app",
        "app_name": "my-app-staging",
        "variant": "staging",
        "hosts": ["my-app-staging.test"],
        "upstream_port": 3000,
        "command": ["bun", "run", "index.ts"],
        "env": {},
        "log_path": "/tmp/log.jsonl"
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::AppRegistered { app_name, .. } => {
            assert_eq!(app_name, "my-app-staging");
        }
        other => panic!("unexpected: {other:?}"),
    }

    // List and verify variant is present.
    let req = serde_json::json!({"type": "ListRegisteredApps"});
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::RegisteredApps { apps } => {
            assert_eq!(apps.len(), 1);
            assert_eq!(apps[0].app_name, "my-app-staging");
            assert_eq!(apps[0].variant, Some("staging".to_string()));
        }
        other => panic!("unexpected: {other:?}"),
    }

    drop(w);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn register_app_without_variant_has_none() {
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (state, _tmp) = test_state();
    let h = tokio::spawn(async move { handle_client(a, state).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": "/proj/my-app/tako.toml",
        "project_dir": "/proj/my-app",
        "app_name": "my-app",
        "hosts": ["my-app.test"],
        "upstream_port": 3000,
        "command": ["bun", "run", "index.ts"],
        "env": {},
        "log_path": "/tmp/log.jsonl"
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let _line = lines.next_line().await.unwrap().unwrap();

    let req = serde_json::json!({"type": "ListRegisteredApps"});
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::RegisteredApps { apps } => {
            assert_eq!(apps.len(), 1);
            assert_eq!(apps[0].app_name, "my-app");
            assert!(apps[0].variant.is_none());
        }
        other => panic!("unexpected: {other:?}"),
    }

    drop(w);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn variant_and_non_variant_coexist_in_list() {
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let (state, _tmp) = test_state();
    let h = tokio::spawn(async move { handle_client(a, state).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    // Register "app-foo" without variant from /proj1
    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": "/proj1/tako.toml",
        "project_dir": "/proj1",
        "app_name": "app-foo",
        "hosts": ["app-foo.test"],
        "upstream_port": 3000,
        "command": ["bun", "run", "index.ts"],
        "env": {},
        "log_path": "/tmp/log1.jsonl"
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    let _line = lines.next_line().await.unwrap().unwrap();

    // Register "app-foo" with variant "foo" from /proj2
    // (in practice the CLI would have disambiguated the name, but
    //  this tests that both can coexist with different project_dirs)
    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": "/proj2/tako.toml",
        "project_dir": "/proj2",
        "app_name": "app-foo-proj2",
        "variant": "foo",
        "hosts": ["app-foo-proj2.test"],
        "upstream_port": 3001,
        "command": ["bun", "run", "index.ts"],
        "env": {},
        "log_path": "/tmp/log2.jsonl"
    });
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    let _line = lines.next_line().await.unwrap().unwrap();

    // List all
    let req = serde_json::json!({"type": "ListRegisteredApps"});
    w.write_all(req.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    match resp {
        Response::RegisteredApps { apps } => {
            assert_eq!(apps.len(), 2);
            let no_variant = apps.iter().find(|a| a.project_dir == "/proj1").unwrap();
            let with_variant = apps.iter().find(|a| a.project_dir == "/proj2").unwrap();
            assert_eq!(no_variant.app_name, "app-foo");
            assert!(no_variant.variant.is_none());
            assert_eq!(with_variant.app_name, "app-foo-proj2");
            assert_eq!(with_variant.variant, Some("foo".to_string()));
        }
        other => panic!("unexpected: {other:?}"),
    }

    drop(w);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn register_app_spawns_process_and_sets_pid() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (state, _tmp_db) = test_state();

    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let mut lines = BufReader::new(r).lines();

    let req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": "/tmp/test-spawn/tako.toml",
        "project_dir": tmp.path().to_str().unwrap(),
        "app_name": "spawn-test",
        "hosts": ["spawn-test.test"],
        "upstream_port": 19999,
        "command": ["sleep", "60"],
        "env": {},
    });
    w.write_all(format!("{}\n", req).as_bytes()).await.unwrap();

    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert!(matches!(resp, Response::AppRegistered { .. }));

    // Wait a moment for the background spawn task.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The app should now have a PID set by the daemon.
    let pid = {
        let s = state.lock().unwrap();
        s.apps.get("/tmp/test-spawn/tako.toml").and_then(|a| a.pid)
    };
    assert!(pid.is_some(), "daemon should have spawned the app process");

    // Clean up: kill the spawned process.
    if let Some(pid) = pid {
        kill_app_process(pid);
    }

    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();
}

#[tokio::test]
async fn unregister_app_kills_running_process() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (state, _tmp_db) = test_state();

    // Insert an app with a real running process.
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid = child.id();

    {
        let mut s = state.lock().unwrap();
        s.apps.insert(
            "/tmp/test-kill/tako.toml".to_string(),
            state::RuntimeApp {
                project_dir: tmp.path().to_string_lossy().to_string(),
                name: "kill-test".to_string(),
                variant: None,
                hosts: vec!["kill-test.test".to_string()],
                upstream_port: 19998,
                is_idle: false,
                command: vec!["sleep".to_string(), "60".to_string()],
                worker_command: None,
                env: std::collections::HashMap::new(),
                log_buffer: state::LogBuffer::new(),
                pid: Some(pid),
                client_pid: None,
                tunnel: None,
                readiness_failure_hint: None,
                bootstrap_token: "dev-token".to_string(),
                secrets: std::collections::HashMap::new(),
                storages: std::collections::HashMap::new(),
            },
        );
    }

    // Unregister via socket.
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let state_for_handler = state.clone();
    let h = tokio::spawn(async move { handle_client(a, state_for_handler).await });

    let (r, mut w) = b.into_split();
    let req = serde_json::json!({
        "type": "UnregisterApp",
        "config_path": "/tmp/test-kill/tako.toml",
    });
    w.write_all(format!("{}\n", req).as_bytes()).await.unwrap();

    let mut lines = BufReader::new(r).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert!(matches!(resp, Response::AppUnregistered { .. }));

    drop(w);
    drop(lines);
    h.await.unwrap().unwrap();

    // The process should have been killed.
    let status = child.wait().unwrap();
    assert!(!status.success(), "process should have been killed");
}

#[test]
fn variant_persisted_in_sqlite() {
    let (state, _tmp) = test_state();
    let s = state.lock().unwrap();
    let db = s.db.as_ref().unwrap();

    db.register("/proj/tako.toml", "/proj", "my-app", Some("staging"))
        .unwrap();
    let app = db.get("/proj/tako.toml").unwrap().unwrap();
    assert_eq!(app.name, "my-app");
    assert_eq!(app.variant.as_deref(), Some("staging"));

    // Re-register without variant clears it.
    db.register("/proj/tako.toml", "/proj", "my-app", None)
        .unwrap();
    let app = db.get("/proj/tako.toml").unwrap().unwrap();
    assert!(app.variant.is_none());
}

/// Concurrent wake-on-request calls must spawn exactly one process.
///
/// Before the fix, all callers raced through the `is_idle` check and each
/// proceeded to spawn, producing N processes for N concurrent requests.
/// After the fix the check-and-clear is atomic under the same lock, so only
/// the first caller spawns; the rest bail out immediately.

#[tokio::test]
async fn wake_on_request_spawns_exactly_one_process() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (state, _tmp_db) = test_state();

    // Directory where each spawn attempt writes a marker file.
    let spawn_dir = tmp.path().join("spawns");
    std::fs::create_dir_all(&spawn_dir).unwrap();

    // The command touches a file named after its own PID, then sleeps.
    // Each distinct spawn produces a distinct file.
    let cmd_str = format!("touch {}/$$; sleep 60", spawn_dir.display());
    let config_path = format!("{}/tako.toml", tmp.path().display());

    {
        let mut s = state.lock().unwrap();
        s.apps.insert(
            config_path.clone(),
            state::RuntimeApp {
                project_dir: tmp.path().to_str().unwrap().to_string(),
                name: "wake-race-test".to_string(),
                variant: None,
                hosts: vec!["wake-race.test".to_string()],
                upstream_port: 0,
                is_idle: true,
                command: vec!["sh".to_string(), "-c".to_string(), cmd_str],
                worker_command: None,
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
            vec!["wake-race.test".to_string()],
            0,
            false,
        );
    }

    // Fire 10 concurrent wake tasks — only one should win the spawn race.
    let handles: Vec<_> = (0..10)
        .map(|_| {
            let state = state.clone();
            tokio::spawn(async move {
                handle_wake_on_request(state, "wake-race.test".to_string(), "/".to_string()).await;
            })
        })
        .collect();

    // Give the tasks time to acquire the lock and make their decision.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Kill the single spawned process so the readiness wait unblocks.
    let pid = state
        .lock()
        .unwrap()
        .apps
        .get(&config_path)
        .and_then(|a| a.pid);
    if let Some(pid) = pid {
        kill_app_process(pid);
    }

    for h in handles {
        let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
    }

    // Exactly one marker file means exactly one spawn.
    let spawn_count = std::fs::read_dir(&spawn_dir).unwrap().count();
    assert_eq!(
        spawn_count, 1,
        "expected exactly 1 spawn, got {spawn_count}"
    );
}

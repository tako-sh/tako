use super::*;

#[tokio::test]
async fn queued_compose_start_is_rejected_during_shutdown() {
    let (shutdown_tx, _shutdown_rx) = watch::channel(true);
    let state = Arc::new(Mutex::new(State::new(
        shutdown_tx,
        proxy::Routes::default(),
        EventsHub::default(),
        false,
        53535,
        8443,
        "127.0.0.1:8443".into(),
        "127.0.0.1".into(),
    )));
    let error = crate::process::spawn_and_monitor_app(state, "/pending/tako.toml")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("shutting down"));
}

#[tokio::test]
#[ignore = "requires Docker Compose and shared host/container loopback and project paths"]
async fn compose_registration_restart_stop_and_shutdown_keep_redis_data() {
    let project = tempfile::tempdir().unwrap();
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/compose/bun-redis");
    for file in [
        "tako.toml",
        "compose.yaml",
        "Dockerfile",
        "server.ts",
        ".dockerignore",
    ] {
        std::fs::copy(source.join(file), project.path().join(file)).unwrap();
    }
    let key = project
        .path()
        .join("tako.toml")
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    let config = tako_core::compose::ComposeConfig {
        file: "compose.yaml".into(),
        service: "web".into(),
        port: 3000,
        health_path: "/health".into(),
        health_host: None,
    };
    let runtime = tako_compose::ComposeRuntime::development(
        project.path().into(),
        config.clone(),
        Default::default(),
        &format!("{key}:"),
    )
    .unwrap();
    struct Cleanup(tako_compose::ComposeRuntime);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self
                .0
                .command(false)
                .into_std()
                .args(["down", "--volumes", "--remove-orphans"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    let _cleanup = Cleanup(runtime.clone());
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);
    let state = Arc::new(Mutex::new(State::new(
        shutdown_tx,
        proxy::Routes::default(),
        EventsHub::default(),
        false,
        53535,
        8443,
        "127.0.0.1:8443".into(),
        "127.0.0.1".into(),
    )));
    let register = serde_json::json!({"type":"RegisterApp", "config_path":key,
        "project_dir":project.path(), "app_name":"compose-example", "hosts":["compose.test"],
        "command":[], "env":{}, "compose":config});

    async fn request(state: &Arc<Mutex<State>>, value: serde_json::Value) -> Response {
        let (server, client) = tokio::net::UnixStream::pair().unwrap();
        let handle = tokio::spawn(handle_client(server, state.clone()));
        let (read, mut write) = client.into_split();
        write
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        let mut lines = BufReader::new(read).lines();
        let response = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        drop(write);
        drop(lines);
        handle.await.unwrap().unwrap();
        response
    }
    async fn ready(state: &Arc<Mutex<State>>, key: &str) -> u16 {
        for _ in 0..600 {
            if let Some(port) = {
                let state = state.lock().unwrap();
                state
                    .apps
                    .get(key)
                    .filter(|app| app.pid.is_some() && app.upstream_port > 0)
                    .filter(|_| {
                        state
                            .routes
                            .lookup("compose.test", "/")
                            .is_some_and(|route| route.active)
                    })
                    .map(|app| app.upstream_port)
            } {
                return port;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("Compose app did not become ready");
    }
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert!(matches!(
        request(&state, register.clone()).await,
        Response::AppRegistered { .. }
    ));
    let port = ready(&state, &key).await;
    let first: serde_json::Value = http
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        http.get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(matches!(
        request(
            &state,
            serde_json::json!({"type":"RestartApp","config_path":key})
        )
        .await,
        Response::AppRestarting { .. }
    ));
    let port = ready(&state, &key).await;
    let second: serde_json::Value = http
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        second["visits"].as_i64(),
        first["visits"].as_i64().map(|n| n + 1)
    );
    assert!(matches!(
        request(
            &state,
            serde_json::json!({"type":"UnregisterApp","config_path":key})
        )
        .await,
        Response::AppUnregistered { .. }
    ));
    assert!(
        runtime
            .status()
            .await
            .unwrap()
            .iter()
            .all(|service| service.state == "exited")
    );
    assert!(matches!(
        request(&state, register).await,
        Response::AppRegistered { .. }
    ));
    let port = ready(&state, &key).await;
    let third: serde_json::Value = http
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        third["visits"].as_i64(),
        second["visits"].as_i64().map(|n| n + 1)
    );
    crate::compose::stop_all(&state).await;
    kill_all_app_processes(&state);
    assert!(
        runtime
            .status()
            .await
            .unwrap()
            .iter()
            .all(|service| service.state == "exited")
    );
}

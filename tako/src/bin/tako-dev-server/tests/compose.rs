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
async fn compose_apps_route_independently_and_keep_redis_data_across_restart() {
    let project = tempfile::tempdir().unwrap();
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/compose/web-api-redis");
    for file in [
        "tako.toml",
        "compose.yaml",
        "Dockerfile",
        "web.ts",
        "api.ts",
        ".dockerignore",
    ] {
        std::fs::copy(source.join(file), project.path().join(file)).unwrap();
    }
    // Production replicas must not compete for the per-service local dev ports.
    let yaml = project.path().join("compose.yaml");
    let source = std::fs::read_to_string(&yaml).unwrap().replacen(
        "  web:\n",
        "  web:\n    deploy:\n      replicas: 3\n",
        1,
    );
    std::fs::write(yaml, source).unwrap();
    let server = project.path().join("web.ts");
    let mut source = std::fs::read_to_string(&server).unwrap();
    // Exceed pipe capacity on both streams before the markers. The log client
    // must keep draining while startup waits for HTTP readiness.
    source.push_str(
        "\nconsole.log('o'.repeat(96 * 1024));\nconsole.log('compose-stdout-end');\nconsole.error('e'.repeat(96 * 1024));\nconsole.error('compose-stderr-end');\n",
    );
    std::fs::write(server, source).unwrap();
    let key = project
        .path()
        .join("tako.toml")
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    let config = tako_compose::load_config(project.path(), "compose.yaml").unwrap();
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
        "project_dir":project.path(), "app_name":"compose-example", "hosts":["shop.test", "api.shop.test"],
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
    async fn ready(state: &Arc<Mutex<State>>, key: &str, previous_pid: Option<u32>) -> (u16, u16) {
        for _ in 0..600 {
            if let Some(ports) = {
                let state = state.lock().unwrap();
                state
                    .apps
                    .get(key)
                    .filter(|app| app.pid.is_some() && app.pid != previous_pid)
                    .and_then(|_| {
                        let web = state.routes.lookup("shop.test", "/")?;
                        let api = state.routes.lookup("api.shop.test", "/visits")?;
                        (web.active && api.active).then_some((web.upstream_port, api.upstream_port))
                    })
            } {
                assert_ne!(ports.0, ports.1, "Each app must have its own backend");
                return ports;
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
    let (web_port, port) = ready(&state, &key, None).await;
    let web = http
        .get(format!("http://127.0.0.1:{web_port}/"))
        .send()
        .await
        .unwrap();
    assert!(web.text().await.unwrap().contains("One group, two apps"));
    let services = runtime.status().await.unwrap();
    assert_eq!(services.iter().filter(|s| s.service == "web").count(), 1);
    assert_eq!(services.iter().filter(|s| s.service == "api").count(), 1);
    assert!(
        state
            .lock()
            .unwrap()
            .routes
            .lookup("redis.shop.test", "/")
            .is_none()
    );
    let (backlog, mut logs, _) = state.lock().unwrap().apps[&key].log_buffer.subscribe(None);
    let mut stdout_seen = backlog
        .iter()
        .any(|entry| entry.line.contains("compose-stdout-end"));
    let mut stderr_seen = backlog
        .iter()
        .any(|entry| entry.line.contains("compose-stderr-end"));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !stdout_seen || !stderr_seen {
            let entry = logs.recv().await.expect("Compose log stream closed");
            stdout_seen |= entry.line.contains("compose-stdout-end");
            stderr_seen |= entry.line.contains("compose-stderr-end");
        }
    })
    .await
    .expect("Compose stdout and stderr did not reach the log buffer");
    let first: serde_json::Value = http
        .get(format!("http://127.0.0.1:{port}/visits"))
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
    let previous_pid = state.lock().unwrap().apps[&key].pid;
    assert!(matches!(
        request(
            &state,
            serde_json::json!({"type":"RestartApp","config_path":key})
        )
        .await,
        Response::AppRestarting { .. }
    ));
    let (_, port) = ready(&state, &key, previous_pid).await;
    let second: serde_json::Value = http
        .get(format!("http://127.0.0.1:{port}/visits"))
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
    let (_, port) = ready(&state, &key, None).await;
    let third: serde_json::Value = http
        .get(format!("http://127.0.0.1:{port}/visits"))
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
    // Restart reloads route metadata without requiring re-registration.
    let yaml = project.path().join("compose.yaml");
    let original = std::fs::read_to_string(&yaml).unwrap();
    std::fs::write(
        &yaml,
        original.replace("api.shop.test", "renamed.shop.test"),
    )
    .unwrap();
    let previous_pid = state.lock().unwrap().apps[&key].pid;
    assert!(matches!(
        request(
            &state,
            serde_json::json!({"type":"RestartApp","config_path":key})
        )
        .await,
        Response::AppRestarting { .. }
    ));
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let ready = {
                let s = state.lock().unwrap();
                s.apps[&key].pid.is_some()
                    && s.apps[&key].pid != previous_pid
                    && s.routes
                        .lookup("renamed.shop.test", "/visits")
                        .is_some_and(|r| r.active)
            };
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        state
            .lock()
            .unwrap()
            .routes
            .lookup("api.shop.test", "/visits")
            .is_none()
    );
    assert!(
        state.lock().unwrap().apps[&key]
            .hosts
            .contains(&"renamed.shop.test".to_string())
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

#[test]
fn compose_restart_reloads_tagged_apps_and_variant_routes() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("compose.yaml");
    std::fs::write(
        &file,
        "services:\n  web:\n    image: busybox\n    x-tako: {http: true}\n",
    )
    .unwrap();
    let runtime = tako_compose::ComposeRuntime::development(
        project.path().into(),
        tako_compose::load_config(project.path(), "compose.yaml").unwrap(),
        Default::default(),
        "variant-test",
    )
    .unwrap()
    .with_development_hosts("feature.shop.test".into(), Some("shop.test".into()));
    std::fs::write(
        &file,
        r#"
services:
  web:
    image: busybox
  api:
    image: busybox
    x-tako:
      envs:
        development:
          route: api.shop.test
  new:
    image: busybox
    x-tako: {http: true}
  worker:
    image: busybox
    x-tako: {sdk: true}
"#,
    )
    .unwrap();
    let refreshed = runtime.reload_development().unwrap();
    assert!(!refreshed.config.services.contains_key("web"));
    // Workers are x-tako services without a development hostname.
    assert!(refreshed.config.services.contains_key("worker"));
    assert_eq!(
        refreshed.config.routes("development"),
        ["api.feature.shop.test", "new.feature.shop.test"]
    );
    assert_eq!(refreshed.project, runtime.project);
}

use super::*;
use std::collections::BTreeSet;
use tako_compose::ComposeRuntime;

const APP: &str = "scale/production";

fn state(dir: &Path, standby: bool) -> ServerState {
    ServerState::new_with_runtime(
        dir.into(),
        Arc::new(CertManager::new(CertManagerConfig {
            cert_dir: dir.join("certs"),
            ..Default::default()
        })),
        None,
        empty_challenge_tokens(),
        ServerRuntimeConfig {
            standby,
            ..ServerRuntimeConfig::for_defaults(dir.into())
        },
    )
    .unwrap()
}

fn scale(count: Option<u8>, service: Option<&str>) -> Command {
    Command::Scale {
        app: APP.into(),
        instances: count,
        service: service.map(str::to_owned),
        reset: count.is_none(),
    }
}

fn ok(response: Response) -> Value {
    match response {
        Response::Ok { data } => data,
        Response::Error { .. } => panic!("Compose command should succeed"),
    }
}

async fn deploy(state: &ServerState, dir: &Path, version: &str) {
    ok(state
        .handle_command(
            serde_json::from_value(serde_json::json!({
                "command":"deploy", "app":APP, "version":version,
                "path":crate::release::app_release_root(dir, APP, version),
                "routes":["scale.localhost"]
            }))
            .unwrap(),
        )
        .await);
}

async fn assert_replicas(
    state: &ServerState,
    runtime: &ComposeRuntime,
    count: usize,
) -> BTreeSet<String> {
    let containers = runtime.containers().await.unwrap();
    let web: Vec<_> = containers
        .iter()
        .filter(|c| c.service == "web" && c.running())
        .collect();
    assert_eq!(web.len(), count);
    let app = state.app_manager.get_app(APP).unwrap();
    let instances = app.get_instances();
    assert_eq!(instances.len(), count);
    assert_eq!(app.config.read().min_instances, count as u32);
    let ids: BTreeSet<_> = web.iter().map(|c| c.id.clone()).collect();
    assert_eq!(
        instances
            .iter()
            .map(|i| i.compose_container_id().unwrap())
            .collect::<BTreeSet<_>>(),
        ids
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let mut ports = BTreeSet::new();
    for instance in instances {
        assert_eq!(instance.state(), InstanceState::Healthy);
        let endpoint = instance.endpoint().unwrap();
        ports.insert(endpoint.port());
        let body = client
            .get(format!("http://{endpoint}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!body.trim().is_empty());
    }
    assert_eq!(ports.len(), count);
    let routed: BTreeSet<_> = (0..count * 2)
        .map(|_| {
            state
                .load_balancer
                .get_backend(APP)
                .unwrap()
                .endpoint()
                .unwrap()
                .port()
        })
        .collect();
    assert_eq!(routed, ports);
    ids
}

async fn assert_dependency_logs(state: &ServerState, runtime: &ComposeRuntime, marker: &str) {
    let container = runtime
        .containers()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.service == "database")
        .unwrap();
    let output = tokio::process::Command::new("docker")
        .args([
            "exec",
            &container.id,
            "sh",
            "-c",
            "echo \"$1\" > /proc/1/fd/1",
            "fixture",
            marker,
        ])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let path = state
        .runtime
        .data_dir
        .join("apps")
        .join(APP)
        .join("logs/current.log");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains(marker)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("Dependency output must reach the group log");
}

/// Secrets change by redeploying the current release, with its own routes, and
/// a new snapshot.
async fn rotate_dependency_secret(
    state: &ServerState,
    runtime: &ComposeRuntime,
    routes: &[String],
    value: &str,
) {
    let data_dir = state.runtime.data_dir.clone();
    let version = state
        .app_manager
        .get_app(APP)
        .expect("deployed app")
        .config
        .read()
        .version
        .clone();
    let digest = crate::secret_snapshot::test_file::pin(
        &data_dir,
        APP,
        crate::secret_snapshot::test_file::TestScope {
            app: [("ROTATION".into(), value.into())].into(),
            ..Default::default()
        },
    );
    ok(state
        .handle_command(
            serde_json::from_value(serde_json::json!({
                "command":"deploy", "app":APP, "version":version,
                "path":crate::release::app_release_root(&data_dir, APP, &version),
                "routes":routes, "secrets_digest":digest
            }))
            .unwrap(),
        )
        .await);
    let container = runtime
        .containers()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.service == "database")
        .unwrap();
    let output = tokio::process::Command::new("docker")
        .args(["exec", &container.id, "printenv", "ROTATION"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout == format!("{value}\n").as_bytes());
}

#[tokio::test]
#[ignore = "requires Docker Compose with shared host/container loopback and project paths"]
async fn compose_service_scaling_survives_deploy_restart_and_failed_commit() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let data = crate::release::ensure_app_runtime_data_dirs(dir.path(), APP).unwrap();
    let compose = tako_core::compose::ComposeConfig {
        file: "compose.yaml".into(),
        services: serde_json::from_value(
            serde_json::json!({"web":{"envs":{"production":{"route":"scale.localhost"}}}}),
        )
        .unwrap(),
    };
    let mut runtimes = Vec::new();
    for (version, defaults) in [("v1", 1), ("v2", 3)] {
        let release = crate::release::app_release_root(dir.path(), APP, version);
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(
            release.join("app.json"),
            serde_json::json!({
                "protocol_version":tako_core::PROTOCOL_VERSION, "release_kind":"compose",
                "app_name":"scale", "environment":"production", "version":version,
                "runtime":"compose", "main":"", "idle_timeout":300, "compose":compose,
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            release.join("compose.yaml"),
            format!(
                r#"
services:
  web:
    expose: ["8080"]
    x-tako: {{envs: {{production: {{route: scale.localhost}}}}}}
    image: docker.io/library/busybox:1.37
    command: [sh, -c, 'mkdir -p /www; hostname > /www/index.html; exec httpd -f -p 8080 -h /www']
    deploy:
      replicas: {defaults}
      update_config: {{ monitor: 0s }}
  worker:
    image: docker.io/library/busybox:1.37
    command: [sleep, '3600']
    deploy:
      update_config: {{ monitor: 0s }}
    volumes: [data:/data]
volumes:
  data:
"#
            ),
        )
        .unwrap();
        let runtime = ComposeRuntime::new(
            release,
            compose.clone(),
            HashMap::from([(
                crate::release::TAKO_APP_DATA_DIR_ENV.into(),
                data.app.display().to_string(),
            )]),
        )
        .unwrap();
        runtime.prepare(version).await.unwrap();
        runtimes.push(runtime);
    }
    struct Cleanup(ComposeRuntime);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self
                .0
                .command(false)
                .into_std()
                .args(["down", "--volumes", "--remove-orphans"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _cleanup = Cleanup(runtimes[0].clone());
    let runtime = &runtimes[0];
    let server = state(dir.path(), false);
    deploy(&server, dir.path(), "v1").await;
    let initial = assert_replicas(&server, runtime, 1).await;
    let worker = runtime
        .containers()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.service == "worker")
        .unwrap();
    assert!(
        tokio::process::Command::new("docker")
            .args([
                "exec",
                &worker.id,
                "sh",
                "-c",
                "echo retained > /data/marker"
            ])
            .status()
            .await
            .unwrap()
            .success()
    );

    let result = ok(server.handle_command(scale(Some(5), None)).await);
    assert_eq!(result["service"], "web");
    assert_eq!(result["instances"], 5);
    let five = assert_replicas(&server, runtime, 5).await;
    assert!(initial.is_subset(&five));
    assert_eq!(
        runtime
            .containers()
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.service == "worker")
            .unwrap()
            .id,
        worker.id
    );
    for command in [
        scale(Some(0), None),
        scale(Some(129), None),
        scale(Some(2), Some("missing")),
    ] {
        assert!(matches!(
            server.handle_command(command).await,
            Response::Error { .. }
        ));
    }
    assert_eq!(assert_replicas(&server, runtime, 5).await, five);

    // A late durable commit failure must restore old containers and old override.
    let db = rusqlite::Connection::open(dir.path().join("state.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_scale BEFORE DELETE ON app_compose_pending BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
    for count in [7, 2] {
        assert!(matches!(
            server.handle_command(scale(Some(count), None)).await,
            Response::Error { .. }
        ));
    }
    assert_eq!(assert_replicas(&server, runtime, 5).await, five);
    assert_eq!(
        server.state_store.load_apps().unwrap()[0]
            .config
            .compose_replicas["web"],
        5
    );
    db.execute_batch("DROP TRIGGER fail_scale;").unwrap();

    deploy(&server, dir.path(), "v2").await;
    assert_eq!(assert_replicas(&server, runtime, 5).await, five);
    server.app_manager.shutdown_all().await;
    drop(server);
    let server = state(dir.path(), true);
    server.restore_from_state_store().await.unwrap();
    assert_eq!(assert_replicas(&server, runtime, 5).await, five);
    // Rolling back code preserves the override too.
    deploy(&server, dir.path(), "v1").await;
    assert_eq!(assert_replicas(&server, runtime, 5).await, five);

    // Removing one HTTP replica through health recovery adopts its siblings.
    let app = server.app_manager.get_app(APP).unwrap();
    let failed = app.get_instances()[0].clone();
    failed.kill().await.unwrap();
    crate::runtime_events::handle_health_event(
        &server,
        crate::instances::HealthEvent::Dead {
            app: APP.into(),
            instance_id: failed.id.clone(),
        },
    )
    .await;
    assert_eq!(assert_replicas(&server, runtime, 5).await, five);

    ok(server.handle_command(scale(Some(3), Some("worker"))).await);
    assert_eq!(assert_replicas(&server, runtime, 5).await, five);
    assert_eq!(
        runtime
            .containers()
            .await
            .unwrap()
            .iter()
            .filter(|c| c.service == "worker")
            .count(),
        3
    );
    ok(server.handle_command(scale(Some(0), Some("worker"))).await);
    assert!(
        runtime
            .containers()
            .await
            .unwrap()
            .iter()
            .all(|c| c.service != "worker")
    );
    ok(server.handle_command(scale(None, Some("worker"))).await);
    let worker = runtime
        .containers()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.service == "worker")
        .unwrap();
    let marker = tokio::process::Command::new("docker")
        .args(["exec", &worker.id, "cat", "/data/marker"])
        .output()
        .await
        .unwrap();
    assert_eq!(String::from_utf8(marker.stdout).unwrap().trim(), "retained");

    deploy(&server, dir.path(), "v2").await;
    // Both retiring replicas must leave routing together, while each keeps
    // its own in-flight request alive until request accounting releases it.
    let retiring_ids: BTreeSet<_> = runtime
        .containers()
        .await
        .unwrap()
        .into_iter()
        .filter(|c| c.service == "web" && c.slot >= 3)
        .map(|c| c.id)
        .collect();
    let retiring: Vec<_> = app
        .get_instances()
        .into_iter()
        .filter(|i| {
            i.compose_container_id()
                .is_some_and(|id| retiring_ids.contains(&id))
        })
        .collect();
    assert_eq!(retiring.len(), 2);
    for instance in &retiring {
        instance.request_started();
    }
    let (response, ()) = tokio::join!(server.handle_command(scale(None, Some("web"))), async {
        tokio::time::timeout(Duration::from_secs(60), async {
            while !retiring
                .iter()
                .all(|i| i.state() == InstanceState::Draining)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let current = runtime.containers().await.unwrap();
        assert!(
            retiring_ids
                .iter()
                .all(|id| current.iter().any(|c| &c.id == id && c.running()))
        );
        for _ in 0..10 {
            let backend = server.load_balancer.get_backend(APP).unwrap();
            assert!(retiring.iter().all(|i| backend.instance_id() != i.id));
        }
        for instance in &retiring {
            instance.request_finished();
        }
    });
    let reset = ok(response);
    assert_eq!(reset["instances"], 3);
    let three = assert_replicas(&server, runtime, 3).await;
    assert!(three.is_subset(&five));
    assert!(
        server.state_store.load_apps().unwrap()[0]
            .config
            .compose_replicas
            .is_empty()
    );
    deploy(&server, dir.path(), "v1").await;
    assert_eq!(assert_replicas(&server, runtime, 1).await, initial);
    server.app_manager.stop_app(APP).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Docker Compose with shared host/container loopback and project paths"]
async fn compose_group_routes_status_scaling_and_restart_keep_app_identity() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let data = crate::release::ensure_app_runtime_data_dirs(dir.path(), APP).unwrap();
    let release = crate::release::app_release_root(dir.path(), APP, "v1");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(release.join("compose.yaml"), r#"
services:
  web:
    image: docker.io/library/busybox:1.37
    expose: [8080]
    depends_on: [api]
    x-tako:
      envs:
        production:
          route: web.localhost
    command: [sh, -c, 'mkdir -p /www; echo frontend > /www/index.html; exec httpd -f -p 8080 -h /www']
    deploy: {update_config: {monitor: 0s}}
  api:
    image: docker.io/library/busybox:1.37
    expose: [9090]
    depends_on: [database]
    x-tako:
      envs:
        production:
          routes: [api.localhost, web.localhost/api/*]
    command: [sh, -c, 'mkdir -p /www; echo backend > /www/index.html; exec httpd -f -p 9090 -h /www']
    deploy: {update_config: {monitor: 0s}}
  database:
    image: docker.io/library/busybox:1.37
    environment: {ROTATION: "${ROTATION:-initial}"}
    command: [sleep, '3600']
    deploy: {update_config: {monitor: 0s}}
"#).unwrap();
    let compose = tako_compose::load_config(&release, "compose.yaml").unwrap();
    std::fs::write(
        release.join("app.json"),
        serde_json::json!({
            "protocol_version":tako_core::PROTOCOL_VERSION, "release_kind":"compose",
            "app_name":"scale", "environment":"production", "version":"v1",
            "runtime":"compose", "main":"", "idle_timeout":300, "compose":compose,
        })
        .to_string(),
    )
    .unwrap();
    retain_test_release(&release);
    let runtime = ComposeRuntime::new(
        release.clone(),
        compose.clone(),
        HashMap::from([(
            crate::release::TAKO_APP_DATA_DIR_ENV.into(),
            data.app.display().to_string(),
        )]),
    )
    .unwrap();
    struct Cleanup(ComposeRuntime);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self
                .0
                .command(false)
                .into_std()
                .args(["down", "--volumes", "--remove-orphans"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _cleanup = Cleanup(runtime.clone());
    runtime.prepare("v1").await.unwrap();
    let server = state(dir.path(), false);
    ok(server
        .handle_command(
            serde_json::from_value(serde_json::json!({
                "command":"deploy", "app":APP, "version":"v1", "path":release,
                "routes":compose.routes("production")
            }))
            .unwrap(),
        )
        .await);
    async fn check(server: &ServerState) {
        for (host, expected) in [("web.localhost", "frontend"), ("api.localhost", "backend")] {
            let route = server.routes.read().select_with_route(host, "/").unwrap();
            let backend = server
                .load_balancer
                .get_service_backend(&route.app, route.service.as_deref().unwrap())
                .unwrap();
            let response = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(format!("http://{}/", backend.endpoint().unwrap()))
                .header("Host", host)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert_eq!(response.trim(), expected);
        }
        assert_eq!(
            server
                .routes
                .read()
                .select_with_route("web.localhost", "/api/orders")
                .unwrap()
                .service
                .as_deref(),
            Some("api")
        );
        // An ambiguous group request must not balance between unrelated apps.
        assert!(server.load_balancer.get_backend(APP).is_none());
        let status = ok(server.get_status(APP).await);
        assert_eq!(status["kind"], "application_group");
        assert_eq!(status["applications"].as_array().unwrap().len(), 2);
        assert!(
            status["applications"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| a["service"] != "database")
        );
        assert!(
            status["services"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["service"] == "database")
        );
    }
    check(&server).await;
    let before = runtime.containers().await.unwrap();
    assert!(matches!(
        server.handle_command(scale(Some(2), None)).await,
        Response::Error { .. }
    ));
    ok(server.handle_command(scale(Some(2), Some("api"))).await);
    let after = runtime.containers().await.unwrap();
    assert_eq!(after.iter().filter(|c| c.service == "api").count(), 2);
    for service in ["web", "database"] {
        assert_eq!(
            before.iter().find(|c| c.service == service).unwrap().id,
            after.iter().find(|c| c.service == service).unwrap().id
        );
    }
    check(&server).await;
    let mut selected_api_replicas = BTreeSet::new();
    for _ in 0..12 {
        server
            .load_balancer
            .get_service_backend(APP, "web")
            .unwrap();
        selected_api_replicas.insert(
            server
                .load_balancer
                .get_service_backend(APP, "api")
                .unwrap()
                .instance_id()
                .to_owned(),
        );
    }
    assert_eq!(
        selected_api_replicas.len(),
        2,
        "sibling traffic must not skew round robin"
    );
    // Individual lifecycle operations preserve siblings, routes and desired scale.
    ok(server
        .handle_command(Command::Stop {
            app: APP.into(),
            service: Some("api".into()),
        })
        .await);
    assert!(
        server
            .load_balancer
            .get_service_backend(APP, "api")
            .is_none()
    );
    assert!(
        server
            .load_balancer
            .get_service_backend(APP, "web")
            .is_some()
    );
    assert!(
        server
            .routes
            .read()
            .select_with_route("api.localhost", "/")
            .is_some()
    );
    let stopped = runtime.containers().await.unwrap();
    assert!(stopped.iter().all(|c| c.service != "api"));
    for service in ["web", "database"] {
        assert_eq!(
            after.iter().find(|c| c.service == service).unwrap().id,
            stopped.iter().find(|c| c.service == service).unwrap().id
        );
    }
    assert_dependency_logs(&server, &runtime, "after-first-app-stop").await;
    let status = ok(server.get_status(APP).await);
    assert_eq!(
        status["applications"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["service"] == "api")
            .unwrap()["state"],
        "stopped"
    );
    assert!(matches!(
        server
            .handle_command(Command::Stop {
                app: APP.into(),
                service: Some("database".into())
            })
            .await,
        Response::Error { .. }
    ));
    // A failed start cannot clear durable stop intent or affect siblings.
    rusqlite::Connection::open(dir.path().join("state.sqlite")).unwrap().execute_batch("CREATE TRIGGER fail_lifecycle BEFORE UPDATE ON app_lifecycle BEGIN SELECT RAISE(ABORT, 'injected lifecycle failure'); END;").unwrap();
    assert!(matches!(
        server
            .handle_command(Command::Start {
                app: APP.into(),
                service: Some("api".into())
            })
            .await,
        Response::Error { .. }
    ));
    rusqlite::Connection::open(dir.path().join("state.sqlite"))
        .unwrap()
        .execute_batch("DROP TRIGGER fail_lifecycle;")
        .unwrap();
    assert!(
        server
            .app_manager
            .get_app(APP)
            .unwrap()
            .config
            .read()
            .compose_stopped
            .contains("api")
    );
    assert!(
        runtime
            .containers()
            .await
            .unwrap()
            .iter()
            .all(|c| c.service != "api")
    );
    let scale_result = ok(server.handle_command(scale(Some(3), Some("api"))).await);
    assert_eq!(scale_result["instances"], 3);
    assert_eq!(scale_result["stopped"], true);
    assert!(
        runtime
            .containers()
            .await
            .unwrap()
            .iter()
            .all(|c| c.service != "api")
    );
    ok(server
        .handle_command(
            serde_json::from_value(serde_json::json!({
                "command":"deploy", "app":APP, "version":"v1", "path":release,
                "routes":compose.routes("production")
            }))
            .unwrap(),
        )
        .await);
    assert!(
        runtime
            .containers()
            .await
            .unwrap()
            .iter()
            .all(|c| c.service != "api")
    );
    server.app_manager.shutdown_all().await;
    drop(server);
    let restarted = state(dir.path(), false);
    restarted.restore_from_state_store().await.unwrap();
    // SQLite loads routes in lexical order, independently of service order.
    rotate_dependency_secret(
        &restarted,
        &runtime,
        &compose.routes("production"),
        "after-restart",
    )
    .await;
    ok(restarted
        .handle_command(Command::Stop {
            app: APP.into(),
            service: Some("web".into()),
        })
        .await);
    assert!(
        restarted
            .app_manager
            .get_app(APP)
            .unwrap()
            .get_instances()
            .is_empty()
    );
    rotate_dependency_secret(
        &restarted,
        &runtime,
        &compose.routes("production"),
        "all-paused",
    )
    .await;
    assert_dependency_logs(&restarted, &runtime, "all-http-apps-paused").await;
    ok(restarted
        .handle_command(Command::Start {
            app: APP.into(),
            service: Some("web".into()),
        })
        .await);
    assert!(
        restarted
            .load_balancer
            .get_service_backend(APP, "api")
            .is_none()
    );
    assert!(
        restarted
            .load_balancer
            .get_service_backend(APP, "web")
            .is_some()
    );
    ok(restarted
        .handle_command(Command::Stop {
            app: APP.into(),
            service: None,
        })
        .await);
    assert_eq!(
        restarted.app_manager.get_app(APP).unwrap().state(),
        AppState::Stopped
    );
    assert!(runtime.containers().await.unwrap().is_empty());
    restarted.app_manager.shutdown_all().await;
    drop(restarted);
    let restarted = state(dir.path(), false);
    restarted.restore_from_state_store().await.unwrap();
    assert_eq!(
        restarted.app_manager.get_app(APP).unwrap().state(),
        AppState::Stopped
    );
    assert!(runtime.containers().await.unwrap().is_empty());
    ok(restarted
        .handle_command(Command::Start {
            app: APP.into(),
            service: None,
        })
        .await);
    assert!(
        restarted
            .load_balancer
            .get_service_backend(APP, "api")
            .is_none(),
        "group start preserves explicit child stops"
    );
    assert!(
        restarted
            .load_balancer
            .get_service_backend(APP, "web")
            .is_some()
    );
    ok(restarted
        .handle_command(Command::Start {
            app: APP.into(),
            service: Some("api".into()),
        })
        .await);
    check(&restarted).await;
    assert_eq!(
        runtime
            .containers()
            .await
            .unwrap()
            .iter()
            .filter(|c| c.service == "api")
            .count(),
        3
    );
    assert_eq!(
        restarted
            .app_manager
            .get_app(APP)
            .unwrap()
            .config
            .read()
            .compose_replicas["api"],
        3
    );

    // A Docker-only group has no app routes/backends, but still recovers its services.
    let private_release = crate::release::app_release_root(dir.path(), APP, "v2");
    std::fs::create_dir_all(&private_release).unwrap();
    std::fs::write(
        private_release.join("compose.yaml"),
        serde_json::json!({
            "services": {"database": {"image": "docker.io/library/busybox:1.37",
                "environment": {"ROTATION":"${ROTATION:-initial}"}, "command": ["sleep", "3600"], "deploy": {"update_config": {"monitor": "0s"}}}}
        })
        .to_string(),
    )
    .unwrap();
    let private_compose = tako_compose::load_config(&private_release, "compose.yaml").unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(release.join("app.json")).unwrap()).unwrap();
    manifest["version"] = serde_json::json!("v2");
    manifest["compose"] = serde_json::to_value(&private_compose).unwrap();
    std::fs::write(private_release.join("app.json"), manifest.to_string()).unwrap();
    retain_test_release(&private_release);
    let private_runtime = ComposeRuntime::new(
        private_release.clone(),
        private_compose,
        HashMap::from([(
            crate::release::TAKO_APP_DATA_DIR_ENV.into(),
            data.app.display().to_string(),
        )]),
    )
    .unwrap();
    private_runtime.prepare("v2").await.unwrap();
    ok(restarted
        .prepare_deploy(
            APP,
            "v2",
            &private_release.to_string_lossy(),
            vec![],
            tako_core::SslBinding::default(),
            false,
        )
        .await);
    ok(restarted
        .handle_command(
            serde_json::from_value(serde_json::json!({
                "command":"deploy", "app":APP, "version":"v2", "path":private_release, "routes":[]
            }))
            .unwrap(),
        )
        .await);
    assert!(
        restarted
            .routes
            .read()
            .select_with_route("web.localhost", "/")
            .is_none()
    );
    assert_eq!(
        restarted
            .app_manager
            .get_app(APP)
            .unwrap()
            .config
            .read()
            .min_instances,
        0
    );
    restarted.app_manager.shutdown_all().await;
    drop(restarted);
    let private_server = state(dir.path(), false);
    private_server.restore_from_state_store().await.unwrap();
    let status = ok(private_server.get_status(APP).await);
    assert_eq!(status["kind"], "application_group");
    assert!(
        serde_json::from_value::<tako_core::AppStatus>(status)
            .unwrap()
            .applications
            .is_empty()
    );
    assert_eq!(
        private_server.app_manager.get_app(APP).unwrap().state(),
        AppState::Running
    );
    let containers = private_runtime.containers().await.unwrap();
    assert_eq!(containers.len(), 1);
    assert!(containers.iter().all(|c| c.running() && c.port.is_none()));
    rotate_dependency_secret(&private_server, &private_runtime, &[], "docker-only").await;
    assert_dependency_logs(
        &private_server,
        &private_runtime,
        "docker-only-dependencies",
    )
    .await;
    ok(private_server.rollback_app(APP, "v1").await);
    check(&private_server).await;
    ok(private_server.rollback_app(APP, "v2").await);
    assert!(
        private_server
            .routes
            .read()
            .select_with_route("web.localhost", "/")
            .is_none()
    );
    private_server.app_manager.stop_app(APP).await.unwrap();
}

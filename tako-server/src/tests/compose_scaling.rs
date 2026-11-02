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

#[tokio::test]
#[ignore = "requires Docker Compose with shared host/container loopback and project paths"]
async fn compose_service_scaling_survives_deploy_restart_and_failed_commit() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let data = crate::release::ensure_app_runtime_data_dirs(dir.path(), APP).unwrap();
    let compose = tako_core::compose::ComposeConfig {
        file: "compose.yaml".into(),
        service: "web".into(),
        port: 8080,
        health_path: "/".into(),
        health_host: None,
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

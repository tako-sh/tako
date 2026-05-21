use super::*;

#[tokio::test]
async fn scale_command_persists_zero_instances_across_restore() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));

    let state_a = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager.clone(),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let release_dir = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    std::fs::write(
        release_dir.join("app.json"),
        r#"{"runtime":"node","main":"index.js","idle_timeout":300,"start":["/bin/sh","-lc","sleep 600"]}"#,
    )
    .unwrap();

    let app = state_a.app_manager.register_app(AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 2,
        max_instances: 4,
        idle_timeout: Duration::from_secs(300),
        ..Default::default()
    });
    state_a.load_balancer.register_app(app.clone());
    {
        let mut route_table = state_a.routes.write().await;
        route_table.set_app_routes("my-app".to_string(), vec!["api.example.com".to_string()]);
    }

    let first = app.allocate_instance();
    first.set_state(InstanceState::Healthy);
    let second = app.allocate_instance();
    second.set_state(InstanceState::Healthy);

    let response = state_a
        .handle_command(Command::Scale {
            app: "my-app".to_string(),
            instances: 0,
        })
        .await;
    assert!(matches!(response, Response::Ok { .. }));
    assert_eq!(app.config.read().min_instances, 0);
    assert!(app.get_instances().is_empty());

    drop(state_a);

    let state_b = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    state_b.restore_from_state_store().await.unwrap();

    let restored = state_b.app_manager.get_app("my-app").expect("app restored");
    assert_eq!(restored.config.read().min_instances, 0);
    assert_eq!(restored.state(), AppState::Idle);
}

#[tokio::test]
async fn deploy_preserves_scaled_instance_count() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let current_release = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&current_release).unwrap();
    std::fs::write(
        current_release.join("app.json"),
        r#"{"runtime":"node","main":"index.js","idle_timeout":300,"start":["/bin/sh","-lc","sleep 600"]}"#,
    )
    .unwrap();

    let app = state.app_manager.register_app(AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        path: current_release.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 2,
        max_instances: 4,
        idle_timeout: Duration::from_secs(300),
        ..Default::default()
    });
    state.load_balancer.register_app(app.clone());
    {
        let mut route_table = state.routes.write().await;
        route_table.set_app_routes("my-app".to_string(), vec!["api.example.com".to_string()]);
    }

    let old_instance = app.allocate_instance();
    old_instance.set_state(InstanceState::Healthy);

    let broken_release = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("releases")
        .join("v2");
    std::fs::create_dir_all(&broken_release).unwrap();
    std::fs::write(
        broken_release.join("app.json"),
        r#"{"runtime":"node","main":"index.js","idle_timeout":300,"start":["/bin/sh","-lc","exit 1"]}"#,
    )
    .unwrap();

    let response = state
        .handle_command(Command::Deploy {
            app: "my-app".to_string(),
            version: "v2".to_string(),
            path: broken_release.to_string_lossy().to_string(),
            routes: vec!["api.example.com".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            storages: Some(HashMap::new()),
            dns: None,
        })
        .await;

    assert!(matches!(response, Response::Error { .. }));
    assert_eq!(app.config.read().min_instances, 2);
}

#[tokio::test]
async fn delete_command_removes_persisted_state_for_next_boot() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state_a = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager.clone(),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release_dir = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    write_release_manifest(
        &release_dir,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );
    let app = state_a.app_manager.register_app(AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 0,
        ..Default::default()
    });
    state_a.load_balancer.register_app(app);
    {
        let mut route_table = state_a.routes.write().await;
        route_table.set_app_routes(
            "my-app/production".to_string(),
            vec!["api.example.com".to_string()],
        );
    }
    state_a.persist_app_state("my-app/production").await;

    let response = state_a
        .handle_command(Command::Delete {
            app: "my-app/production".to_string(),
        })
        .await;
    assert!(matches!(response, Response::Ok { .. }));

    let state_b = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    state_b.restore_from_state_store().await.unwrap();
    assert!(state_b.app_manager.get_app("my-app/production").is_none());
}

#[tokio::test]
async fn deploy_on_demand_validates_startup_and_fails_for_unhealthy_build() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release_dir = temp
        .path()
        .join("apps")
        .join("broken-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    std::fs::write(
        release_dir.join("app.json"),
        r#"{"runtime":"node","main":"index.js","idle_timeout":300,"install":"true","start":["/bin/sh","-lc","exit 1"]}"#,
    )
    .unwrap();

    let response = state
        .handle_command(Command::Deploy {
            app: "broken-app".to_string(),
            version: "v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["broken.localhost".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            storages: Some(HashMap::new()),
            dns: None,
        })
        .await;

    assert!(
        matches!(response, Response::Error { .. }),
        "expected startup validation failure for on-demand deploy: {response:?}"
    );
}

// TODO: This test needs a rewrite to work with the plugin-derived launch
// command. The fake bun script exits immediately because the spawner's
// binary resolution doesn't find the fake bun via the manifest's PATH.
// The deploy lifecycle is fully covered by e2e tests (e2e/fixtures/).
#[tokio::test]
#[ignore = "needs rewrite for plugin architecture"]
async fn deploy_on_demand_keeps_one_warm_instance_after_successful_deploy() {
    if !python3_ok() || !python3_can_bind_loopback_tcp() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let runtime = ServerRuntimeConfig {
        socket: "/tmp/tako-warm.sock".to_string(),
        ..ServerRuntimeConfig::for_defaults(temp.path().to_path_buf())
    };
    let state = ServerState::new_with_runtime(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
        runtime,
    )
    .unwrap();

    let fake_bin_dir = temp.path().join("bin");
    std::fs::create_dir_all(&fake_bin_dir).unwrap();
    let fake_bun = fake_bin_dir.join("bun");
    let fake_server_py = temp.path().join("server.py");
    std::fs::write(
        &fake_server_py,
        r#"import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer

port = int(os.environ.get("PORT") or "0")
with os.fdopen(3, "r") as _bootstrap_fd:
    _bootstrap = json.load(_bootstrap_fd)
internal_token = _bootstrap.get("token") or ""
if not port or not internal_token:
raise SystemExit("PORT and fd 3 bootstrap token are required")

class Handler(BaseHTTPRequestHandler):
def do_GET(self):
    if self.path == "/status" and (self.headers.get("Host") or "").split(":")[0].lower() == "tako":
        if self.headers.get("X-Tako-Internal-Token") != internal_token:
            self.send_response(403)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error":"forbidden"}')
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("X-Tako-Internal-Token", internal_token)
        self.end_headers()
        self.wfile.write(b'{"status":"ok"}')
        return
    self.send_response(404)
    self.end_headers()

def log_message(self, format, *args):
    return

HTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#,
    )
    .unwrap();
    std::fs::write(
        &fake_bun,
        format!(
            "#!/bin/sh\ncase \"$1\" in install) exit 0;; esac\nexec python3 {}\n",
            fake_server_py.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&fake_bun).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_bun, permissions).unwrap();
    }

    let release_dir = temp
        .path()
        .join("apps")
        .join("warm-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    std::fs::write(
        release_dir.join("package.json"),
        r#"{"name":"warm-app","scripts":{"dev":"bun run index.ts"}}"#,
    )
    .unwrap();
    std::fs::write(release_dir.join("index.ts"), "export default {};\n").unwrap();
    std::fs::create_dir_all(release_dir.join("node_modules/tako.sh/dist/entrypoints")).unwrap();
    std::fs::write(
        release_dir.join("node_modules/tako.sh/dist/entrypoints/bun-server.mjs"),
        "export default {};",
    )
    .unwrap();
    // Include PATH in the manifest env_vars so that the spawned instance
    // can find the fake bun binary.  Also set runtime_bin to the absolute
    // path so resolve_runtime_binary picks it up directly.
    let path_with_fake = format!(
        "{}:{}",
        fake_bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    std::fs::write(
        release_dir.join("app.json"),
        serde_json::json!({
            "runtime": "bun",
            "main": "index.ts",
            "idle_timeout": 300,
            "env_vars": { "PATH": &path_with_fake }
        })
        .to_string(),
    )
    .unwrap();

    let app = state.app_manager.register_app(AppConfig {
        name: "warm-app".to_string(),
        version: "v0".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "exit 0".to_string(),
        ],
        min_instances: 0,
        max_instances: 4,
        ..Default::default()
    });
    state.load_balancer.register_app(app);

    let response = state
        .handle_command(Command::Deploy {
            app: "warm-app".to_string(),
            version: "v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["warm.localhost".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            storages: Some(HashMap::new()),
            dns: None,
        })
        .await;
    assert!(
        matches!(response, Response::Ok { .. }),
        "expected successful on-demand deploy: {response:?}"
    );

    let status = state
        .handle_command(Command::Status {
            app: "warm-app".to_string(),
        })
        .await;
    let Response::Ok { data } = status else {
        panic!("expected status response for warm-app");
    };

    assert_eq!(data.get("state").and_then(Value::as_str), Some("running"));
    let instances = data
        .get("instances")
        .and_then(Value::as_array)
        .expect("status should include instances");
    assert_eq!(instances.len(), 1);
}

#[tokio::test]
async fn instance_idle_event_resets_cold_start_when_app_scales_to_zero() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let app = state.app_manager.register_app(AppConfig {
        name: "idle-app".to_string(),
        version: "v1".to_string(),
        min_instances: 0,
        ..Default::default()
    });
    state.load_balancer.register_app(app.clone());
    app.set_state(AppState::Running);

    let instance = app.allocate_instance();
    instance.set_state(InstanceState::Healthy);

    // Simulate a prior successful cold start.
    state.cold_start.begin("idle-app");
    state.cold_start.mark_ready("idle-app");
    assert!(!state.cold_start.begin("idle-app").leader);

    handle_idle_event(
        &state,
        crate::scaling::IdleEvent::InstanceIdle {
            app: "idle-app".to_string(),
            instance_id: instance.id.clone(),
        },
    )
    .await;

    assert!(app.get_instances().is_empty());
    assert_eq!(app.state(), AppState::Idle);
    assert!(state.cold_start.begin("idle-app").leader);
}

#[tokio::test]
async fn instance_ready_event_sets_health_metric() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let app = state.app_manager.register_app(AppConfig {
        name: "metrics-app".to_string(),
        version: "v1".to_string(),
        min_instances: 1,
        ..Default::default()
    });
    state.load_balancer.register_app(app.clone());
    app.set_state(AppState::Running);

    let instance = app.allocate_instance();
    // Spawner sets state to Healthy directly before emitting Ready.
    instance.set_state(InstanceState::Healthy);

    handle_instance_event(
        &state,
        crate::instances::InstanceEvent::Ready {
            app: "metrics-app".to_string(),
            instance_id: instance.id.clone(),
        },
    )
    .await;

    let health = crate::metrics::INSTANCE_HEALTH
        .with_label_values(&[crate::metrics::server(), "metrics-app", &instance.id])
        .get();
    assert_eq!(
        health, 1,
        "InstanceEvent::Ready should set tako_instance_health to 1"
    );

    let running = crate::metrics::INSTANCES_RUNNING
        .with_label_values(&[crate::metrics::server(), "metrics-app"])
        .get();
    assert_eq!(
        running, 1,
        "InstanceEvent::Ready should update tako_instances_running"
    );
}

#[tokio::test]
async fn status_includes_running_builds_for_each_version() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let app = state.app_manager.register_app(AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        min_instances: 0,
        ..Default::default()
    });

    let old = app.allocate_instance();
    old.set_state(InstanceState::Healthy);

    let mut cfg = app.config.read().clone();
    cfg.version = "v2".to_string();
    app.update_config(cfg);

    let new = app.allocate_instance();
    new.set_state(InstanceState::Healthy);

    let response = state
        .handle_command(Command::Status {
            app: "my-app".to_string(),
        })
        .await;

    let Response::Ok { data } = response else {
        panic!("expected ok status response");
    };

    let builds = data
        .get("builds")
        .and_then(Value::as_array)
        .expect("status should include builds");
    let versions: Vec<&str> = builds
        .iter()
        .filter_map(|b| b.get("version").and_then(Value::as_str))
        .collect();
    assert!(
        versions.contains(&"v1") && versions.contains(&"v2"),
        "expected status to include both running builds: {data}"
    );
}

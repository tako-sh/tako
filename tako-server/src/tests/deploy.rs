use super::*;
use crate::isolation::fixture::TestDataDir as TempDir;

#[tokio::test]
async fn native_deploy_keeps_previous_release_on_failed_commit_and_restart() {
    if !python3_ok() || !python3_can_bind_loopback_tcp() {
        return;
    }
    async fn served_build(backend: &crate::lb::Backend) -> String {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{}/", backend.endpoint().unwrap()))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap()
    }
    let temp = TempDir::new().unwrap();
    let certs = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let open_server = || {
        ServerState::new(
            temp.path().into(),
            certs.clone(),
            None,
            empty_challenge_tokens(),
        )
        .unwrap()
    };
    let state = open_server();
    let app_id = "commit-test/production";
    let release = |version: &str| crate::release::app_release_root(temp.path(), app_id, version);
    let deploy = |version: &str| Command::Deploy {
        app: app_id.into(),
        version: version.into(),
        path: release(version).display().to_string(),
        routes: vec![format!("{version}.localhost")],
        source_ip: Default::default(),
        secrets: Some(HashMap::from([("secret".into(), version.into())])),
        runtime_credentials: Some(HashMap::from([("key".into(), version.into())])),
        storages: None,
        ssl: Default::default(),
        backup: None,
        force: false,
    };
    for version in ["v1", "v2"] {
        let path = release(version);
        std::fs::create_dir_all(path.join("public")).unwrap();
        std::fs::write(path.join("public/build.txt"), version).unwrap();
        std::fs::write(
            path.join("app.py"),
            r#"import json, os
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
with os.fdopen(3) as bootstrap:
    token = json.load(bootstrap)["token"]
build = (Path(__file__).parent / "public" / "build.txt").read_bytes()
class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("X-Tako-Internal-Token", token)
        self.end_headers()
        self.wfile.write(build)
    def log_message(self, *args):
        pass
server = HTTPServer(("127.0.0.1", 0), Handler)
with os.fdopen(4, "w") as ready:
    print(server.server_port, file=ready)
server.serve_forever()
"#,
        )
        .unwrap();
        write_release_manifest(&path, "go", "app.py", &["python3", "{main}"], None, 300);
    }

    // Abort at the last binding write, after SQLite has updated the app, its
    // routes and other encrypted bindings inside the same transaction.
    let db = tako_sqlite::open_local(&temp.path().join("state.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_first BEFORE INSERT ON app_runtime_credentials
        BEGIN SELECT RAISE(ABORT, 'first commit failed'); END;",
    )
    .unwrap();
    let first = state.handle_command(deploy("v1")).await;
    assert!(
        first
            .error_message()
            .unwrap()
            .contains("first commit failed")
    );
    assert!(state.app_manager.get_app(app_id).is_none());
    assert!(state.routes.read().routes_for_app(app_id).is_empty());
    assert!(state.state_store.load_apps().unwrap().is_empty());
    assert!(state.state_store.get_secrets(app_id).unwrap().is_empty());
    db.execute_batch("DROP TRIGGER reject_first").unwrap();

    let baseline = state.handle_command(deploy("v1")).await;
    assert!(baseline.is_ok(), "{baseline:?}");
    let app = state.app_manager.get_app(app_id).unwrap();
    let backend = state.load_balancer.get_backend(app_id).unwrap();
    let predecessor = app.get_instance(backend.instance_id()).unwrap();
    assert_eq!(served_build(&backend).await, "v1");

    // A declared but incomplete worker must reject the new release even when
    // the HTTP executable is healthy. The active supervisor and metadata stay put.
    let manifest = release("v2").join("app.json");
    let original = std::fs::read_to_string(&manifest).unwrap();
    let mut metadata: Value = serde_json::from_str(&original).unwrap();
    metadata["workflow_worker_main"] = serde_json::json!("missing-worker");
    std::fs::write(&manifest, metadata.to_string()).unwrap();
    let workflow_error = state.handle_command(deploy("v2")).await;
    assert!(
        workflow_error
            .error_message()
            .unwrap()
            .contains("Go worker binary not found")
    );
    assert_eq!(app.version(), "v1");
    std::fs::write(manifest, original).unwrap();

    db.execute_batch(
        "CREATE TRIGGER reject_replacement BEFORE UPDATE ON app_runtime_credentials
        BEGIN SELECT RAISE(ABORT, 'replacement commit failed'); END;",
    )
    .unwrap();
    let check_previous = async {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !app.get_instances().iter().any(|instance| {
                instance.build_version() == "v2" && instance.state() == InstanceState::Healthy
            }) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("candidate reached readiness before the stability window");
        assert_eq!(state.routes.read().routes_for_app(app_id), ["v1.localhost"]);
        assert_eq!(
            std::fs::read_to_string(app.config.read().path.join("public/build.txt")).unwrap(),
            "v1"
        );
        let active = state.load_balancer.get_backend(app_id).unwrap();
        assert_eq!(active.instance_id(), predecessor.id);
        assert_eq!(served_build(&active).await, "v1");
        assert_eq!(
            state.state_store.get_secrets(app_id).unwrap()["secret"],
            "v1"
        );
    };
    let (failed, ()) = tokio::join!(state.handle_command(deploy("v2")), check_previous);
    assert!(
        failed
            .error_message()
            .unwrap()
            .contains("replacement commit failed")
    );
    assert_eq!(app.get_instances().len(), 1);
    assert_eq!(predecessor.state(), InstanceState::Healthy);
    assert!(predecessor.is_alive().await);
    assert_eq!(
        state
            .load_balancer
            .get_backend(app_id)
            .unwrap()
            .instance_id(),
        predecessor.id
    );
    assert_eq!(app.version(), "v1");
    assert_eq!(
        state.state_store.get_runtime_credentials(app_id).unwrap()["key"],
        "v1"
    );

    assert_eq!(
        served_build(&state.load_balancer.get_backend(app_id).unwrap()).await,
        "v1"
    );
    state.app_manager.shutdown_all().await;
    drop(state);
    let restarted = open_server();
    restarted.restore_apps_from_state_store(true).await.unwrap();
    let restored = restarted.app_manager.get_app(app_id).unwrap();
    assert_eq!(restored.version(), "v1");
    assert_eq!(restored.config.read().secrets["secret"], "v1");
    assert_eq!(
        restarted.routes.read().routes_for_app(app_id),
        ["v1.localhost"]
    );
    let backend = restarted.load_balancer.get_backend(app_id).unwrap();
    let process = restored.get_instance(backend.instance_id()).unwrap();
    assert_eq!(process.build_version(), "v1");
    assert_eq!(served_build(&backend).await, "v1");

    // Once scaled to zero, a request can cold-start v1 while v2 is preparing.
    // The late v1 process must be included in the final cutover and drain.
    db.execute_batch("DROP TRIGGER reject_replacement").unwrap();
    restarted.app_manager.stop_app(app_id).await.unwrap();
    restored.config.write().min_instances = 0;
    restored.set_state(AppState::Idle);
    let candidate = release("v2").join("app.py");
    let source = std::fs::read_to_string(&candidate).unwrap();
    std::fs::write(
        &candidate,
        source.replace(
            "with os.fdopen(4",
            r#"
import time
gate = Path(os.environ["TAKO_DATA_DIR"]) / "allow-ready"
while not gate.exists():
    time.sleep(0.01)
with os.fdopen(4"#,
        ),
    )
    .unwrap();
    let request_during_deploy = async {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !restored
                .get_instances()
                .iter()
                .any(|i| i.build_version() == "v2")
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut events = restarted.app_manager.take_event_receiver().unwrap();
        let request = crate::proxy::resolve_app_backend(
            &restarted.load_balancer,
            &restarted.cold_start,
            &restarted.drain_requested,
            &restarted.spawn_gate,
            app_id,
        );
        let completion = async {
            while let Some(event) = events.recv().await {
                let ready = matches!(&event, crate::instances::InstanceEvent::Ready { instance_id, .. } if
                    restored.get_instance(instance_id).is_some_and(|i| i.build_version() == "v1"));
                handle_instance_event(&restarted, event).await;
                if ready {
                    break;
                }
            }
        };
        let (resolution, ()) = tokio::join!(request, completion);
        let crate::proxy::BackendResolution::Ready { backend, .. } = resolution else {
            panic!("idle predecessor should serve requests during deployment");
        };
        assert_eq!(served_build(&backend).await, "v1");
        std::fs::write(
            crate::release::app_runtime_data_paths(temp.path(), app_id)
                .app
                .join("allow-ready"),
            "",
        )
        .unwrap();
    };
    let (deployed, ()) = tokio::join!(
        restarted.handle_command(deploy("v2")),
        request_during_deploy
    );
    assert!(deployed.is_ok(), "{deployed:?}");
    assert_eq!(restored.get_instances().len(), 1);
    assert_eq!(restored.get_instances()[0].build_version(), "v2");
    assert_eq!(
        served_build(&restarted.load_balancer.get_backend(app_id).unwrap()).await,
        "v2"
    );
    restarted.app_manager.shutdown_all().await;
}

#[test]
fn deploy_rejects_duplicate_routes() {
    let error =
        validate_deploy_routes(&["app.localhost".into(), "app.localhost".into()]).unwrap_err();
    assert!(error.contains("duplicate route 'app.localhost'"));
}

#[tokio::test]
async fn native_deploy_must_stop_previous_compose_even_without_http_instances() {
    let temp = TempDir::new().unwrap();
    let state = ServerState::new(
        temp.path().into(),
        Arc::new(CertManager::new(CertManagerConfig {
            cert_dir: temp.path().join("certs"),
            ..Default::default()
        })),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let app_id = "compose/production";
    let release = crate::release::app_release_root(temp.path(), app_id, "v2");
    std::fs::create_dir_all(&release).unwrap();
    write_release_manifest(&release, "go", "app", &[], None, 300);
    std::fs::write(release.join("app"), "unused").unwrap();
    let previous = state.app_manager.register_app(AppConfig {
        name: "compose".into(),
        environment: "production".into(),
        version: "v1".into(),
        path: temp.path().into(),
        env_vars: HashMap::from([(
            crate::release::TAKO_APP_DATA_DIR_ENV.into(),
            temp.path().join("data").display().to_string(),
        )]),
        compose: Some(tako_core::compose::ComposeConfig {
            file: "missing-compose.yaml".into(),
            service: "web".into(),
            port: 8080,
            health_path: "/".into(),
            health_host: None,
        }),
        ..Default::default()
    });
    let response = state
        .handle_command(Command::Deploy {
            app: app_id.into(),
            version: "v2".into(),
            path: release.display().to_string(),
            routes: vec!["compose.localhost".into()],
            source_ip: Default::default(),
            secrets: None,
            runtime_credentials: None,
            storages: None,
            ssl: Default::default(),
            backup: None,
            force: false,
        })
        .await;
    let Response::Error { message, .. } = response else {
        panic!("expected stack cleanup failure");
    };
    assert!(message.starts_with("Failed to stop the previous stack:"));
    assert!(message.contains("Compose file:"));
    assert_eq!(previous.version(), "v1");
    assert!(previous.config.read().compose.is_some());
}

#[tokio::test]
async fn deploy_rejects_invalid_app_name() {
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

    let response = state
        .handle_command(Command::Deploy {
            app: "../escape".to_string(),
            version: "v1".to_string(),
            path: temp.path().to_string_lossy().to_string(),
            routes: vec!["api.example.com".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            runtime_credentials: None,
            storages: Some(HashMap::new()),
            ssl: tako_core::SslBinding::default(),
            backup: None,
            force: false,
        })
        .await;

    let Response::Error { message, .. } = response else {
        panic!("expected invalid app name to be rejected");
    };
    // CodeQL[rust/cleartext-logging]: this isolated deploy fixture supplies no app secrets; assertions print test responses.
    assert!(message.contains("Invalid app name"), "got: {message}");
}

#[tokio::test]
async fn state_store_persists_ssl_credentials_per_app() {
    let temp_dir = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp_dir.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp_dir.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let ssl = tako_core::SslBinding {
        provider: tako_core::SslProvider::Cloudflare,
        cloudflare_api_token: Some("token-a".to_string()),
    };

    state
        .state_store
        .set_ssl("my-app/production", &ssl)
        .unwrap();

    assert_eq!(
        state.state_store.get_ssl("my-app/production").unwrap(),
        Some(ssl)
    );
    assert_eq!(
        state.state_store.get_ssl("other-app/production").unwrap(),
        None
    );
}

#[tokio::test]
async fn deploy_uses_prepared_ssl_credentials_when_start_payload_omits_token() {
    let temp_dir = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp_dir.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp_dir.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let app_id = "my-app/production";
    let release_dir = temp_dir
        .path()
        .join("apps")
        .join("my-app%2Fproduction")
        .join("releases")
        .join("v1");
    let routes = vec!["*.example.com".to_string()];
    let prepared_ssl = tako_core::SslBinding {
        provider: tako_core::SslProvider::LetsEncrypt,
        cloudflare_api_token: Some("server-egress-token".to_string()),
    };

    state
        .stage_prepared_deploy_ssl(app_id, &release_dir, routes.clone(), prepared_ssl.clone())
        .await;

    let resolved = state
        .resolve_deploy_ssl_binding(
            app_id,
            &release_dir,
            &routes,
            tako_core::SslBinding {
                provider: tako_core::SslProvider::LetsEncrypt,
                cloudflare_api_token: None,
            },
        )
        .await
        .expect("prepared SSL binding should resolve");

    assert_eq!(resolved, prepared_ssl);
}

#[tokio::test]
async fn failed_deploy_does_not_persist_ssl_credentials() {
    let temp_dir = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp_dir.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp_dir.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let app_id = "my-app/production";
    let release_dir = temp_dir
        .path()
        .join("apps")
        .join("my-app")
        .join("production")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    write_release_manifest(&release_dir, "go", "", &[], Some("true"), 300);

    let response = state
        .handle_command(Command::Deploy {
            app: app_id.to_string(),
            version: "v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["*.example.com".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            runtime_credentials: None,
            storages: Some(HashMap::new()),
            ssl: tako_core::SslBinding {
                provider: tako_core::SslProvider::Cloudflare,
                cloudflare_api_token: Some("ssl-token".to_string()),
            },
            backup: None,
            force: false,
        })
        .await;

    let Response::Error { message, .. } = response else {
        panic!("expected invalid release to be rejected");
    };
    // CodeQL[rust/cleartext-logging]: this isolated deploy fixture supplies no app secrets; assertions print test responses.
    assert!(message.contains("empty main field"), "got: {message}");
    assert_eq!(state.state_store.get_ssl(app_id).unwrap(), None);
}

#[tokio::test]
async fn deploy_rejects_release_path_outside_managed_root() {
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

    let outside_release = temp.path().join("outside-release");
    std::fs::create_dir_all(&outside_release).unwrap();

    let response = state
        .handle_command(Command::Deploy {
            app: "demo-app".to_string(),
            version: "v1".to_string(),
            path: outside_release.to_string_lossy().to_string(),
            routes: vec!["api.example.com".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            runtime_credentials: None,
            storages: Some(HashMap::new()),
            ssl: tako_core::SslBinding::default(),
            backup: None,
            force: false,
        })
        .await;

    let Response::Error { message, .. } = response else {
        panic!("expected out-of-root deploy path to be rejected");
    };
    // CodeQL[rust/cleartext-logging]: this isolated deploy fixture supplies no app secrets; assertions print test responses.
    assert!(
        message.contains("Invalid release path"),
        "expected path validation error, got: {message}"
    );
}

#[tokio::test]
async fn deploy_rejects_invalid_release_version() {
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
        .join("demo-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();

    let response = state
        .handle_command(Command::Deploy {
            app: "demo-app".to_string(),
            version: "../v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["api.example.com".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            runtime_credentials: None,
            storages: Some(HashMap::new()),
            ssl: tako_core::SslBinding::default(),
            backup: None,
            force: false,
        })
        .await;

    let Response::Error { message, .. } = response else {
        panic!("expected invalid release version to be rejected");
    };
    // CodeQL[rust/cleartext-logging]: this isolated deploy fixture supplies no app secrets; assertions print test responses.
    assert!(
        message.contains("Invalid release version"),
        "got: {message}"
    );
}

#[tokio::test]
async fn deploy_rejects_protocol_mismatch_before_registering_app() {
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
    let app_id = "my-app/production";
    let app_root = temp.path().join("apps/my-app/production");
    let release_dir = app_root.join("releases/v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    std::fs::write(
        release_dir.join("app.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "protocol_version": tako_core::PROTOCOL_VERSION + 1,
            "runtime": "bun",
            "main": "index.ts",
            "idle_timeout": 300,
        }))
        .unwrap(),
    )
    .unwrap();

    let response = state
        .handle_command(Command::Deploy {
            app: app_id.to_string(),
            version: "v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["app.localhost".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            runtime_credentials: None,
            storages: Some(HashMap::new()),
            ssl: tako_core::SslBinding::default(),
            backup: None,
            force: false,
        })
        .await;

    let Response::Error { message, .. } = response else {
        // CodeQL[rust/cleartext-logging]: this isolated deploy fixture supplies no app secrets; assertions print test responses.
        panic!("expected protocol mismatch, got: {response:?}");
    };
    // CodeQL[rust/cleartext-logging]: this isolated deploy fixture supplies no app secrets; assertions print test responses.
    assert!(
        message.contains("Protocol version mismatch"),
        "got: {message}"
    );
    assert!(state.app_manager.get_app(app_id).is_none());
    assert!(!app_root.join("data").exists());
}

#[test]
fn private_route_domains_prefer_self_signed_certs() {
    assert!(should_use_self_signed_route_cert(
        "tako-bun-server.orb.local"
    ));
    assert!(should_use_self_signed_route_cert("localhost"));
    assert!(should_use_self_signed_route_cert("api.localhost"));
    assert!(should_use_self_signed_route_cert("my-service"));
}

#[test]
fn public_route_domains_do_not_prefer_self_signed_certs() {
    assert!(!should_use_self_signed_route_cert("api.example.com"));
    assert!(!should_use_self_signed_route_cert("example.com"));
}

#[test]
fn bun_runtime_has_install_script() {
    let runtime = tako_runtime::runtime_def_for("bun", None).unwrap();
    let install = runtime.package_manager.install.as_deref().unwrap();
    assert!(install.contains("bun install --production"));
}

#[test]
fn node_runtime_uses_npm_install_script() {
    let runtime = tako_runtime::runtime_def_for("node", None).unwrap();
    let install = runtime.package_manager.install.as_deref().unwrap();
    assert!(install.contains("npm"));
}

#[tokio::test]
async fn ensure_route_certificate_generates_self_signed_for_private_domain() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    cert_manager.init().unwrap();
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager.clone(),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let cert = state
        .ensure_route_certificate("my-app", "tako-bun-server.orb.local")
        .await
        .expect("private domain should get a generated cert");
    assert!(cert.is_self_signed);
    assert_eq!(cert.domain, "tako-bun-server.orb.local");

    let cached = cert_manager
        .get_cert_for_host("tako-bun-server.orb.local")
        .expect("generated cert should be cached");
    assert!(cached.is_self_signed);
}

#[tokio::test]
async fn list_releases_reports_the_sdk_recorded_in_each_manifest() {
    let temp = TempDir::new().unwrap();
    let state = ServerState::new(
        temp.path().into(),
        Arc::new(CertManager::new(CertManagerConfig {
            cert_dir: temp.path().join("certs"),
            ..Default::default()
        })),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let app_id = "api/production";
    state.app_manager.register_app(AppConfig {
        name: "api".into(),
        environment: "production".into(),
        version: "with-sdk".into(),
        path: temp.path().into(),
        ..Default::default()
    });
    for (version, sdk) in [
        ("without-sdk", None),
        (
            "with-sdk",
            Some(serde_json::json!({
                "language": "javascript",
                "name": "tako.sh",
                "version": "1.4.2",
            })),
        ),
    ] {
        let release = crate::release::app_release_root(temp.path(), app_id, version);
        std::fs::create_dir_all(&release).unwrap();
        write_release_manifest(&release, "bun", "index.ts", &[], None, 300);
        if let Some(sdk) = sdk {
            let path = release.join("app.json");
            let mut manifest: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            manifest["sdk"] = sdk;
            std::fs::write(&path, manifest.to_string()).unwrap();
        }
    }

    let Response::Ok { data } = state
        .handle_command(Command::ListReleases { app: app_id.into() })
        .await
    else {
        panic!("expected a release list");
    };
    let listed: tako_core::ListReleasesResponse = serde_json::from_value(data).unwrap();
    let sdk_of = |version: &str| {
        listed
            .releases
            .iter()
            .find(|release| release.version == version)
            .unwrap_or_else(|| panic!("missing release {version}"))
            .sdk
            .clone()
    };
    assert_eq!(
        sdk_of("with-sdk"),
        Some(tako_core::ReleaseSdk {
            language: "javascript".into(),
            name: "tako.sh".into(),
            version: "1.4.2".into(),
        })
    );
    assert_eq!(sdk_of("without-sdk"), None);
}

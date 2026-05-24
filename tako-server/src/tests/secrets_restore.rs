use super::*;

#[tokio::test]
async fn get_secrets_hash_returns_hash_of_app_secrets() {
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

    // No secrets file → hash of empty map
    let response = state
        .handle_command(Command::GetSecretsHash {
            app: "my-app".to_string(),
        })
        .await;
    let Response::Ok { data } = &response else {
        panic!("expected ok response: {response:?}");
    };
    let empty_hash = data.get("hash").and_then(Value::as_str).unwrap();
    assert_eq!(empty_hash, tako_core::compute_secrets_hash(&HashMap::new()));

    // Store secrets and check hash changes
    let secrets: HashMap<String, String> = [("KEY".to_string(), "val".to_string())]
        .into_iter()
        .collect();
    state.state_store.set_secrets("my-app", &secrets).unwrap();

    let response = state
        .handle_command(Command::GetSecretsHash {
            app: "my-app".to_string(),
        })
        .await;
    let Response::Ok { data } = &response else {
        panic!("expected ok response");
    };
    let with_secrets_hash = data.get("hash").and_then(Value::as_str).unwrap();
    assert_ne!(with_secrets_hash, empty_hash);
    assert_eq!(with_secrets_hash, tako_core::compute_secrets_hash(&secrets));
}

#[tokio::test]
async fn deploy_without_secrets_keeps_existing() {
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

    // Pre-store secrets for the app
    let secrets: HashMap<String, String> = [("API_KEY".to_string(), "original".to_string())]
        .into_iter()
        .collect();
    state.state_store.set_secrets("keep-app", &secrets).unwrap();

    let release_dir = temp
        .path()
        .join("apps")
        .join("keep-app")
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

    // Deploy with secrets: None — should keep existing
    let _response = state
        .handle_command(Command::Deploy {
            app: "keep-app".to_string(),
            version: "v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["keep.localhost".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: None,
            storages: None,
            ssl: tako_core::SslBinding::default(),
        })
        .await;

    // Verify secrets still have original value
    let loaded = state.state_store.get_secrets("keep-app").unwrap();
    assert_eq!(loaded.get("API_KEY"), Some(&"original".to_string()));
}

#[tokio::test]
async fn failed_deploy_does_not_persist_credentials_for_unregistered_app() {
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
        .join("bad-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    std::fs::write(
        release_dir.join("app.json"),
        r#"{"runtime":"python","main":"server.py","idle_timeout":300}"#,
    )
    .unwrap();

    let secrets: HashMap<String, String> = [("API_KEY".to_string(), "new".to_string())]
        .into_iter()
        .collect();
    let storages: HashMap<String, tako_core::StorageBinding> = [(
        "uploads".to_string(),
        tako_core::StorageBinding {
            provider: tako_core::StorageProvider::Local,
            bucket: None,
            endpoint: None,
            region: None,
            access_key_id: None,
            secret_access_key: None,
            force_path_style: false,
            public_base_url: None,
            path: Some("uploads".to_string()),
            signing_key: None,
        },
    )]
    .into_iter()
    .collect();

    let response = state
        .handle_command(Command::Deploy {
            app: "bad-app".to_string(),
            version: "v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["bad.localhost".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(secrets),
            storages: Some(storages),
            ssl: tako_core::SslBinding::default(),
        })
        .await;

    assert!(
        matches!(response, Response::Error { .. }),
        "expected unsupported runtime deploy failure: {response:?}"
    );
    assert!(state.state_store.get_secrets("bad-app").unwrap().is_empty());
    assert!(
        state
            .state_store
            .get_storages("bad-app")
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn restore_from_state_store_rehydrates_apps_routes_and_secrets() {
    let temp = TempDir::new().unwrap();
    let app_id = "my-app/production";
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
        .join("production")
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

    let app_secrets: HashMap<String, String> =
        [("DATABASE_URL".to_string(), "postgres://db".to_string())]
            .into_iter()
            .collect();
    state_a
        .state_store
        .set_secrets(app_id, &app_secrets)
        .unwrap();

    let app = state_a.app_manager.register_app(AppConfig {
        name: "my-app".to_string(),
        environment: "production".to_string(),
        version: "v1".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 0,
        max_instances: 4,
        source_ip: tako_core::SourceIpMode::CloudflareProxy,
        idle_timeout: Duration::from_secs(300),
        ..Default::default()
    });
    state_a.load_balancer.register_app(app);
    {
        let mut route_table = state_a.routes.write().await;
        route_table.set_app_routes_with_source_ip(
            app_id.to_string(),
            vec![
                "api.example.com".to_string(),
                "example.com/api/*".to_string(),
            ],
            tako_core::SourceIpMode::CloudflareProxy,
        );
    }
    state_a.persist_app_state(app_id).await;
    drop(state_a);

    let state_b = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    state_b.restore_from_state_store().await.unwrap();

    let restored = state_b.app_manager.get_app(app_id).expect("app restored");
    assert_eq!(restored.version(), "v1");
    assert_eq!(
        restored.config.read().source_ip,
        tako_core::SourceIpMode::CloudflareProxy
    );
    assert_eq!(restored.state(), crate::socket::AppState::Idle);
    let route_table = state_b.routes.read().await;
    assert_eq!(
        route_table.routes_for_app(app_id),
        vec![
            "api.example.com".to_string(),
            "example.com/api/*".to_string()
        ]
    );
    assert_eq!(
        route_table
            .select_with_route("api.example.com", "/")
            .expect("route restored")
            .source_ip,
        tako_core::SourceIpMode::CloudflareProxy
    );
    let restored_secrets = restored.config.read().secrets.clone();
    assert_eq!(
        restored_secrets.get("DATABASE_URL"),
        Some(&"postgres://db".to_string())
    );
}

#[tokio::test]
async fn restore_from_state_store_restarts_internal_socket_for_apps_with_workflows() {
    let temp = TempDir::new().unwrap();
    let app_id = "workflow-app/production";
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
        .join("workflow-app")
        .join("production")
        .join("releases")
        .join("v1");
    write_js_workflow_scaffold(&release_dir);
    assert!(release_dir.join("src").join("workflows").is_dir());
    assert!(
        release_dir
            .join("node_modules")
            .join("tako.sh")
            .join("dist")
            .join("entrypoints")
            .join("bun-worker.mjs")
            .is_file()
    );
    write_release_manifest(
        &release_dir,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    let app = state_a.app_manager.register_app(AppConfig {
        name: "workflow-app".to_string(),
        environment: "production".to_string(),
        version: "v1".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 0,
        max_instances: 4,
        idle_timeout: Duration::from_secs(300),
        ..Default::default()
    });
    state_a.load_balancer.register_app(app);
    state_a.persist_app_state(app_id).await;
    drop(state_a);

    let state_b = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    state_b.restore_from_state_store().await.unwrap();

    assert!(
        state_b.app_manager.get_app(app_id).is_some(),
        "restored workflow app should be present in the app manager"
    );
    assert!(
        state_b.workflows.has(app_id),
        "restored workflow app should be re-registered with the workflow manager"
    );

    let socket = state_b.workflows.socket_path();
    let socket_ready = socket_ready(&socket);
    assert!(
        socket_ready,
        "restored workflow apps must restart the shared internal socket at {}",
        socket.display()
    );
}

#[tokio::test]
async fn server_state_starts_internal_socket_at_boot() {
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

    let socket = state.workflows.socket_path();
    assert!(
        socket_ready(&socket),
        "server boot must start the shared internal socket at {} so app-side channel .publish() works without workflows/",
        socket.display()
    );
}

#[test]
fn server_state_new_outside_tokio_runtime_does_not_panic() {
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
    .expect("server state should initialize without an entered Tokio runtime");

    assert_eq!(
        state.workflows.socket_path(),
        temp.path().join("internal.sock")
    );
}

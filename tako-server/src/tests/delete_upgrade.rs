use super::*;

#[tokio::test]
async fn delete_command_removes_runtime_registration_and_routes() {
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

    let app_root = temp.path().join("apps").join("my-app");
    let release_dir = app_root.join("releases").join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    std::fs::create_dir_all(app_root.join("data/app")).unwrap();
    std::fs::create_dir_all(app_root.join("data/tako")).unwrap();

    let config = AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "exit 0".to_string(),
        ],
        min_instances: 0,
        ..Default::default()
    };

    let app = state.app_manager.register_app(config);
    state.load_balancer.register_app(app);
    {
        let mut route_table = state.routes.write();
        route_table.set_app_routes("my-app".to_string(), vec!["api.example.com".to_string()]);
    }

    let response = state
        .handle_command(Command::Delete {
            app: "my-app".to_string(),
        })
        .await;
    assert!(matches!(response, Response::Ok { .. }));
    assert!(state.app_manager.get_app("my-app").is_none());
    assert!(!app_root.exists());

    let route_table = state.routes.read();
    assert!(route_table.routes_for_app("my-app").is_empty());
    assert_eq!(route_table.select("api.example.com", "/"), None);
}

#[tokio::test]
async fn delete_command_is_idempotent_for_missing_app() {
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
        .handle_command(Command::Delete {
            app: "missing-app".to_string(),
        })
        .await;
    assert!(matches!(response, Response::Ok { .. }));
    assert!(state.app_manager.get_app("missing-app").is_none());
}

#[tokio::test]
async fn delete_command_rejects_invalid_app_name() {
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
        .handle_command(Command::Delete {
            app: "../bad".to_string(),
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected invalid app name to be rejected");
    };
    assert!(message.contains("Invalid app name"), "got: {message}");
}

#[tokio::test]
async fn upgrading_mode_blocks_mutating_commands() {
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
    state.set_server_mode(UpgradeMode::Upgrading).await.unwrap();

    let response = state
        .handle_command(Command::Delete {
            app: "my-app".to_string(),
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected blocked mutating command while upgrading");
    };
    assert!(message.contains("Server is upgrading"));
    assert!(message.contains("delete"));
}

#[tokio::test]
async fn server_mode_resets_upgrading_on_boot() {
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
    state_a
        .set_server_mode(UpgradeMode::Upgrading)
        .await
        .unwrap();
    // Simulate an upgrade lock left behind by a crashed CLI.
    assert!(state_a.try_enter_upgrading("crashed-cli").await.unwrap());
    drop(state_a);

    // On restart, stale Upgrading mode AND orphaned lock should be cleared.
    let state_b = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager.clone(),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    assert_eq!(*state_b.server_mode.read().await, UpgradeMode::Normal);
    // A new owner should be able to acquire immediately (no 10-min stale wait).
    assert!(state_b.try_enter_upgrading("new-cli").await.unwrap());
}

#[tokio::test]
async fn upgrading_lock_allows_single_owner() {
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
    let state_b = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    assert!(state_a.try_enter_upgrading("controller-a").await.unwrap());
    assert!(!state_b.try_enter_upgrading("controller-b").await.unwrap());
    assert!(state_a.exit_upgrading("controller-a").await.unwrap());
    assert!(state_b.try_enter_upgrading("controller-b").await.unwrap());
}

#[tokio::test]
async fn server_info_command_reports_runtime_config() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let runtime = ServerRuntimeConfig {
        pid: std::process::id(),
        process_started_at_unix_secs: Some(1_778_220_000),
        socket: "/var/run/tako/tako-custom.sock".to_string(),
        data_dir: temp.path().to_path_buf(),
        http_port: 8080,
        https_port: 8443,
        no_acme: true,
        acme_staging: false,
        renewal_interval_hours: 24,
        standby: false,
        metrics_port: Some(9898),
        server_name: Some("test-server".to_string()),
        server_identity: Some("SHA256:testidentity".to_string()),
    };
    let state = ServerState::new_with_runtime(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
        runtime,
    )
    .unwrap();
    state
        .set_server_mode(UpgradeMode::Upgrading)
        .await
        .expect("mode set");

    let response = state.handle_command(Command::ServerInfo).await;
    let Response::Ok { data } = response else {
        panic!("expected server info response");
    };
    assert_eq!(
        data.get("pid").and_then(Value::as_u64),
        Some(std::process::id() as u64)
    );
    assert_eq!(data.get("mode").and_then(Value::as_str), Some("upgrading"));
    assert_eq!(
        data.get("socket").and_then(Value::as_str),
        Some("/var/run/tako/tako-custom.sock")
    );
    assert_eq!(data.get("http_port").and_then(Value::as_u64), Some(8080));
    assert_eq!(data.get("https_port").and_then(Value::as_u64), Some(8443));
    assert_eq!(data.get("no_acme").and_then(Value::as_bool), Some(true));
    assert_eq!(
        data.get("server_identity").and_then(Value::as_str),
        Some("SHA256:testidentity")
    );
}

#[tokio::test]
async fn enter_and_exit_upgrading_commands_use_owner_lock() {
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

    let enter = state
        .handle_command(Command::EnterUpgrading {
            owner: "controller-a".to_string(),
        })
        .await;
    assert!(matches!(enter, Response::Ok { .. }));

    let reject = state
        .handle_command(Command::EnterUpgrading {
            owner: "controller-b".to_string(),
        })
        .await;
    let Response::Error { message } = reject else {
        panic!("expected lock owner rejection");
    };
    assert!(message.contains("already upgrading"));
    assert!(message.contains("controller-a"));

    let wrong_exit = state
        .handle_command(Command::ExitUpgrading {
            owner: "controller-b".to_string(),
        })
        .await;
    assert!(matches!(wrong_exit, Response::Error { .. }));

    let exit = state
        .handle_command(Command::ExitUpgrading {
            owner: "controller-a".to_string(),
        })
        .await;
    assert!(matches!(exit, Response::Ok { .. }));
}

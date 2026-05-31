use super::*;

#[tokio::test]
async fn scale_command_rejects_instances_above_app_limit() {
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
        min_instances: 1,
        max_instances: 4,
        ..Default::default()
    });

    let response = state
        .handle_command(Command::Scale {
            app: "my-app".to_string(),
            instances: 100,
        })
        .await;

    let message = response.error_message().expect("scale should fail");
    assert!(message.contains("Requested 100 instances"));
    assert!(message.contains(&format!(
        "at most {}",
        crate::instances::effective_instance_limit(4)
    )));
    assert_eq!(app.config.read().min_instances, 1);
    assert_eq!(app.config.read().max_instances, 4);
    assert!(app.get_instances().is_empty());
}

#[tokio::test]
async fn standby_scale_still_caps_before_app_limit() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let runtime = ServerRuntimeConfig {
        standby: true,
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

    let app = state.app_manager.register_app(AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        min_instances: 1,
        max_instances: 4,
        ..Default::default()
    });
    let instance = app.allocate_instance();
    app.set_instance_state(&instance, InstanceState::Healthy);

    let response = state
        .handle_command(Command::Scale {
            app: "my-app".to_string(),
            instances: 100,
        })
        .await;

    let data = response.data().expect("standby scale should succeed");
    assert_eq!(data["instances"], 1);
    assert_eq!(data["requested_instances"], 100);
    assert_eq!(data["standby_limited"], true);
    assert_eq!(app.config.read().min_instances, 1);
    assert_eq!(app.config.read().max_instances, 4);
}

#[tokio::test]
async fn deploy_rejects_persisted_instances_above_app_limit() {
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
    write_release_manifest(
        &current_release,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    let app = state.app_manager.register_app(AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        path: current_release,
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 100,
        max_instances: 4,
        idle_timeout: Duration::from_secs(300),
        ..Default::default()
    });

    let next_release = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("releases")
        .join("v2");
    std::fs::create_dir_all(&next_release).unwrap();
    write_release_manifest(
        &next_release,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    let response = state
        .handle_command(Command::Deploy {
            app: "my-app".to_string(),
            version: "v2".to_string(),
            path: next_release.to_string_lossy().to_string(),
            routes: vec!["api.example.com".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            storages: Some(HashMap::new()),
            ssl: tako_core::SslBinding::default(),
            backup: None,
        })
        .await;

    let message = response.error_message().expect("deploy should fail");
    assert!(message.contains("Requested 100 instances"));
    assert!(message.contains(&format!(
        "at most {}",
        crate::instances::effective_instance_limit(4)
    )));
    assert_eq!(app.config.read().version, "v1");
}

#[tokio::test]
async fn restore_clamps_instances_above_app_limit() {
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
        &["/bin/sh", "-lc", "exit 1"],
        Some("true"),
        300,
    );
    let config = AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        path: release_dir,
        min_instances: 100,
        max_instances: 4,
        idle_timeout: Duration::from_secs(300),
        ..Default::default()
    };
    state_a
        .state_store
        .upsert_app(&config, &["api.example.com".to_string()])
        .unwrap();
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
    assert_eq!(
        restored.config.read().min_instances,
        crate::instances::effective_instance_limit(4)
    );
    assert_eq!(restored.config.read().max_instances, 4);
}

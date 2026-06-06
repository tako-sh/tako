use super::*;

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
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected invalid app name to be rejected");
    };
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
    write_release_manifest(
        &release_dir,
        "node",
        "",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

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
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected invalid release to be rejected");
    };
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
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected out-of-root deploy path to be rejected");
    };
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
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected invalid release version to be rejected");
    };
    assert!(
        message.contains("Invalid release version"),
        "got: {message}"
    );
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

use super::*;

#[test]
fn extract_zstd_archive_unpacks_files() {
    let temp = TempDir::new().unwrap();
    let archive_path = temp.path().join("payload.tar.zst");
    let dest = temp.path().join("dest");

    let file = std::fs::File::create(&archive_path).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut archive = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    let payload = b"hello";
    header.set_size(payload.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "app/index.txt", &mut Cursor::new(payload))
        .unwrap();
    let encoder = archive.into_inner().unwrap();
    encoder.finish().unwrap();

    extract_zstd_archive(&archive_path, &dest).unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.join("app/index.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn release_upload_lifecycle_extracts_and_finalizes_release() {
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
    let archive_path = temp.path().join("release.tar.zst");
    write_test_release_archive(&archive_path);

    let response = state
        .handle_command(Command::PrepareReleaseUpload {
            app: "my-app/production".to_string(),
            version: "v1".to_string(),
        })
        .await;
    let Response::Ok { data } = response else {
        panic!("expected upload plan");
    };
    let initial_plan: tako_core::ReleaseUploadPlan = serde_json::from_value(data).unwrap();
    assert!(initial_plan.upload_required);

    let stored_plan = state
        .store_uploaded_release_artifact("my-app/production", "v1", &archive_path)
        .unwrap();
    assert!(stored_plan.upload_required);
    assert!(Path::new(&stored_plan.path).join("app.json").is_file());
    #[cfg(unix)]
    assert!(
        std::fs::symlink_metadata(Path::new(&stored_plan.path).join("logs"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let response = state
        .handle_command(Command::PrepareReleaseUpload {
            app: "my-app/production".to_string(),
            version: "v1".to_string(),
        })
        .await;
    let Response::Ok { data } = response else {
        panic!("expected cached upload plan");
    };
    let cached_plan: tako_core::ReleaseUploadPlan = serde_json::from_value(data).unwrap();
    assert!(!cached_plan.upload_required);

    let response = state
        .handle_command(Command::FinalizeRelease {
            app: "my-app/production".to_string(),
            version: "v1".to_string(),
        })
        .await;
    assert!(matches!(response, Response::Ok { .. }));
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_link(temp.path().join("apps/my-app/production/current")).unwrap(),
        Path::new(&stored_plan.path)
    );
}

#[test]
fn extract_zstd_archive_rejects_path_traversal() {
    let temp = TempDir::new().unwrap();
    let archive_path = temp.path().join("malicious.tar.zst");
    let dest = temp.path().join("dest");

    // Build a tar with a `../escape.txt` entry by writing raw header bytes,
    // bypassing the builder's own path validation.
    let file = std::fs::File::create(&archive_path).unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    {
        use std::io::Write;
        let mut header = tar::Header::new_gnu();
        let payload = b"pwned";
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        // Write path directly into the header name field
        let path = b"../escape.txt";
        let bytes = header.as_mut_bytes();
        bytes[..path.len()].copy_from_slice(path);
        header.set_cksum();

        encoder.write_all(header.as_bytes()).unwrap();
        encoder.write_all(payload).unwrap();
        // Pad to 512-byte boundary
        let padding = 512 - (payload.len() % 512);
        if padding < 512 {
            encoder.write_all(&vec![0u8; padding]).unwrap();
        }
        // Two zero blocks to end archive
        encoder.write_all(&[0u8; 1024]).unwrap();
    }
    encoder.finish().unwrap();

    // tar crate silently skips entries with `..` (returns Ok)
    extract_zstd_archive(&archive_path, &dest).unwrap();
    assert!(
        !temp.path().join("escape.txt").exists(),
        "path traversal: file escaped dest"
    );
    assert!(
        !dest.join("escape.txt").exists(),
        "path traversal: file should be skipped entirely"
    );
}

#[test]
fn run_extract_archive_mode_requires_destination_flag() {
    let args = super::Args::try_parse_from([
        "tako-server",
        "--extract-zstd-archive",
        "/tmp/payload.tar.zst",
    ])
    .unwrap();
    let err = run_extract_archive_mode(&args).unwrap_err();
    assert!(err.contains("--extract-dest"));
}

#[test]
fn install_rustls_crypto_provider_is_idempotent() {
    install_rustls_crypto_provider();
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());

    install_rustls_crypto_provider();
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());
}

#[test]
fn should_signal_parent_on_ready_defaults_to_false() {
    let _guard = signal_parent_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::remove_var(SIGNAL_PARENT_ON_READY_ENV);
    }
    assert!(!should_signal_parent_on_ready());
}

#[test]
fn should_signal_parent_on_ready_reads_env_toggle() {
    let _guard = signal_parent_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    unsafe {
        std::env::set_var(SIGNAL_PARENT_ON_READY_ENV, "1");
    }
    assert!(should_signal_parent_on_ready());

    unsafe {
        std::env::set_var(SIGNAL_PARENT_ON_READY_ENV, "0");
    }
    assert!(!should_signal_parent_on_ready());

    unsafe {
        std::env::remove_var(SIGNAL_PARENT_ON_READY_ENV);
    }
}

#[test]
fn validate_deploy_routes_rejects_empty_routes() {
    let err = validate_deploy_routes(&[]).unwrap_err();
    assert!(err.contains("at least one route"));
}

#[test]
fn validate_deploy_routes_rejects_empty_route_entry() {
    let err = validate_deploy_routes(&["".to_string()]).unwrap_err();
    assert!(err.contains("non-empty"));
}

#[test]
fn validate_app_name_accepts_app_env_identifier() {
    assert!(validate_app_name("my-app/staging").is_ok());
}

#[test]
fn read_server_config_from_json() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"server_name":"prod","trusted_proxy":{"proxy_protocol":true,"trusted_cidrs":["127.0.0.1/32"],"client_ip_headers":["cf-connecting-ip"]}}"#,
    )
    .unwrap();
    let config = read_server_config(dir.path());
    assert_eq!(config.server_name.as_deref(), Some("prod"));
    let trusted_proxy = config.trusted_proxy.unwrap();
    assert!(trusted_proxy.proxy_protocol);
    assert_eq!(trusted_proxy.trusted_cidrs, vec!["127.0.0.1/32"]);
    assert_eq!(trusted_proxy.client_ip_headers, vec!["cf-connecting-ip"]);
}

#[test]
fn read_server_config_returns_defaults_when_missing() {
    let dir = TempDir::new().unwrap();
    let config = read_server_config(dir.path());
    assert!(config.server_name.is_none());
}

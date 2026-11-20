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
    assert!(initial_plan.preparation_required);

    let stored_plan = state
        .store_uploaded_release_artifact("my-app/production", "v1", &archive_path)
        .unwrap();
    assert!(stored_plan.upload_required);
    assert!(stored_plan.preparation_required);
    assert!(Path::new(&stored_plan.path).join("app.json").is_file());
    let retained = temp
        .path()
        .join("apps/my-app/production/archives/v1/release.tar.zst");
    assert_eq!(
        std::fs::read(&retained).unwrap(),
        std::fs::read(&archive_path).unwrap()
    );
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
    assert!(cached_plan.preparation_required);

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

    let Response::Ok { data } = state
        .prepare_release_upload("my-app/production", "v1")
        .await
    else {
        panic!("expected prepared release reuse");
    };
    let prepared: tako_core::ReleaseUploadPlan = serde_json::from_value(data).unwrap();
    assert!(!prepared.upload_required && !prepared.preparation_required);
    assert!(
        state
            .finalize_release("my-app/production", "v1")
            .await
            .is_ok()
    );

    // Server-side installs must disappear with the retired working directory,
    // and must never be folded into the retained upload.
    std::fs::create_dir_all(Path::new(&stored_plan.path).join("node_modules/large")).unwrap();
    std::fs::write(
        Path::new(&stored_plan.path).join("node_modules/large/index.js"),
        "installed",
    )
    .unwrap();
    let second = state
        .store_uploaded_release_artifact("my-app/production", "v2", &archive_path)
        .unwrap();
    assert!(
        state
            .finalize_release("my-app/production", "v2")
            .await
            .is_ok()
    );
    assert!(!Path::new(&stored_plan.path).exists());
    assert!(Path::new(&second.path).is_dir());
    assert_eq!(
        std::fs::read(&retained).unwrap(),
        std::fs::read(&archive_path).unwrap()
    );

    state.app_manager.register_app(AppConfig {
        name: "my-app".into(),
        environment: "production".into(),
        version: "v2".into(),
        ..Default::default()
    });
    let Response::Ok { data } = state.list_releases("my-app/production").await else {
        panic!("expected release history");
    };
    let history: tako_core::ListReleasesResponse = serde_json::from_value(data).unwrap();
    assert_eq!(history.releases.len(), 2);
    assert!(
        history
            .releases
            .iter()
            .any(|r| r.version == "v1" && !r.current)
    );
    assert!(
        history
            .releases
            .iter()
            .any(|r| r.version == "v2" && r.current)
    );

    let Response::Ok { data } = state
        .prepare_release_upload("my-app/production", "v1")
        .await
    else {
        panic!("expected archive reuse");
    };
    let restored: tako_core::ReleaseUploadPlan = serde_json::from_value(data).unwrap();
    assert!(!restored.upload_required);
    assert!(restored.preparation_required);
    assert!(Path::new(&restored.path).join("app.json").is_file());
    assert!(!Path::new(&restored.path).join("node_modules").exists());
    // A candidate staged between RPCs must survive periodic cleanup.
    state
        .compact_app_releases("my-app/production")
        .await
        .unwrap();
    assert!(Path::new(&restored.path).is_dir());
    assert_eq!(
        std::fs::read(&retained).unwrap(),
        std::fs::read(&archive_path).unwrap()
    );
}

#[tokio::test]
async fn directory_only_releases_are_not_history_or_rollback_targets() {
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
    let app = "my-app/production";
    let release = crate::release::app_release_root(temp.path(), app, "old");
    std::fs::create_dir_all(&release).unwrap();
    write_release_manifest(&release, "node", "index.js", &[], None, 300);
    state.app_manager.register_app(AppConfig {
        name: "my-app".into(),
        environment: "production".into(),
        version: "current".into(),
        ..Default::default()
    });
    let Response::Ok { data } = state.list_releases(app).await else {
        panic!("expected release list");
    };
    let history: tako_core::ListReleasesResponse = serde_json::from_value(data).unwrap();
    assert!(history.releases.is_empty());
    let Response::Ok { data } = state.prepare_release_upload(app, "old").await else {
        panic!("expected upload plan");
    };
    let plan: tako_core::ReleaseUploadPlan = serde_json::from_value(data).unwrap();
    assert!(plan.upload_required && plan.preparation_required);
    let response = state.rollback_app(app, "old").await;
    assert!(
        response
            .error_message()
            .unwrap()
            .contains("Release archive")
    );
    assert!(release.is_dir());
    let upload = temp.path().join("upload.tar.zst");
    write_test_release_archive(&upload);
    assert!(
        state
            .store_uploaded_release_artifact(app, "old", &upload)
            .is_err()
    );
    state.compact_app_releases(app).await.unwrap();
    assert!(!release.exists());
}

#[tokio::test]
async fn failed_rollback_preserves_preexisting_dependencies_and_preparation_state() {
    let temp = TempDir::new().unwrap();
    let state = ServerState::new(
        temp.path().to_path_buf(),
        Arc::new(CertManager::new(CertManagerConfig {
            cert_dir: temp.path().join("certs"),
            ..Default::default()
        })),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let app = "my-app/production";
    let upload = temp.path().join("upload.tar.zst");
    write_test_release_archive(&upload);
    let old = state
        .store_uploaded_release_artifact(app, "v1", &upload)
        .unwrap();
    assert!(state.finalize_release(app, "v1").await.is_ok());
    let dependency = Path::new(&old.path).join("node_modules/prepared");
    std::fs::create_dir_all(dependency.parent().unwrap()).unwrap();
    std::fs::write(&dependency, "installed before rollback").unwrap();
    state.app_manager.register_app(AppConfig {
        name: "my-app".into(),
        environment: "production".into(),
        version: "v2".into(),
        ..Default::default()
    });
    let pending = temp
        .path()
        .join("apps/my-app/production/archives/v1/pending");
    for was_pending in [false, true] {
        if was_pending {
            std::fs::write(&pending, []).unwrap();
        }
        // Route validation fails after restoration, before preparation or activation.
        let Response::Error { message, .. } = state.rollback_app(app, "v1").await else {
            panic!("rollback without routes must fail");
        };
        assert!(message.contains("no routes"), "{message}");
        assert_eq!(
            std::fs::read_to_string(&dependency).unwrap(),
            "installed before rollback"
        );
        assert_eq!(pending.exists(), was_pending);
    }
}

#[tokio::test]
async fn release_finalization_succeeds_when_compaction_fails_and_cleanup_retries() {
    let temp = TempDir::new().unwrap();
    let state = ServerState::new(
        temp.path().to_path_buf(),
        Arc::new(CertManager::new(CertManagerConfig {
            cert_dir: temp.path().join("certs"),
            ..Default::default()
        })),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let app = "my-app/production";
    let root = temp.path().join("apps/my-app/production");
    let upload = temp.path().join("upload.tar.zst");
    write_test_release_archive(&upload);
    let old = state
        .store_uploaded_release_artifact(app, "v1", &upload)
        .unwrap();
    assert!(state.finalize_release(app, "v1").await.is_ok());
    let current = state
        .store_uploaded_release_artifact(app, "v2", &upload)
        .unwrap();
    // A malformed private Compose directory prevents removal, even as root.
    std::fs::write(root.join("compose"), "not a directory").unwrap();
    assert!(state.finalize_release(app, "v2").await.is_ok());
    assert_eq!(
        std::fs::read_link(root.join("current")).unwrap(),
        Path::new(&current.path)
    );
    assert!(!root.join("archives/v2/pending").exists());
    assert!(Path::new(&old.path).is_dir());
    state.app_manager.register_app(AppConfig {
        name: "my-app".into(),
        environment: "production".into(),
        version: "v2".into(),
        ..Default::default()
    });
    std::fs::remove_file(root.join("compose")).unwrap();
    {
        let _operator = state.management_lock.lock().await;
        tokio::time::timeout(Duration::from_millis(100), state.compact_app_releases(app))
            .await
            .unwrap()
            .unwrap();
        assert!(
            Path::new(&old.path).is_dir(),
            "housekeeping must yield to operators"
        );
    }
    state.compact_app_releases(app).await.unwrap();
    assert!(!Path::new(&old.path).exists());
    assert!(root.join("archives/v1/release.tar.zst").is_file());
}

#[test]
fn archive_restoration_queues_on_the_blocking_pool_for_upload_and_rollback() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = TempDir::new().unwrap();
        let state = ServerState::new(
            temp.path().to_path_buf(),
            Arc::new(CertManager::new(CertManagerConfig {
                cert_dir: temp.path().join("certs"),
                ..Default::default()
            })),
            None,
            empty_challenge_tokens(),
        )
        .unwrap();
        let app = "my-app/production";
        let upload = temp.path().join("upload.tar.zst");
        write_test_release_archive(&upload);
        let old = state
            .store_uploaded_release_artifact(app, "v1", &upload)
            .unwrap();
        state.app_manager.register_app(AppConfig {
            name: "my-app".into(),
            environment: "production".into(),
            version: "v2".into(),
            ..Default::default()
        });
        for rollback in [false, true] {
            std::fs::remove_dir_all(&old.path).unwrap();
            let (release_worker, wait) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || wait.recv().unwrap());
            let restore = async {
                if rollback {
                    state.rollback_app(app, "v1").await
                } else {
                    state.prepare_release_upload(app, "v1").await
                }
            };
            tokio::pin!(restore);
            let queued = tokio::time::timeout(Duration::from_millis(20), &mut restore)
                .await
                .is_err();
            let still_absent = !Path::new(&old.path).exists();
            release_worker.send(()).unwrap();
            blocker.await.unwrap();
            assert!(
                queued && still_absent,
                "extraction must wait for the blocking pool while Tokio remains responsive"
            );
            let response = restore.await;
            assert_eq!(response.is_ok(), !rollback);
        }
    });
}

#[tokio::test]
async fn release_compaction_bounds_slow_docker_inspection_for_waiting_operators() {
    const CHILD: &str = "TAKO_TEST_SLOW_COMPACTION_DOCKER";
    let temp = TempDir::new().unwrap();
    if std::env::var_os(CHILD).is_none() {
        use std::os::unix::fs::PermissionsExt;
        let docker = temp.path().join("docker");
        std::fs::write(&docker, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Isolate PATH from the other parallel tests.
        let output = StdCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::archive_boot::release_compaction_bounds_slow_docker_inspection_for_waiting_operators", "--nocapture"])
            .env(CHILD, "1")
            .env("PATH", std::env::join_paths(
                std::iter::once(temp.path().to_path_buf())
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()))
            ).unwrap())
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let state = ServerState::new(
        temp.path().to_path_buf(),
        Arc::new(CertManager::new(CertManagerConfig {
            cert_dir: temp.path().join("certs"),
            ..Default::default()
        })),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let app = "my-app/production";
    let release = crate::release::app_release_root(temp.path(), app, "v1");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(release.join("compose.yaml"), "services: {}\n").unwrap();
    state.app_manager.register_app(AppConfig {
        name: "my-app".into(),
        environment: "production".into(),
        version: "v1".into(),
        path: release,
        compose: Some(tako_core::compose::ComposeConfig {
            file: "compose.yaml".into(),
            services: Default::default(),
        }),
        env_vars: HashMap::from([(
            crate::release::TAKO_APP_DATA_DIR_ENV.into(),
            temp.path()
                .join("apps/my-app/production/data/app")
                .to_string_lossy()
                .into_owned(),
        )]),
        ..Default::default()
    });
    let waiting_operator = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _guard = tokio::time::timeout(Duration::from_secs(2), state.management_lock.lock())
            .await
            .expect("housekeeping must release the mutation lock promptly");
    };
    let (cleanup, ()) = tokio::join!(state.compact_app_releases(app), waiting_operator);
    assert!(cleanup.unwrap_err().contains("inspection timed out"));
}

#[tokio::test]
async fn corrupt_archived_rollback_preserves_current_release_and_original_archive() {
    let temp = TempDir::new().unwrap();
    let state = ServerState::new(
        temp.path().to_path_buf(),
        Arc::new(CertManager::new(CertManagerConfig {
            cert_dir: temp.path().join("certs"),
            ..Default::default()
        })),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let source = temp.path().join("upload.tar.zst");
    write_test_release_archive(&source);
    let old = state
        .store_uploaded_release_artifact("my-app/production", "v1", &source)
        .unwrap();
    assert!(
        state
            .finalize_release("my-app/production", "v1")
            .await
            .is_ok()
    );
    let current = state
        .store_uploaded_release_artifact("my-app/production", "v2", &source)
        .unwrap();
    assert!(
        state
            .finalize_release("my-app/production", "v2")
            .await
            .is_ok()
    );
    state.app_manager.register_app(AppConfig {
        name: "my-app".into(),
        environment: "production".into(),
        version: "v2".into(),
        ..Default::default()
    });
    let archive = temp
        .path()
        .join("apps/my-app/production/archives/v1/release.tar.zst");
    std::fs::write(&archive, b"broken archive").unwrap();
    let response = state.rollback_app("my-app/production", "v1").await;
    assert!(!response.is_ok(), "{response:?}");
    assert_eq!(
        state
            .app_manager
            .get_app("my-app/production")
            .unwrap()
            .version(),
        "v2"
    );
    assert!(Path::new(&current.path).is_dir());
    assert!(!Path::new(&old.path).exists());
    assert_eq!(std::fs::read(&archive).unwrap(), b"broken archive");
    assert!(!archive.parent().unwrap().join("pending").exists());
    assert_eq!(
        std::fs::read_dir(temp.path().join("apps/my-app/production/releases"))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn archived_rollback_install_failure_keeps_current_release_and_history() {
    use std::os::unix::fs::PermissionsExt;
    let temp = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = ServerState::new(
        temp.path().to_path_buf(),
        Arc::new(CertManager::new(CertManagerConfig {
            cert_dir: temp.path().join("certs"),
            ..Default::default()
        })),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let app_id = "my-app/production";
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("app.json"), r#"{"protocol_version":0,"runtime":"bun","runtime_version":"1.0.0","main":"index.ts","idle_timeout":300}"#).unwrap();
    let upload = temp.path().join("upload.tar.zst");
    let encoder =
        zstd::stream::write::Encoder::new(std::fs::File::create(&upload).unwrap(), 3).unwrap();
    let mut archive = tar::Builder::new(encoder);
    archive.append_dir_all(".", &source).unwrap();
    archive.into_inner().unwrap().finish().unwrap();
    let old = state
        .store_uploaded_release_artifact(app_id, "v1", &upload)
        .unwrap();
    assert!(state.finalize_release(app_id, "v1").await.is_ok());
    let current = state
        .store_uploaded_release_artifact(app_id, "v2", &upload)
        .unwrap();
    assert!(state.finalize_release(app_id, "v2").await.is_ok());
    state.app_manager.register_app(AppConfig {
        name: "my-app".into(),
        environment: "production".into(),
        version: "v2".into(),
        ..Default::default()
    });
    state
        .routes
        .write()
        .set_app_routes(app_id.into(), vec!["my-app.localhost".into()]);
    let runtime = temp.path().join("runtimes/bun/1.0.0/bun");
    std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
    std::fs::write(
        &runtime,
        "#!/bin/sh\necho 'dependency unavailable' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();

    let Response::Error { message, .. } = state.rollback_app(app_id, "v1").await else {
        panic!("rollback must fail when production install fails");
    };
    assert!(message.contains("dependency unavailable"), "{message}");
    assert_eq!(state.app_manager.get_app(app_id).unwrap().version(), "v2");
    assert!(Path::new(&current.path).is_dir());
    assert!(!Path::new(&old.path).exists());
    let retained = temp
        .path()
        .join("apps/my-app/production/archives/v1/release.tar.zst");
    assert_eq!(
        std::fs::read(retained).unwrap(),
        std::fs::read(upload).unwrap()
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
fn run_protocol_compatibility_mode_requires_data_dir() {
    let args =
        super::Args::try_parse_from(["tako-server", "--check-protocol-compatibility"]).unwrap();

    let error = run_protocol_compatibility_mode(&args).unwrap_err();

    assert!(error.contains("--data-dir"), "got: {error}");
}

#[test]
fn run_protocol_compatibility_mode_requires_expected_protocol() {
    let args = super::Args::try_parse_from([
        "tako-server",
        "--check-protocol-compatibility",
        "--data-dir",
        "/opt/tako",
    ])
    .unwrap();

    let error = run_protocol_compatibility_mode(&args).unwrap_err();

    assert!(
        error.contains("--expected-protocol-version"),
        "got: {error}"
    );
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
fn validate_deploy_routes_rejects_path_escaping_hosts() {
    let err = validate_deploy_routes(&["..".to_string()]).unwrap_err();
    assert!(err.contains("invalid route host"));
    let err = validate_deploy_routes(&["../evil.example.com".to_string()]).unwrap_err();
    assert!(err.contains("invalid route host"));
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

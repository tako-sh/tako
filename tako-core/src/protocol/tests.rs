use super::*;

#[test]
fn test_command_serialization() {
    let cmd = Command::Status {
        app: "my-app".to_string(),
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains("status"));
    assert!(json.contains("my-app"));
}

#[test]
fn management_auth_message_includes_context_and_body() {
    // CodeQL[rust/hard-coded-cryptographic-value]: fixed nonce keeps this protocol fixture deterministic.
    let message = management_auth_message("1778220000", "abc123", br#"{"command":"list"}"#);

    assert_eq!(
        message,
        b"tako-management-rpc-v0\n1778220000\nabc123\n{\"command\":\"list\"}"
    );
}

#[test]
fn release_artifact_upload_auth_body_is_stable() {
    let body = release_artifact_upload_auth_body("my-app/production", "v1", 42, "abcdef");

    assert_eq!(
        body,
        b"release_artifact_upload\nmy-app/production\nv1\n42\nabcdef"
    );
}

#[test]
fn logs_request_auth_body_is_stable() {
    let body = logs_request_auth_body("my-app/production", 12, 34, Some(1_778_220_000), 8192);

    assert_eq!(body, b"logs\nmy-app/production\n12\n34\n1778220000\n8192");
}

#[test]
fn test_prepare_release_command_roundtrip() {
    let cmd = Command::PrepareRelease {
        app: "my-app".to_string(),
        path: "/opt/tako/apps/my-app/releases/v1".to_string(),
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""command":"prepare_release""#));
    let parsed: Command = serde_json::from_str(&json).unwrap();
    match parsed {
        Command::PrepareRelease { app, path } => {
            assert_eq!(app, "my-app");
            assert_eq!(path, "/opt/tako/apps/my-app/releases/v1");
        }
        _ => panic!("Expected PrepareRelease command"),
    }
}

#[test]
fn release_upload_commands_roundtrip() {
    let commands = [
        Command::PrepareReleaseUpload {
            app: "my-app/production".to_string(),
            version: "v1".to_string(),
        },
        Command::PrepareDeploy {
            app: "my-app/production".to_string(),
            version: "v1".to_string(),
            path: "/opt/tako/apps/my-app/releases/v1".to_string(),
            routes: vec!["*.example.com".to_string()],
            ssl: SslBinding {
                provider: SslProvider::LetsEncrypt,
                cloudflare_api_token: Some("token".to_string()),
            },
        },
        Command::CleanupRelease {
            app: "my-app/production".to_string(),
            version: "v1".to_string(),
        },
        Command::CleanupPreparedDeploy {
            app: "my-app/production".to_string(),
            version: "v1".to_string(),
        },
        Command::FinalizeRelease {
            app: "my-app/production".to_string(),
            version: "v1".to_string(),
        },
        Command::CheckDeploySpace {
            min_free_bytes: 256 * 1024 * 1024,
        },
    ];

    for command in commands {
        let json = serde_json::to_string(&command).unwrap();
        let parsed: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_value(parsed).unwrap(),
            serde_json::to_value(command).unwrap()
        );
    }
}

#[test]
fn test_deploy_command_serialization_includes_scaling() {
    let cmd = Command::Deploy {
        app: "my-app".to_string(),
        version: "v1".to_string(),
        path: "/opt/tako/apps/my-app/releases/v1".to_string(),
        routes: vec!["example.com".to_string()],
        source_ip: SourceIpMode::Auto,
        secrets: Some(HashMap::from([(
            "API_KEY".to_string(),
            "secret123".to_string(),
        )])),
        runtime_credentials: None,
        storages: None,
        ssl: SslBinding::default(),
        backup: None,
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""command":"deploy""#));
    assert!(json.contains(r#""secrets":{"API_KEY":"secret123"}"#));
}

#[test]
fn test_deploy_command_serialization_includes_ssl_binding() {
    let cmd = Command::Deploy {
        app: "my-app".to_string(),
        version: "v1".to_string(),
        path: "/opt/tako/apps/my-app/releases/v1".to_string(),
        routes: vec!["example.com".to_string()],
        source_ip: SourceIpMode::Auto,
        secrets: None,
        runtime_credentials: None,
        storages: None,
        ssl: SslBinding {
            provider: SslProvider::Cloudflare,
            cloudflare_api_token: Some("token".to_string()),
        },
        backup: None,
    };

    let json = serde_json::to_string(&cmd).unwrap();

    assert!(json.contains(r#""ssl":{"provider":"cloudflare""#));
    assert!(json.contains(r#""cloudflare_api_token":"token""#));
}

#[test]
fn backup_binding_serializes_backup_keys() {
    let binding = BackupBinding {
        storage: StorageBinding {
            provider: crate::StorageProvider::S3,
            bucket: Some("bucket".to_string()),
            endpoint: Some("https://s3.example.com".to_string()),
            region: Some("auto".to_string()),
            access_key_id: Some("access".to_string()),
            secret_access_key: Some("secret".to_string()),
            force_path_style: false,
            public_base_url: None,
            path: None,
            signing_key: None,
        },
        backup_keys: vec![BackupKeyBinding {
            id: "backup-key-0123456789abcdef".to_string(),
            key_base64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
        }],
        retention_days: DEFAULT_BACKUP_RETENTION_DAYS,
    };

    let json = serde_json::to_string(&binding).unwrap();
    assert!(json.contains(r#""backup_keys":[{"id":"backup-key-0123456789abcdef""#));
    let parsed: BackupBinding = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.backup_keys.len(), 1);
    assert_eq!(parsed.backup_keys[0].id, "backup-key-0123456789abcdef");
}

#[test]
fn backup_info_serializes_encryption_metadata() {
    let info = BackupInfo {
        id: "b1".to_string(),
        app: "demo".to_string(),
        environment: "production".to_string(),
        server: "la".to_string(),
        created_at_unix_secs: 1_778_220_000,
        size_bytes: 123,
        sha256_hex: "abc".to_string(),
        archive_key: "_tako/backups/demo/production/la/b1.tar.zst.enc".to_string(),
        manifest_key: "_tako/backups/demo/production/la/b1.json".to_string(),
        encryption: BackupEncryptionInfo {
            algorithm: "aes-256-gcm".to_string(),
            key_id: "backup-key-0123456789abcdef".to_string(),
            nonce_base64: "nonce".to_string(),
            tag_base64: "tag".to_string(),
        },
    };

    let json = serde_json::to_string(&info).unwrap();
    assert!(json.contains(r#""algorithm":"aes-256-gcm""#));
    assert!(json.contains(r#""key_id":"backup-key-0123456789abcdef""#));
    let parsed: BackupInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(
        parsed.encryption.key_id.as_str(),
        "backup-key-0123456789abcdef"
    );
}

#[test]
fn backup_info_requires_encryption_metadata() {
    let json = r#"{
        "id":"b1",
        "app":"demo",
        "environment":"production",
        "server":"la",
        "created_at_unix_secs":1778220000,
        "size_bytes":123,
        "sha256_hex":"abc",
        "archive_key":"_tako/backups/demo/production/la/b1.tar.zst.enc",
        "manifest_key":"_tako/backups/demo/production/la/b1.json"
    }"#;

    assert!(serde_json::from_str::<BackupInfo>(json).is_err());
}

#[test]
fn test_deploy_command_serialization_includes_source_ip_mode() {
    let cmd = Command::Deploy {
        app: "my-app".to_string(),
        version: "v1".to_string(),
        path: "/opt/tako/apps/my-app/releases/v1".to_string(),
        routes: vec!["example.com".to_string()],
        source_ip: SourceIpMode::TrustedProxy,
        secrets: None,
        runtime_credentials: None,
        storages: None,
        ssl: SslBinding::default(),
        backup: None,
    };

    let json = serde_json::to_string(&cmd).unwrap();

    assert!(json.contains(r#""source_ip":"trusted-proxy""#));
}

#[test]
fn test_deploy_command_deserialization_defaults_secrets_when_missing() {
    let json = r#"{
            "command":"deploy",
            "app":"my-app",
            "version":"v1",
            "path":"/opt/tako/apps/my-app/releases/v1",
            "routes":["example.com"]
        }"#;
    let cmd: Command = serde_json::from_str(json).unwrap();
    match cmd {
        Command::Deploy {
            source_ip,
            secrets,
            storages,
            ssl,
            backup,
            ..
        } => {
            assert_eq!(source_ip, SourceIpMode::Auto);
            assert!(secrets.is_none());
            assert!(storages.is_none());
            assert_eq!(ssl.provider, SslProvider::LetsEncrypt);
            assert!(backup.is_none());
        }
        _ => panic!("Expected deploy command"),
    }
}

#[test]
fn test_deployment_app_id_round_trip() {
    let app_id = deployment_app_id("my-app", "staging");
    assert_eq!(app_id, "my-app/staging");
    assert_eq!(
        split_deployment_app_id(&app_id),
        Some(("my-app", "staging"))
    );
}

#[test]
fn test_split_deployment_app_id_rejects_invalid_values() {
    assert_eq!(split_deployment_app_id("my-app"), None);
    assert_eq!(split_deployment_app_id("/staging"), None);
    assert_eq!(split_deployment_app_id("my-app/"), None);
    assert_eq!(split_deployment_app_id("my-app/staging/blue"), None);
}

#[test]
fn test_deployment_app_id_filename_encodes_separator() {
    assert_eq!(
        deployment_app_id_filename("my-app/staging"),
        "my-app%2Fstaging"
    );
}

#[test]
fn test_scale_command_serialization() {
    let cmd = Command::Scale {
        app: "my-app".to_string(),
        instances: 3,
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""command":"scale""#));
    assert!(json.contains(r#""app":"my-app""#));
    assert!(json.contains(r#""instances":3"#));
}

#[test]
fn test_scale_command_deserialization() {
    let json = r#"{"command":"scale","app":"my-app","instances":2}"#;
    let cmd: Command = serde_json::from_str(json).unwrap();
    match cmd {
        Command::Scale { app, instances } => {
            assert_eq!(app, "my-app");
            assert_eq!(instances, 2);
        }
        _ => panic!("Expected scale command"),
    }
}

#[test]
fn test_hello_roundtrip() {
    let cmd = Command::Hello {
        protocol_version: PROTOCOL_VERSION,
    };
    let json = serde_json::to_string(&cmd).unwrap();
    let parsed: Command = serde_json::from_str(&json).unwrap();
    match parsed {
        Command::Hello { protocol_version } => assert_eq!(protocol_version, PROTOCOL_VERSION),
        _ => panic!("expected hello"),
    }
}

#[test]
fn test_routes_command_serialization() {
    let cmd = Command::Routes;
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""command":"routes""#));
}

#[test]
fn test_server_info_command_serialization() {
    let cmd = Command::ServerInfo;
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""command":"server_info""#));
}

#[test]
fn test_enter_upgrading_command_serialization() {
    let cmd = Command::EnterUpgrading {
        owner: "controller-a".to_string(),
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""command":"enter_upgrading""#));
    assert!(json.contains(r#""owner":"controller-a""#));
}

#[test]
fn test_exit_upgrading_command_serialization() {
    let cmd = Command::ExitUpgrading {
        owner: "controller-a".to_string(),
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""command":"exit_upgrading""#));
    assert!(json.contains(r#""owner":"controller-a""#));
}

#[test]
fn test_list_releases_command_serialization() {
    let cmd = Command::ListReleases {
        app: "my-app".to_string(),
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""command":"list_releases""#));
    assert!(json.contains(r#""app":"my-app""#));
}

#[test]
fn test_rollback_command_serialization() {
    let cmd = Command::Rollback {
        app: "my-app".to_string(),
        version: "abc1234".to_string(),
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""command":"rollback""#));
    assert!(json.contains(r#""app":"my-app""#));
    assert!(json.contains(r#""version":"abc1234""#));
}

#[test]
fn test_delete_command_serialization() {
    let cmd = Command::Delete {
        app: "my-app".to_string(),
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""command":"delete""#));
    assert!(json.contains(r#""app":"my-app""#));
}

#[test]
fn test_response_ok() {
    let response = Response::ok(serde_json::json!({"name": "test"}));
    assert!(response.is_ok());
    assert!(response.data().is_some());
}

struct FailingSerialize;

impl serde::Serialize for FailingSerialize {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom("boom"))
    }
}

#[test]
#[should_panic(expected = "Response::ok data must serialize")]
fn test_response_ok_panics_when_serialization_fails() {
    let _ = Response::ok(FailingSerialize);
}

#[test]
fn test_response_error() {
    let response = Response::error("Something went wrong");
    assert!(!response.is_ok());
    assert_eq!(response.error_message(), Some("Something went wrong"));
}

#[test]
fn test_app_state_display() {
    assert_eq!(AppState::Running.to_string(), "running");
    assert_eq!(AppState::Deploying.to_string(), "deploying");
}

#[test]
fn test_instance_state_display() {
    assert_eq!(InstanceState::Healthy.to_string(), "healthy");
    assert_eq!(InstanceState::Draining.to_string(), "draining");
}

#[test]
fn test_app_status_deserializes_without_builds_field() {
    let value = serde_json::json!({
        "name": "demo",
        "version": "v1",
        "instances": [],
        "state": "running",
        "last_error": null
    });

    let status: AppStatus = serde_json::from_value(value).unwrap();
    assert!(status.builds.is_empty());
}

#[test]
fn test_upgrade_mode_serialization() {
    let mode = UpgradeMode::Upgrading;
    let json = serde_json::to_string(&mode).unwrap();
    assert_eq!(json, r#""upgrading""#);
}

#[test]
fn test_get_secrets_hash_command_serialization() {
    let cmd = Command::GetSecretsHash {
        app: "my-app".to_string(),
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""command":"get_secrets_hash""#));
    assert!(json.contains(r#""app":"my-app""#));
}

#[test]
fn test_compute_secrets_hash_deterministic() {
    let secrets = HashMap::from([
        ("B".to_string(), "2".to_string()),
        ("A".to_string(), "1".to_string()),
    ]);
    let hash1 = compute_secrets_hash(&secrets);
    let hash2 = compute_secrets_hash(&secrets);
    assert_eq!(hash1, hash2);
}

#[test]
fn test_compute_secrets_hash_order_independent() {
    let mut a = HashMap::new();
    a.insert("X".to_string(), "1".to_string());
    a.insert("Y".to_string(), "2".to_string());

    let mut b = HashMap::new();
    b.insert("Y".to_string(), "2".to_string());
    b.insert("X".to_string(), "1".to_string());

    assert_eq!(compute_secrets_hash(&a), compute_secrets_hash(&b));
}

#[test]
fn test_compute_secrets_hash_differs_for_different_values() {
    let a = HashMap::from([("KEY".to_string(), "value1".to_string())]);
    let b = HashMap::from([("KEY".to_string(), "value2".to_string())]);
    assert_ne!(compute_secrets_hash(&a), compute_secrets_hash(&b));
}

#[test]
fn test_compute_secrets_hash_empty_map() {
    let empty = HashMap::new();
    let hash = compute_secrets_hash(&empty);
    assert!(!hash.is_empty());
    // Empty map should produce a consistent hash
    assert_eq!(hash, compute_secrets_hash(&HashMap::new()));
}

#[test]
fn test_enqueue_run_command_roundtrip() {
    let cmd = Command::EnqueueRun {
        app: "my-app".to_string(),
        name: "send-email".to_string(),
        payload: serde_json::json!({ "to": "a@b.c" }),
        opts: EnqueueOpts {
            run_at_ms: Some(1_700_000_000_000),
            max_attempts: Some(5),
            unique_key: Some("cron:send-email:0".to_string()),
        },
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""command":"enqueue_run""#));
    assert!(json.contains(r#""unique_key":"cron:send-email:0""#));
    let parsed: Command = serde_json::from_str(&json).unwrap();
    match parsed {
        Command::EnqueueRun {
            app, name, opts, ..
        } => {
            assert_eq!(app, "my-app");
            assert_eq!(name, "send-email");
            assert_eq!(opts.max_attempts, Some(5));
        }
        _ => panic!("expected EnqueueRun"),
    }
}

#[test]
fn test_enqueue_run_command_defaults_opts_when_missing() {
    let json = r#"{
            "command":"enqueue_run",
            "app":"my-app",
            "name":"w",
            "payload":{}
        }"#;
    let cmd: Command = serde_json::from_str(json).unwrap();
    match cmd {
        Command::EnqueueRun { opts, .. } => {
            assert!(opts.run_at_ms.is_none());
            assert!(opts.max_attempts.is_none());
            assert!(opts.unique_key.is_none());
        }
        _ => panic!("expected EnqueueRun"),
    }
}

#[test]
fn test_enqueue_run_response_serialization() {
    let r = EnqueueRunResponse {
        id: "01abc".to_string(),
        deduplicated: true,
    };
    let json = serde_json::to_string(&r).unwrap();
    let parsed: EnqueueRunResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, r);
}

#[test]
fn test_compute_secrets_hash_no_boundary_collision() {
    // {"A=B":"C"} and {"A":"B=C"} must produce different hashes
    let a = HashMap::from([("A=B".to_string(), "C".to_string())]);
    let b = HashMap::from([("A".to_string(), "B=C".to_string())]);
    assert_ne!(compute_secrets_hash(&a), compute_secrets_hash(&b));
}

#[test]
fn test_deploy_with_none_secrets_keeps_existing() {
    let cmd = Command::Deploy {
        app: "my-app".to_string(),
        version: "v1".to_string(),
        path: "/opt/tako/apps/my-app/releases/v1".to_string(),
        routes: vec!["example.com".to_string()],
        source_ip: SourceIpMode::Auto,
        secrets: None,
        runtime_credentials: None,
        storages: None,
        ssl: SslBinding::default(),
        backup: None,
    };
    let json = serde_json::to_string(&cmd).unwrap();
    let parsed: Command = serde_json::from_str(&json).unwrap();
    match parsed {
        Command::Deploy {
            source_ip,
            secrets,
            storages,
            ssl,
            backup,
            ..
        } => {
            assert_eq!(source_ip, SourceIpMode::Auto);
            assert!(secrets.is_none());
            assert!(storages.is_none());
            assert_eq!(ssl.provider, SslProvider::LetsEncrypt);
            assert!(backup.is_none());
        }
        _ => panic!("Expected deploy command"),
    }
}

#[test]
fn parses_run_release_command() {
    let json = r#"{
            "command": "run_release",
            "app": "my-app",
            "version": "abc1234",
            "path": "/var/lib/tako/my-app/releases/abc1234",
            "command_line": "bun run db:migrate",
            "vars": {"NODE_ENV": "production"},
            "secrets": {"DATABASE_URL": "postgres://x"}
        }"#;
    let cmd: Command = serde_json::from_str(json).unwrap();
    match cmd {
        Command::RunRelease {
            app,
            version,
            path,
            command_line,
            vars,
            secrets,
        } => {
            assert_eq!(app, "my-app");
            assert_eq!(version, "abc1234");
            assert!(path.contains("releases"));
            assert_eq!(command_line, "bun run db:migrate");
            assert_eq!(vars.get("NODE_ENV").map(String::as_str), Some("production"));
            assert_eq!(
                secrets.get("DATABASE_URL").map(String::as_str),
                Some("postgres://x")
            );
        }
        _ => panic!("Expected RunRelease command"),
    }
}

#[test]
fn test_server_runtime_info_pid_roundtrip() {
    let info = ServerRuntimeInfo {
        pid: 42,
        mode: UpgradeMode::Normal,
        process_started_at_unix_secs: Some(1_778_220_000),
        socket: "/var/run/tako/tako.sock".to_string(),
        data_dir: "/var/lib/tako".to_string(),
        http_port: 80,
        https_port: 443,
        no_acme: false,
        acme_staging: false,
        acme_email: None,
        renewal_interval_hours: 12,
        standby: false,
        metrics_port: Some(9898),
        server_name: Some("la".to_string()),
        server_identity: Some("SHA256:testidentity".to_string()),
        storage_engine: Some("turso".to_string()),
    };
    let json = serde_json::to_string(&info).unwrap();
    let parsed: ServerRuntimeInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.pid, 42);
    assert_eq!(parsed.server_name.as_deref(), Some("la"));
    assert_eq!(
        parsed.server_identity.as_deref(),
        Some("SHA256:testidentity")
    );
    assert_eq!(parsed.storage_engine.as_deref(), Some("turso"));
}

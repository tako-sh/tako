use super::*;
use crate::config::{EncryptedSecretValue, EnvConfig, POSTGRES_CREDENTIAL_NAME};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

#[test]
fn release_command_payload_pins_the_secret_snapshot() {
    let cfg = DeployConfig {
        app_name: "my-app/production".to_string(),
        version: "v1".to_string(),
        routes: vec![],
        source_ip: tako_core::SourceIpMode::Auto,
        secrets_digest: Some("digest".to_string()),
        storages: HashMap::new(),
        databases: HashMap::from([(
            "main".into(),
            tako_core::DatabaseBinding {
                name: "orders".into(),
                server: "s1".into(),
                peer_key: None,
            },
        )]),
        channel_database: Some(tako_core::DatabaseBinding {
            name: "replay".into(),
            server: "s2".into(),
            peer_key: None,
        }),
        ssl: tako_core::SslBinding::default(),
        backup: None,
        force: true,
        main: "index.ts".to_string(),
        use_unified_target_process: false,
        release_command: Some("bun run migrate".to_string()),
        leader_server: "prod".to_string(),
    };

    let Some(tako_core::Command::RunRelease {
        app,
        version,
        path,
        command_line,
        secrets_digest,
        databases,
        force,
        ..
    }) = cfg.release_command_payload("/opt/tako/apps/my-app/production/releases/v1")
    else {
        panic!("expected run_release command payload");
    };

    assert_eq!(app, "my-app/production");
    assert_eq!(version, "v1");
    assert_eq!(path, "/opt/tako/apps/my-app/production/releases/v1");
    assert_eq!(command_line, "bun run migrate");
    assert!(force);
    assert_eq!(secrets_digest.as_deref(), Some("digest"));
    assert_eq!(
        databases, cfg.databases,
        "release commands get only SQL bindings"
    );
    let full = cfg.deploy_databases(true).unwrap();
    assert_eq!(full.len(), 2);
    let scoped = cfg.deploy_databases(false).unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[tako_core::CHANNEL_REPLAY_BINDING].name, "replay");
}

#[test]
fn source_bundle_root_falls_back_to_runtime_project_root_without_git() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("app");
    std::fs::create_dir_all(&project_dir).unwrap();
    // No lockfile anywhere → falls back to project_dir itself
    assert_eq!(source_bundle_root(&project_dir, "bun"), project_dir);
}

#[test]
fn source_bundle_root_walks_up_to_lockfile_without_git() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("monorepo");
    let project_dir = root.join("apps/web");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(root.join("bun.lock"), "").unwrap();
    // No git, but lockfile is at the monorepo root → returns lockfile root
    assert_eq!(source_bundle_root(&project_dir, "bun"), root);
}

#[test]
fn acquire_project_deploy_lock_writes_current_pid() {
    let temp = TempDir::new().unwrap();
    let _lock = acquire_project_deploy_lock(temp.path()).unwrap();

    let pid_path = deploy_lock_path(temp.path());
    assert_eq!(
        fs::read_to_string(pid_path).unwrap().trim(),
        std::process::id().to_string()
    );
}

#[test]
fn acquire_project_deploy_lock_rejects_second_holder() {
    let temp = TempDir::new().unwrap();
    let _lock = acquire_project_deploy_lock(temp.path()).unwrap();

    let err = acquire_project_deploy_lock(temp.path()).unwrap_err();
    assert!(err.contains("Another deploy is already running"));
    assert!(err.contains(&std::process::id().to_string()));
}

#[test]
fn acquire_project_deploy_lock_allows_reacquire_after_drop() {
    let temp = TempDir::new().unwrap();
    let first = acquire_project_deploy_lock(temp.path()).unwrap();
    drop(first);

    let second = acquire_project_deploy_lock(temp.path()).unwrap();
    let pid_path = deploy_lock_path(temp.path());
    assert_eq!(
        fs::read_to_string(pid_path).unwrap().trim(),
        std::process::id().to_string()
    );
    drop(second);
}

#[test]
fn runtime_declarations_do_not_require_storage_on_multiple_servers() {
    let temp = TempDir::new().unwrap();
    for suffix in ["ts", "mts", "cts"] {
        write_workflow(
            &temp,
            &format!("network.d.{suffix}"),
            "export type Remote = unknown;",
        );
        for kind in ["channels", "presence", "stores"] {
            let dir = temp.path().join("src").join(kind);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(format!("types.d.{suffix}")),
                "export type Remote = unknown;",
            )
            .unwrap();
        }
    }
    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &multi_server_js_config(),
        &SecretsStore::default(),
        "production",
        2,
    );
    assert!(!result.has_errors(), "{:?}", result.errors);
}

#[test]
fn workflow_storage_validation_rejects_multi_server_without_postgres_url() {
    let temp = TempDir::new().unwrap();
    write_workflow(
        &temp,
        "daily.ts",
        r#"export default defineWorkflow("daily", { handler: async () => {} });"#,
    );
    let config = multi_server_js_config();
    let secrets = SecretsStore::default();

    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &config,
        &secrets,
        "production",
        2,
    );

    assert!(result.has_errors());
    assert!(
        result.errors[0].contains("postgres_url"),
        "{:?}",
        result.errors
    );
}

#[test]
fn workflow_storage_validation_allows_multi_server_with_postgres_url() {
    let temp = TempDir::new().unwrap();
    write_workflow(
        &temp,
        "daily.ts",
        r#"export default defineWorkflow("daily", { handler: async () => {} });"#,
    );
    let config = multi_server_js_config();
    let mut secrets = SecretsStore::default();
    secrets.ensure_env_key_id("production").unwrap();
    secrets
        .set_credential(
            "production",
            POSTGRES_CREDENTIAL_NAME,
            EncryptedSecretValue::new("encrypted".to_string(), None),
        )
        .unwrap();

    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &config,
        &secrets,
        "production",
        2,
    );

    assert!(
        !result.has_errors(),
        "unexpected errors: {:?}",
        result.errors
    );
}

#[test]
fn workflow_storage_validation_ignores_multi_server_without_workflows_dir() {
    let temp = TempDir::new().unwrap();
    let mut config = TakoToml::default();
    config.envs.insert(
        "production".to_string(),
        EnvConfig {
            servers: vec!["sfo".to_string(), "iad".to_string()],
            ..Default::default()
        },
    );
    let secrets = SecretsStore::default();

    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &config,
        &secrets,
        "production",
        2,
    );

    assert!(
        !result.has_errors(),
        "unexpected errors: {:?}",
        result.errors
    );
}

#[cfg(unix)]
#[test]
fn workflow_storage_validation_rejects_unreadable_workflows_dir() {
    let temp = TempDir::new().unwrap();
    let workflows_dir = temp.path().join("src/workflows");
    fs::create_dir_all(&workflows_dir).unwrap();
    fs::set_permissions(&workflows_dir, fs::Permissions::from_mode(0o000)).unwrap();
    let config = multi_server_js_config();
    let secrets = SecretsStore::default();

    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &config,
        &secrets,
        "production",
        2,
    );

    fs::set_permissions(&workflows_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.has_errors());
    assert!(
        result.errors[0].contains("local: true"),
        "{:?}",
        result.errors
    );
}

#[test]
fn workflow_storage_validation_allows_define_workflow_local_true_multi_server_workflows() {
    let temp = TempDir::new().unwrap();
    write_workflow(
        &temp,
        "daily.ts",
        r#"export default defineWorkflow("daily", { local: true, handler: async () => {} });"#,
    );
    let config = multi_server_js_config();
    let secrets = SecretsStore::default();

    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &config,
        &secrets,
        "production",
        2,
    );

    assert!(
        !result.has_errors(),
        "unexpected errors: {:?}",
        result.errors
    );
}

#[test]
fn workflow_storage_validation_rejects_mixed_local_and_remote_multi_server_workflows() {
    let temp = TempDir::new().unwrap();
    write_workflow(
        &temp,
        "local.ts",
        r#"export default defineWorkflow("local", { local: true, handler: async () => {} });"#,
    );
    write_workflow(
        &temp,
        "remote.ts",
        r#"export default defineWorkflow("remote", { handler: async () => {} });"#,
    );
    let config = multi_server_js_config();
    let secrets = SecretsStore::default();

    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &config,
        &secrets,
        "production",
        2,
    );

    assert!(result.has_errors());
    assert!(
        result.errors[0].contains("local: true"),
        "{:?}",
        result.errors
    );
}

#[test]
fn workflow_storage_validation_rejects_unrelated_local_true_outside_define_workflow() {
    let temp = TempDir::new().unwrap();
    write_workflow(
        &temp,
        "daily.ts",
        r#"
const opts = { local: true };
export default defineWorkflow("daily", { handler: async () => opts });
"#,
    );
    let config = multi_server_js_config();
    let secrets = SecretsStore::default();

    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &config,
        &secrets,
        "production",
        2,
    );

    assert!(result.has_errors());
    assert!(
        result.errors[0].contains("local: true"),
        "{:?}",
        result.errors
    );
}

#[test]
fn workflow_storage_validation_handles_non_ascii_source_when_scanning_local_true() {
    let temp = TempDir::new().unwrap();
    write_workflow(
        &temp,
        "daily.ts",
        r#"
// café
export default defineWorkflow("daily", { local: true, handler: async () => {} });
"#,
    );
    let config = multi_server_js_config();
    let secrets = SecretsStore::default();

    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &config,
        &secrets,
        "production",
        2,
    );

    assert!(
        !result.has_errors(),
        "unexpected errors: {:?}",
        result.errors
    );
}

#[test]
fn runtime_state_storage_validation_allows_multi_server_channels_with_postgres_url() {
    let temp = TempDir::new().unwrap();
    write_channel(&temp, "chat.ts", r#"export default defineChannel("chat");"#);
    let config = multi_server_js_config();
    let mut secrets = SecretsStore::default();
    secrets.ensure_env_key_id("production").unwrap();
    secrets
        .set_credential(
            "production",
            POSTGRES_CREDENTIAL_NAME,
            EncryptedSecretValue::new("encrypted".to_string(), None),
        )
        .unwrap();

    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &config,
        &secrets,
        "production",
        2,
    );

    assert!(
        !result.has_errors(),
        "unexpected errors: {:?}",
        result.errors
    );
}

#[test]
fn workflow_storage_validation_rejects_multi_server_go_worker_without_postgres_url() {
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("cmd/worker")).unwrap();
    fs::write(temp.path().join("cmd/worker/main.go"), "package main").unwrap();
    let config = TakoToml {
        runtime: Some("go".to_string()),
        ..Default::default()
    };

    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &config,
        &SecretsStore::default(),
        "production",
        2,
    );

    assert!(
        result
            .errors
            .iter()
            .any(|error| error.contains("Workflows")),
        "{:?}",
        result.errors
    );
}

fn native_multi_server_js_config() -> TakoToml {
    let mut config = multi_server_js_config();
    let production = config.envs.get_mut("production").unwrap();
    production.servers.clear();
    config
}

#[test]
fn runtime_state_storage_validation_native_channels_presence_and_stores_allow_sqlite() {
    for directory in ["channels", "presence", "stores"] {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("src").join(directory)).unwrap();
        fs::write(
            temp.path().join("src").join(directory).join("live.ts"),
            "export default {};",
        )
        .unwrap();
        for server_count in [1, 2] {
            let result = config::validate_runtime_state_storage_for_deploy(
                temp.path(),
                &native_multi_server_js_config(),
                &SecretsStore::default(),
                "production",
                server_count,
            );
            assert!(!result.has_errors(), "{directory}: {:?}", result.errors);
        }
    }
}

#[test]
fn runtime_state_storage_validation_native_channels_do_not_replace_workflow_storage() {
    let temp = TempDir::new().unwrap();
    write_channel(&temp, "chat.ts", "export default defineChannel(\"chat\");");
    write_workflow(
        &temp,
        "daily.ts",
        "export default defineWorkflow(\"daily\", { handler: async () => {} });",
    );
    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &native_multi_server_js_config(),
        &SecretsStore::default(),
        "production",
        2,
    );
    assert!(result.has_errors());
    assert!(result.errors[0].contains("Workflows"));
    assert!(result.errors[0].contains("postgres_url"));
    assert!(!result.errors[0].contains("Channels"));
}

#[test]
fn runtime_state_storage_validation_native_channels_allow_intentionally_local_workflows() {
    let temp = TempDir::new().unwrap();
    write_channel(&temp, "chat.ts", "export default defineChannel(\"chat\");");
    write_workflow(
        &temp,
        "daily.ts",
        "export default defineWorkflow(\"daily\", { local: true, handler: async () => {} });",
    );
    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &native_multi_server_js_config(),
        &SecretsStore::default(),
        "production",
        2,
    );
    assert!(!result.has_errors(), "{:?}", result.errors);
}

#[test]
fn runtime_state_storage_validation_native_channels_validate_configured_postgres() {
    let temp = TempDir::new().unwrap();
    write_channel(&temp, "chat.ts", "export default defineChannel(\"chat\");");
    for (expiry, expected_errors) in [
        (None, false),
        (Some("2000-01-01"), true),
        (Some("invalid"), true),
    ] {
        let mut secrets = SecretsStore::default();
        secrets.ensure_env_key_id("production").unwrap();
        secrets
            .set_credential(
                "production",
                POSTGRES_CREDENTIAL_NAME,
                EncryptedSecretValue::new("encrypted".into(), None),
            )
            .unwrap();
        secrets
            .environments
            .get_mut("production")
            .unwrap()
            .credentials
            .get_mut(POSTGRES_CREDENTIAL_NAME)
            .unwrap()
            .expires_on = expiry.map(str::to_string);
        let result = config::validate_runtime_state_storage_for_deploy(
            temp.path(),
            &native_multi_server_js_config(),
            &secrets,
            "production",
            2,
        );
        assert_eq!(result.has_errors(), expected_errors, "{:?}", result.errors);
        assert!(
            secrets
                .get_credential("production", POSTGRES_CREDENTIAL_NAME)
                .is_some()
        );
    }
}

#[test]
fn hosted_channel_replay_ignores_an_expired_postgres_url_unless_workflows_use_it() {
    let temp = TempDir::new().unwrap();
    write_channel(&temp, "chat.ts", "export default defineChannel(\"chat\");");
    let mut config = native_multi_server_js_config();
    config.envs.get_mut("production").unwrap().channels = Some(crate::config::ChannelsConfig {
        database: "events".into(),
    });
    let mut secrets = SecretsStore::default();
    secrets.ensure_env_key_id("production").unwrap();
    secrets
        .set_credential(
            "production",
            POSTGRES_CREDENTIAL_NAME,
            EncryptedSecretValue::new("encrypted".into(), Some("2000-01-01".into())),
        )
        .unwrap();
    let validate = || {
        config::validate_runtime_state_storage_for_deploy(
            temp.path(),
            &config,
            &secrets,
            "production",
            2,
        )
    };
    assert!(!validate().has_errors(), "{:?}", validate().errors);
    write_workflow(
        &temp,
        "daily.ts",
        r#"export default defineWorkflow("daily", { handler: async () => {} });"#,
    );
    assert!(validate().errors[0].contains("expired"));
}

fn write_workflow(temp: &TempDir, name: &str, source: &str) {
    let workflows_dir = temp.path().join("src/workflows");
    fs::create_dir_all(&workflows_dir).unwrap();
    fs::write(workflows_dir.join(name), source).unwrap();
}

fn write_channel(temp: &TempDir, name: &str, source: &str) {
    let channels_dir = temp.path().join("src/channels");
    fs::create_dir_all(&channels_dir).unwrap();
    fs::write(channels_dir.join(name), source).unwrap();
}

fn multi_server_js_config() -> TakoToml {
    TakoToml {
        app_root: Some("src".to_string()),
        envs: HashMap::from([(
            "production".to_string(),
            EnvConfig {
                servers: vec!["sfo".to_string(), "iad".to_string()],
                ..Default::default()
            },
        )]),
        ..Default::default()
    }
}

#[test]
fn native_releases_need_channel_replay_only_with_a_tako_sdk() {
    let temp = TempDir::new().unwrap();
    let config = TakoToml::parse("name = 'api'\nruntime = 'go'\n").unwrap();
    let needs = || config::needs_channel_replay(temp.path(), &config, BuildAdapter::Go);
    fs::write(temp.path().join("go.mod"), "module api\n\ngo 1.22\n").unwrap();
    assert!(!needs());
    fs::write(
        temp.path().join("go.mod"),
        "module api\n\ngo 1.22\n\nrequire tako.sh v0.4.1\n",
    )
    .unwrap();
    assert!(needs());
    // A custom-built Rust binary counts before its first build writes Cargo.lock.
    let rust = TempDir::new().unwrap();
    fs::write(
        rust.path().join("Cargo.toml"),
        "[package]\nname = 'api'\n[dependencies]\ntako = '0.3'\n",
    )
    .unwrap();
    assert!(config::needs_channel_replay(
        rust.path(),
        &config,
        BuildAdapter::Unknown
    ));
}

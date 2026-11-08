use super::*;
use crate::config::{EncryptedSecretValue, EnvConfig, POSTGRES_CREDENTIAL_NAME};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

#[test]
fn release_command_payload_includes_deploy_secrets() {
    let cfg = DeployConfig {
        app_name: "my-app/production".to_string(),
        version: "v1".to_string(),
        routes: vec![],
        source_ip: tako_core::SourceIpMode::Auto,
        secrets: HashMap::from([("DATABASE_URL".to_string(), "postgres://new".to_string())]),
        runtime_credentials: HashMap::new(),
        storages: HashMap::new(),
        ssl: tako_core::SslBinding::default(),
        backup: None,
        force: true,
        secrets_hash: String::new(),
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
        secrets,
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
    assert_eq!(
        secrets.get("DATABASE_URL").map(String::as_str),
        Some("postgres://new")
    );
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
fn runtime_state_storage_validation_rejects_multi_server_channels_without_postgres_url() {
    let temp = TempDir::new().unwrap();
    write_channel(&temp, "chat.ts", r#"export default defineChannel("chat");"#);
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
    assert!(result.errors[0].contains("Channels"), "{:?}", result.errors);
    assert!(
        result.errors[0].contains("postgres_url"),
        "{:?}",
        result.errors
    );
}

#[test]
fn runtime_state_storage_validation_requires_postgres_for_multi_server_presence() {
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("src/presence")).unwrap();
    fs::write(
        temp.path().join("src/presence/lobby.ts"),
        r#"export default definePresence("lobby");"#,
    )
    .unwrap();
    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &multi_server_js_config(),
        &SecretsStore::default(),
        "production",
        2,
    );
    assert!(result.has_errors());
    assert!(result.errors[0].contains("postgres_url"));
}

#[test]
fn runtime_state_storage_validation_requires_postgres_for_multi_server_stores() {
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("app/stores")).unwrap();
    fs::write(
        temp.path().join("app/stores/messages.ts"),
        r#"export default defineStore("messages", { load: async () => [] });"#,
    )
    .unwrap();
    let mut tako_config = multi_server_js_config();
    tako_config.app_root = Some("app".to_string());
    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &tako_config,
        &SecretsStore::default(),
        "production",
        2,
    );
    assert!(result.has_errors());
    assert!(result.errors[0].contains("stores"));
    assert!(result.errors[0].contains("postgres_url"));
}

#[cfg(unix)]
#[test]
fn runtime_state_storage_validation_rejects_unreadable_channels_dir() {
    let temp = TempDir::new().unwrap();
    let channels_dir = temp.path().join("src/channels");
    fs::create_dir_all(&channels_dir).unwrap();
    fs::set_permissions(&channels_dir, fs::Permissions::from_mode(0o000)).unwrap();
    let config = multi_server_js_config();
    let secrets = SecretsStore::default();

    let result = config::validate_runtime_state_storage_for_deploy(
        temp.path(),
        &config,
        &secrets,
        "production",
        2,
    );

    fs::set_permissions(&channels_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.has_errors());
    assert!(result.errors[0].contains("Channels"), "{:?}", result.errors);
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

#[test]
fn runtime_state_storage_validation_channels_override_all_local_workflows() {
    let temp = TempDir::new().unwrap();
    write_workflow(
        &temp,
        "daily.ts",
        r#"export default defineWorkflow("daily", { local: true, handler: async () => {} });"#,
    );
    write_channel(&temp, "chat.ts", r#"export default defineChannel("chat");"#);
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
    assert!(result.errors[0].contains("Channels"), "{:?}", result.errors);
}

fn native_multi_server_js_config() -> TakoToml {
    let mut config = multi_server_js_config();
    let production = config.envs.get_mut("production").unwrap();
    production.servers.clear();
    production.network = Some("0123456789abcdef0123456789abcdef".into());
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
fn runtime_state_storage_validation_uses_only_the_selected_environment_network() {
    let temp = TempDir::new().unwrap();
    write_channel(&temp, "chat.ts", "export default defineChannel(\"chat\");");
    let mut config = native_multi_server_js_config();
    config.envs.insert(
        "staging".into(),
        EnvConfig {
            servers: vec!["sfo".into(), "iad".into()],
            ..Default::default()
        },
    );
    for (server_count, expected_errors) in [(1, false), (2, true)] {
        let result = config::validate_runtime_state_storage_for_deploy(
            temp.path(),
            &config,
            &SecretsStore::default(),
            "staging",
            server_count,
        );
        assert_eq!(result.has_errors(), expected_errors, "{:?}", result.errors);
    }
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

use super::*;
use crate::config::{EnvConfig, ServerEntry, ServerTarget, ServersToml, TakoToml};
use tempfile::TempDir;

#[test]
fn resolve_deploy_environment_prefers_explicit_env() {
    let mut config = TakoToml::default();
    config.envs.insert(
        "production".to_string(),
        EnvConfig {
            route: Some("prod.example.com".to_string()),
            ..Default::default()
        },
    );
    config.envs.insert(
        "staging".to_string(),
        EnvConfig {
            route: Some("staging.example.com".to_string()),
            ..Default::default()
        },
    );

    let resolved = resolve_deploy_environment(Some("staging"), &config).unwrap();
    assert_eq!(resolved, "staging");
}

#[test]
fn resolve_build_preset_ref_prefers_tako_toml_override() {
    let temp = TempDir::new().unwrap();
    let config = TakoToml {
        runtime: Some("bun".to_string()),
        preset: Some("tanstack-start@abc1234".to_string()),
        ..Default::default()
    };

    assert_eq!(
        resolve_build_preset_ref(temp.path(), &config).unwrap(),
        "javascript/tanstack-start@abc1234"
    );
}

#[test]
fn resolve_build_preset_ref_qualifies_runtime_local_alias() {
    let temp = TempDir::new().unwrap();
    let config = TakoToml {
        runtime: Some("bun".to_string()),
        preset: Some("tanstack-start".to_string()),
        ..Default::default()
    };

    assert_eq!(
        resolve_build_preset_ref(temp.path(), &config).unwrap(),
        "javascript/tanstack-start"
    );
}

#[test]
fn resolve_build_preset_ref_errors_when_runtime_is_unknown_for_local_alias() {
    let temp = TempDir::new().unwrap();
    let config = TakoToml {
        preset: Some("tanstack-start".to_string()),
        ..Default::default()
    };

    let err = resolve_build_preset_ref(temp.path(), &config).unwrap_err();
    assert!(err.contains("Cannot resolve preset"));
}

#[test]
fn resolve_build_preset_ref_falls_back_to_detected_adapter_default() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("package.json"), r#"{"name":"demo"}"#).unwrap();
    let config = TakoToml::default();
    assert_eq!(
        resolve_build_preset_ref(temp.path(), &config).unwrap(),
        "node"
    );
}

#[test]
fn resolve_build_preset_ref_uses_build_adapter_override_when_preset_is_missing() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("package.json"), r#"{"name":"demo"}"#).unwrap();
    let config = TakoToml {
        runtime: Some("node".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve_build_preset_ref(temp.path(), &config).unwrap(),
        "node"
    );
}

#[test]
fn resolve_build_preset_ref_rejects_unknown_build_adapter_override() {
    let temp = TempDir::new().unwrap();
    let config = TakoToml {
        runtime: Some("python".to_string()),
        ..Default::default()
    };
    let err = resolve_build_preset_ref(temp.path(), &config).unwrap_err();
    assert!(err.contains("Invalid runtime"));
}

#[test]
fn resolve_effective_build_adapter_uses_preset_group_when_detection_is_unknown() {
    let temp = TempDir::new().unwrap();
    let config = TakoToml::default();

    let adapter = resolve_effective_build_adapter(temp.path(), &config, "bun").unwrap();
    assert_eq!(adapter, BuildAdapter::Bun);
}

#[test]
fn resolve_effective_build_adapter_prefers_runtime_override() {
    let temp = TempDir::new().unwrap();
    let config = TakoToml {
        runtime: Some("node".to_string()),
        ..Default::default()
    };

    let adapter = resolve_effective_build_adapter(temp.path(), &config, "tanstack-start").unwrap();
    assert_eq!(adapter, BuildAdapter::Node);
}

#[test]
fn has_bun_lockfile_detects_both_supported_lockfiles() {
    let temp = TempDir::new().unwrap();
    assert!(!has_bun_lockfile(temp.path()));

    std::fs::write(temp.path().join("bun.lock"), "").unwrap();
    assert!(has_bun_lockfile(temp.path()));

    std::fs::remove_file(temp.path().join("bun.lock")).unwrap();
    std::fs::write(temp.path().join("bun.lockb"), "").unwrap();
    assert!(has_bun_lockfile(temp.path()));
}

#[test]
fn run_bun_lockfile_preflight_skips_when_lockfile_is_missing() {
    let temp = TempDir::new().unwrap();
    let checked = run_bun_lockfile_preflight(temp.path()).unwrap();
    assert!(!checked);
}

#[test]
fn resolve_deploy_environment_rejects_development() {
    let config = TakoToml::default();

    let err = resolve_deploy_environment(Some("development"), &config).unwrap_err();
    assert!(err.contains("reserved for local development"));
}

#[test]
fn resolve_deploy_environment_defaults_to_production_with_single_server() {
    let mut config = TakoToml::default();
    config.envs.insert(
        "production".to_string(),
        EnvConfig {
            route: Some("prod.example.com".to_string()),
            ..Default::default()
        },
    );

    let resolved = resolve_deploy_environment(None, &config).unwrap();
    assert_eq!(resolved, "production");
}

#[test]
fn resolve_deploy_environment_defaults_to_production() {
    let mut config = TakoToml::default();
    config.envs.insert(
        "production".to_string(),
        EnvConfig {
            route: Some("prod.example.com".to_string()),
            ..Default::default()
        },
    );

    let resolved = resolve_deploy_environment(None, &config).unwrap();
    assert_eq!(resolved, "production");
}

#[test]
fn resolve_deploy_environment_rejects_missing_requested_environment() {
    let mut config = TakoToml::default();
    config.envs.insert(
        "production".to_string(),
        EnvConfig {
            route: Some("prod.example.com".to_string()),
            ..Default::default()
        },
    );

    let err = resolve_deploy_environment(Some("staging"), &config).unwrap_err();
    assert!(err.contains("Environment 'staging' not found"));
}

#[test]
fn resolve_deploy_environment_rejects_missing_default_production_environment() {
    let mut config = TakoToml::default();
    config.envs.insert(
        "staging".to_string(),
        EnvConfig {
            route: Some("staging.example.com".to_string()),
            ..Default::default()
        },
    );

    let err = resolve_deploy_environment(None, &config).unwrap_err();
    assert!(err.contains("Environment 'production' not found"));
}

#[test]
fn should_confirm_production_deploy_requires_interactive_unless_yes_is_set() {
    assert!(should_confirm_production_deploy(
        "production",
        false,
        true,
        true
    ));
    assert!(!should_confirm_production_deploy(
        "production",
        true,
        true,
        true
    ));
    assert!(!should_confirm_production_deploy(
        "production",
        false,
        false,
        true
    ));
    assert!(!should_confirm_production_deploy(
        "staging", false, true, true
    ));
}

#[test]
fn should_confirm_production_deploy_skips_when_only_one_deploy_target_exists() {
    assert!(!should_confirm_production_deploy(
        "production",
        false,
        true,
        false
    ));
}

#[test]
fn has_multiple_deploy_targets_is_false_for_single_production_env() {
    let mut config = TakoToml::default();
    config.envs.insert(
        "production".to_string(),
        EnvConfig {
            route: Some("prod.example.com".to_string()),
            ..Default::default()
        },
    );

    assert!(!has_multiple_deploy_targets(&config));
}

#[test]
fn has_multiple_deploy_targets_ignores_the_reserved_development_env() {
    let mut config = TakoToml::default();
    config.envs.insert(
        "production".to_string(),
        EnvConfig {
            route: Some("prod.example.com".to_string()),
            ..Default::default()
        },
    );
    config.envs.insert(
        "development".to_string(),
        EnvConfig {
            route: Some("localhost".to_string()),
            ..Default::default()
        },
    );

    assert!(!has_multiple_deploy_targets(&config));
}

#[test]
fn has_multiple_deploy_targets_is_true_with_staging_and_production() {
    let mut config = TakoToml::default();
    config.envs.insert(
        "production".to_string(),
        EnvConfig {
            route: Some("prod.example.com".to_string()),
            ..Default::default()
        },
    );
    config.envs.insert(
        "staging".to_string(),
        EnvConfig {
            route: Some("staging.example.com".to_string()),
            ..Default::default()
        },
    );

    assert!(has_multiple_deploy_targets(&config));
}

#[test]
fn format_production_deploy_confirm_prompt_is_short() {
    let prompt = format_production_deploy_confirm_prompt();
    assert!(prompt.contains("production"));
    assert!(!prompt.contains("--yes"));
}

#[test]
fn format_production_deploy_confirm_hint_mentions_yes_flag() {
    let hint = format_production_deploy_confirm_hint();
    assert!(hint.contains("--yes"));
    assert!(hint.contains("-y"));
}

#[test]
fn resolve_deploy_servers_prefers_explicit_mapping() {
    let mut config = TakoToml::default();
    config.envs.insert(
        "production".to_string(),
        EnvConfig {
            servers: vec!["prod-1".to_string()],
            ..Default::default()
        },
    );

    let mut servers = ServersToml::default();
    servers.servers.insert(
        "prod-1".to_string(),
        ServerEntry {
            host: "127.0.0.1".to_string(),
            port: 22,
            description: None,
            ..Default::default()
        },
    );

    let resolved = resolve_deploy_server_names(&config, &servers, "production").unwrap();
    assert_eq!(resolved, vec!["prod-1".to_string()]);
}

#[test]
fn resolve_deploy_servers_require_explicit_mapping() {
    let mut config = TakoToml::default();
    config
        .envs
        .insert("production".to_string(), Default::default());

    let mut servers = ServersToml::default();
    servers.servers.insert(
        "solo".to_string(),
        ServerEntry {
            host: "127.0.0.1".to_string(),
            port: 22,
            description: None,
            ..Default::default()
        },
    );

    let err = resolve_deploy_server_names(&config, &servers, "production").unwrap_err();
    assert!(err.contains("No servers configured for environment 'production'"));
}

#[test]
fn resolve_deploy_servers_errors_with_hint_when_no_global_servers_exist() {
    let mut config = TakoToml::default();
    config
        .envs
        .insert("production".to_string(), Default::default());
    let servers = ServersToml::default();

    let err = resolve_deploy_server_names(&config, &servers, "production").unwrap_err();
    assert!(err.contains("No servers have been added"));
    assert!(err.contains("tako servers add <host>"));
}

#[test]
fn resolve_deploy_servers_errors_for_non_production_without_mapping() {
    let mut config = TakoToml::default();
    config
        .envs
        .insert("staging".to_string(), Default::default());
    let mut servers = ServersToml::default();
    servers.servers.insert(
        "solo".to_string(),
        ServerEntry {
            host: "127.0.0.1".to_string(),
            port: 22,
            description: None,
            ..Default::default()
        },
    );

    let err = resolve_deploy_server_names(&config, &servers, "staging").unwrap_err();
    assert!(err.contains("No servers configured for environment 'staging'"));
}

#[test]
fn persist_server_env_mapping_updates_env_server_list() {
    let temp_dir = TempDir::new().unwrap();
    std::fs::write(
        temp_dir.path().join("tako.toml"),
        r#"
name = "test-app"

[envs.production]
route = "app.example.com"
"#,
    )
    .unwrap();

    persist_server_env_mapping(
        &temp_dir.path().join("tako.toml"),
        "tako-server",
        "production",
    )
    .unwrap();

    let saved = TakoToml::load_from_dir(temp_dir.path()).unwrap();
    assert_eq!(saved.get_servers_for_env("production"), vec!["tako-server"]);
}

#[tokio::test]
async fn resolve_deploy_servers_with_setup_persists_single_server_mapping() {
    let config = TakoToml {
        name: Some("test-app".to_string()),
        envs: [(
            "production".to_string(),
            EnvConfig {
                route: Some("app.example.com".to_string()),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let mut servers = ServersToml::default();
    servers.servers.insert(
        "solo".to_string(),
        ServerEntry {
            host: "127.0.0.1".to_string(),
            port: 22,
            description: None,
            ..Default::default()
        },
    );

    let temp_dir = TempDir::new().unwrap();
    std::fs::write(
        temp_dir.path().join("tako.toml"),
        r#"
name = "test-app"

[envs.production]
route = "app.example.com"
"#,
    )
    .unwrap();

    let resolved = resolve_deploy_server_names_with_setup(
        &config,
        &mut servers,
        "production",
        &temp_dir.path().join("tako.toml"),
    )
    .await
    .unwrap();
    assert_eq!(resolved, vec!["solo".to_string()]);

    let saved = TakoToml::load_from_dir(temp_dir.path()).unwrap();
    assert_eq!(saved.get_servers_for_env("production"), vec!["solo"]);
}

#[tokio::test]
async fn resolve_deploy_servers_with_setup_requires_interactive_selection_when_multiple_servers() {
    let config = TakoToml::default();
    let mut servers = ServersToml::default();
    servers.servers.insert(
        "a".to_string(),
        ServerEntry {
            host: "10.0.0.1".to_string(),
            port: 22,
            description: None,
            ..Default::default()
        },
    );
    servers.servers.insert(
        "b".to_string(),
        ServerEntry {
            host: "10.0.0.2".to_string(),
            port: 22,
            description: Some("backup".to_string()),
            ..Default::default()
        },
    );

    let temp_dir = TempDir::new().unwrap();
    let err = resolve_deploy_server_names_with_setup(
        &config,
        &mut servers,
        "production",
        &temp_dir.path().join("tako.toml"),
    )
    .await
    .unwrap_err();
    assert!(err.contains("No servers configured for environment 'production'"));
}

#[test]
fn resolve_deploy_server_targets_requires_metadata_for_each_server() {
    let mut servers = ServersToml::default();
    servers.servers.insert(
        "prod-1".to_string(),
        ServerEntry {
            host: "10.0.0.1".to_string(),
            port: 22,
            description: None,
            ..Default::default()
        },
    );

    let err = resolve_deploy_server_targets(&servers, &["prod-1".to_string()]).unwrap_err();
    assert!(err.contains("missing targets"));
    assert!(err.contains("prod-1"));
    assert!(err.contains("does not probe"));
}

#[test]
fn resolve_deploy_server_targets_rejects_invalid_values() {
    let mut servers = ServersToml::default();
    servers.servers.insert(
        "prod-1".to_string(),
        ServerEntry {
            host: "10.0.0.1".to_string(),
            port: 22,
            description: None,
            ..Default::default()
        },
    );
    servers.server_targets.insert(
        "prod-1".to_string(),
        ServerTarget {
            arch: "sparc".to_string(),
            libc: "glibc".to_string(),
        },
    );

    let err = resolve_deploy_server_targets(&servers, &["prod-1".to_string()]).unwrap_err();
    assert!(err.contains("invalid targets"));
    assert!(err.contains("sparc"));
}

#[test]
fn should_run_bun_lockfile_preflight_runs_for_bun_runtime() {
    assert!(should_run_bun_lockfile_preflight(BuildAdapter::Bun));
}

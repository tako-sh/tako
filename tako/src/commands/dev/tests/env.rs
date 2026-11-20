use super::*;

#[test]
fn custom_environment_selects_only_its_vars_routes_and_data() {
    let cfg = TakoToml::parse(
        r#"
[vars]
SHARED = "shared"
[vars.development]
VALUE = "dev"
DEV_ONLY = "dev"
[vars.test-e2e]
VALUE = "test"
[envs.development]
route = "app.test"
[envs.test-e2e]
route = "app-test-e2e.test"
"#,
    )
    .unwrap();
    let mut env = compute_dev_env(&cfg, "test-e2e");
    assert_eq!(env["ENV"], "test-e2e");
    assert_eq!(env["VALUE"], "test");
    assert_eq!(env["SHARED"], "shared");
    assert!(!env.contains_key("DEV_ONLY"));
    assert_eq!(
        compute_dev_hosts("app-test-e2e", &cfg, "fallback.test", None, "test-e2e").unwrap(),
        ["app-test-e2e.test"]
    );
    let dir = TempDir::new().unwrap();
    inject_dev_data_dir(dir.path(), &mut env, "test-e2e").unwrap();
    assert_eq!(
        Path::new(&env["TAKO_DATA_DIR"]),
        dir.path().join(".tako/data/envs/test-e2e/app")
    );
    assert!(!dir.path().join(".tako/data/app").exists());
}

#[test]
fn local_environment_validation_rejects_typos_and_unsafe_paths() {
    let cfg = TakoToml::parse("[vars.test-e2e]\nVALUE = \"test\"").unwrap();
    assert!(shared::validate_dev_environment(&cfg, "development").is_ok());
    assert!(shared::validate_dev_environment(&cfg, "test-e2e").is_ok());
    assert!(shared::validate_dev_environment(&cfg, "typo").is_err());
    assert!(shared::validate_dev_environment(&cfg, "../test").is_err());
}

#[test]
fn registration_keys_scope_environments_and_canonicalize_config() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("app.toml");
    std::fs::write(&config, "").unwrap();
    let canonical = config.canonicalize().unwrap();
    assert_eq!(
        dev_config_key(&config, "development"),
        canonical.display().to_string()
    );
    assert_eq!(
        dev_config_key(&config, "test-e2e"),
        dev_config_key(&canonical, "test-e2e")
    );
    assert_ne!(
        dev_config_key(&config, "development"),
        dev_config_key(&config, "test-e2e")
    );
}

#[test]
fn inject_dev_data_dir_creates_nested_app_and_tako_dirs() {
    let temp = TempDir::new().unwrap();
    let mut env = std::collections::HashMap::new();

    inject_dev_data_dir(temp.path(), &mut env, "development").unwrap();

    assert_eq!(
        env.get("TAKO_DATA_DIR").map(String::as_str),
        Some(
            temp.path()
                .join(".tako/data/app")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(
        dev_runtime_data_root(temp.path(), "development")
            .join("app")
            .is_dir()
    );
    assert!(
        dev_runtime_data_root(temp.path(), "development")
            .join("tako")
            .is_dir()
    );
}

#[test]
fn inject_dev_secrets_uses_selected_config_without_default_tako_toml() {
    let temp = TempDir::new().unwrap();
    // The secrets lock rejects symlinked ancestors such as macOS `/var`.
    let project = temp.path().canonicalize().unwrap();
    let cfg = TakoToml::parse("[vars]\nGREETING = \"hi\"\n").unwrap();
    let mut env = std::collections::HashMap::new();

    let secrets = inject_dev_secrets(&project, &cfg, &mut env, "development").unwrap();

    assert!(!project.join("tako.toml").exists());
    assert!(secrets.is_empty());
}

#[test]
fn compute_dev_env_ignores_configured_env_and_derives_development() {
    let cfg = TakoToml::parse(
        r#"
[vars]
ENV = "custom"
"#,
    )
    .unwrap();

    let env = compute_dev_env(&cfg, "development");
    assert_eq!(env.get("ENV").map(String::as_str), Some("development"));
}

#[test]
fn compute_dev_env_passes_through_user_log_level_from_vars() {
    let cfg = TakoToml::parse(
        r#"
[vars.development]
LOG_LEVEL = "debug"
"#,
    )
    .unwrap();

    let env = compute_dev_env(&cfg, "development");
    assert_eq!(env.get("LOG_LEVEL").map(String::as_str), Some("debug"));
}

#[test]
fn inject_dev_allowed_hosts_exports_route_hosts_for_vite() {
    let mut env = std::collections::HashMap::new();
    let hosts = vec![
        "app.test".to_string(),
        "tunnel.example.com".to_string(),
        "tunnel.example.com/api".to_string(),
        "*.preview.example.com".to_string(),
    ];

    inject_dev_allowed_hosts(&hosts, &mut env);

    assert_eq!(
        env.get("TAKO_DEV_ALLOWED_HOSTS").map(String::as_str),
        Some("app.test,tunnel.example.com,.preview.example.com")
    );
}

use super::*;

#[test]
fn inject_dev_data_dir_creates_nested_app_and_tako_dirs() {
    let temp = TempDir::new().unwrap();
    let mut env = std::collections::HashMap::new();

    inject_dev_data_dir(temp.path(), &mut env).unwrap();

    assert_eq!(
        env.get("TAKO_DATA_DIR").map(String::as_str),
        Some(
            temp.path()
                .join(".tako/data/app")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(dev_runtime_data_root(temp.path()).join("app").is_dir());
    assert!(dev_runtime_data_root(temp.path()).join("tako").is_dir());
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

    let env = compute_dev_env(&cfg);
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

    let env = compute_dev_env(&cfg);
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

use super::super::*;
use std::fs;

// ==================== Helper Method Tests ====================

#[test]
fn test_get_routes_single() {
    let toml = r#"
[envs.production]
route = "api.example.com"
"#;
    let config = Config::parse(toml).unwrap();
    let routes = config.get_routes("production").unwrap();
    assert_eq!(routes, vec!["api.example.com"]);
}

#[test]
fn test_get_routes_multiple() {
    let toml = r#"
[envs.production]
routes = ["api.example.com", "www.example.com"]
"#;
    let config = Config::parse(toml).unwrap();
    let routes = config.get_routes("production").unwrap();
    assert_eq!(routes, vec!["api.example.com", "www.example.com"]);
}

#[test]
fn test_get_routes_nonexistent_env() {
    let config = Config::default();
    assert!(config.get_routes("production").is_none());
}

#[test]
fn test_load_from_dir_requires_tako_toml() {
    let temp = tempfile::TempDir::new().unwrap();
    let err = Config::load_from_dir(temp.path()).unwrap_err();
    assert!(err.to_string().contains("tako.toml"));
}

#[test]
fn test_load_from_dir_allows_missing_name() {
    let temp = tempfile::TempDir::new().unwrap();
    fs::write(
        temp.path().join("tako.toml"),
        r#"
[envs.production]
route = "prod.example.com"
"#,
    )
    .unwrap();

    let config = Config::load_from_dir(temp.path()).unwrap();
    assert!(config.name.is_none());
    assert_eq!(
        config
            .get_routes("production")
            .expect("production routes should exist"),
        vec!["prod.example.com".to_string()]
    );
}

#[test]
fn test_get_environment_names() {
    let toml = r#"
[envs.production]
route = "prod.example.com"

[envs.staging]
route = "staging.example.com"
"#;
    let config = Config::parse(toml).unwrap();
    let mut names = config.get_environment_names();
    names.sort();
    assert_eq!(names, vec!["production", "staging"]);
}

#[test]
fn test_deployable_env_names_excludes_development() {
    let toml = r#"
[envs.development]
route = "localhost"

[envs.production]
route = "prod.example.com"

[envs.staging]
route = "staging.example.com"
"#;
    let config = Config::parse(toml).unwrap();
    let mut names: Vec<&str> = config.deployable_env_names().collect();
    names.sort_unstable();
    assert_eq!(names, vec!["production", "staging"]);
}

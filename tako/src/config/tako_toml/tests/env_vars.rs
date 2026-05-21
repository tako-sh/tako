use super::super::*;

// ==================== Per-Environment Vars Tests ====================

#[test]
fn test_parse_per_env_vars() {
    let toml = r#"
[vars]
API_URL = "https://api.example.com"

[vars.production]
DATABASE_URL = "postgres://prod"

[vars.staging]
DATABASE_URL = "postgres://staging"
"#;
    let config = Config::parse(toml).unwrap();

    // Global var
    assert_eq!(
        config.vars.get("API_URL"),
        Some(&"https://api.example.com".to_string())
    );

    // Per-env vars
    let prod_vars = config.vars_per_env.get("production").unwrap();
    assert_eq!(
        prod_vars.get("DATABASE_URL"),
        Some(&"postgres://prod".to_string())
    );

    let staging_vars = config.vars_per_env.get("staging").unwrap();
    assert_eq!(
        staging_vars.get("DATABASE_URL"),
        Some(&"postgres://staging".to_string())
    );
}

#[test]
fn test_get_merged_vars() {
    let toml = r#"
[vars]
API_URL = "https://api.example.com"

[vars.production]
DATABASE_URL = "postgres://prod"
"#;
    let config = Config::parse(toml).unwrap();

    let merged = config.get_merged_vars("production");
    assert_eq!(
        merged.get("API_URL"),
        Some(&"https://api.example.com".to_string())
    );
    assert_eq!(
        merged.get("DATABASE_URL"),
        Some(&"postgres://prod".to_string())
    );
}

#[test]
fn test_get_merged_vars_ignores_reserved_env_variable() {
    let toml = r#"
[vars]
ENV = "custom-global"
API_URL = "https://api.example.com"

[vars.production]
ENV = "custom-production"
DATABASE_URL = "postgres://prod"
"#;
    let config = Config::parse(toml).unwrap();

    let merged = config.get_merged_vars("production");
    assert!(!merged.contains_key("ENV"));
    assert_eq!(
        merged.get("API_URL"),
        Some(&"https://api.example.com".to_string())
    );
    assert_eq!(
        merged.get("DATABASE_URL"),
        Some(&"postgres://prod".to_string())
    );
}

#[test]
fn test_get_merged_vars_nonexistent_env() {
    let toml = r#"
[vars]
API_URL = "https://api.example.com"
"#;
    let config = Config::parse(toml).unwrap();

    let merged = config.get_merged_vars("nonexistent");
    assert_eq!(
        merged.get("API_URL"),
        Some(&"https://api.example.com".to_string())
    );
    assert_eq!(merged.len(), 1);
}

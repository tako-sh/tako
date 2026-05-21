use super::super::*;

#[test]
fn test_parse_global_vars() {
    let toml = r#"
[vars]
API_URL = "https://api.example.com"
DEBUG = "1"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(
        config.vars.get("API_URL"),
        Some(&"https://api.example.com".to_string())
    );
    assert_eq!(config.vars.get("DEBUG"), Some(&"1".to_string()));
}

#[test]
fn test_parse_single_route() {
    let toml = r#"
[envs.production]
route = "api.example.com"
"#;
    let config = Config::parse(toml).unwrap();
    let env = config.envs.get("production").unwrap();
    assert_eq!(env.route, Some("api.example.com".to_string()));
    assert_eq!(env.routes, None);
}

#[test]
fn test_parse_env_without_routes_is_rejected() {
    let toml = r#"
[envs.production]
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(
        err.to_string()
            .contains("must define either 'route' or 'routes'")
    );
}

#[test]
fn test_parse_development_env_without_routes_is_allowed() {
    let toml = r#"
[envs.development]
"#;
    let config = Config::parse(toml).unwrap();
    let env = config.envs.get("development").unwrap();
    assert_eq!(env.route, None);
    assert_eq!(env.routes, None);
}

#[test]
fn test_parse_env_with_empty_routes_is_rejected() {
    let toml = r#"
[envs.production]
routes = []
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("routes"));
}

#[test]
fn test_parse_development_env_with_empty_routes_is_allowed() {
    let toml = r#"
[envs.development]
routes = []
"#;
    let config = Config::parse(toml).unwrap();
    let env = config.envs.get("development").unwrap();
    assert_eq!(env.route, None);
    assert_eq!(env.routes, Some(Vec::new()));
}

#[test]
fn test_parse_multiple_routes() {
    let toml = r#"
[envs.production]
routes = ["api.example.com", "*.api.example.com", "example.com/api/*"]
"#;
    let config = Config::parse(toml).unwrap();
    let env = config.envs.get("production").unwrap();
    assert_eq!(env.route, None);
    assert_eq!(
        env.routes,
        Some(vec![
            "api.example.com".to_string(),
            "*.api.example.com".to_string(),
            "example.com/api/*".to_string(),
        ])
    );
}

#[test]
fn test_parse_env_rejects_additional_keys() {
    let toml = r#"
[envs.production]
route = "api.example.com"
replicas = 3
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("unknown field"));
}

#[test]
fn test_parse_env_servers_and_idle_timeout() {
    let toml = r#"
[envs.production]
route = "api.example.com"
servers = ["la-prod", "nyc-prod"]
idle_timeout = 600
"#;
    let config = Config::parse(toml).unwrap();
    let env = config.envs.get("production").unwrap();
    assert_eq!(
        env.servers,
        vec!["la-prod".to_string(), "nyc-prod".to_string()]
    );
    assert_eq!(env.idle_timeout, 600);
}

#[test]
fn test_default_env_idle_timeout_is_five_minutes() {
    let config = Config::default();
    assert_eq!(config.get_idle_timeout("production"), 300);
}

#[test]
fn test_parse_complete_config() {
    let toml = r#"
name = "my-api"
main = "server/index.mjs"
preset = "bun"
assets = ["public", ".output/public"]

[build]
run = "bun run build"
include = ["dist/**"]
exclude = ["**/*.map"]

[vars]
API_BASE_URL = "https://api.example.com"

[envs.production]
route = "api.example.com"
servers = ["prod-1"]

[envs.staging]
routes = ["staging.example.com", "*.staging.example.com"]
"#;
    let config = Config::parse(toml).unwrap();

    assert_eq!(config.name, Some("my-api".to_string()));
    assert_eq!(config.main, Some("server/index.mjs".to_string()));
    assert_eq!(config.preset, Some("bun".to_string()));
    assert_eq!(config.build.run, Some("bun run build".to_string()));
    assert_eq!(config.build.include, vec!["dist/**".to_string()]);
    assert_eq!(config.build.exclude, vec!["**/*.map".to_string()]);
    assert_eq!(
        config.assets,
        vec!["public".to_string(), ".output/public".to_string()]
    );
    assert_eq!(
        config.vars.get("API_BASE_URL"),
        Some(&"https://api.example.com".to_string())
    );

    let prod = config.envs.get("production").unwrap();
    assert_eq!(prod.route, Some("api.example.com".to_string()));

    let staging = config.envs.get("staging").unwrap();
    assert_eq!(staging.routes.as_ref().unwrap().len(), 2);
    let prod = config.envs.get("production").unwrap();
    assert_eq!(prod.servers, vec!["prod-1".to_string()]);
}

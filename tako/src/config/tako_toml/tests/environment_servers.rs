use super::super::*;

// ==================== Environment Server Mapping Tests ====================

#[test]
fn test_get_servers_for_env() {
    let toml = r#"
[envs.production]
route = "api.example.com"
servers = ["la-prod", "nyc-prod"]

[envs.staging]
route = "staging.example.com"
servers = ["staging-server"]
"#;
    let config = Config::parse(toml).unwrap();

    let prod_servers = config.get_servers_for_env("production");
    assert_eq!(prod_servers.len(), 2);
    assert!(prod_servers.contains(&"la-prod"));
    assert!(prod_servers.contains(&"nyc-prod"));

    let staging_servers = config.get_servers_for_env("staging");
    assert_eq!(staging_servers.len(), 1);
    assert!(staging_servers.contains(&"staging-server"));

    let dev_servers = config.get_servers_for_env("development");
    assert!(dev_servers.is_empty());
}

#[test]
fn test_get_idle_timeout() {
    let toml = r#"
[envs.production]
route = "api.example.com"
idle_timeout = 300

[envs.staging]
route = "staging.example.com"
idle_timeout = 600
"#;
    let config = Config::parse(toml).unwrap();

    assert_eq!(config.get_idle_timeout("production"), 300);
    assert_eq!(config.get_idle_timeout("staging"), 600);
    assert_eq!(config.get_idle_timeout("unknown"), 300);
}

#[test]
fn test_duplicate_non_development_server_membership_is_allowed() {
    let toml = r#"
[envs.production]
route = "api.example.com"
servers = ["shared"]

[envs.staging]
route = "staging.example.com"
servers = ["shared"]
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.get_servers_for_env("production"), vec!["shared"]);
    assert_eq!(config.get_servers_for_env("staging"), vec!["shared"]);
}

#[test]
fn test_duplicate_server_membership_with_development_is_allowed() {
    let toml = r#"
[envs.production]
route = "api.example.com"
servers = ["shared"]

[envs.development]
servers = ["shared"]
"#;
    assert!(Config::parse(toml).is_ok());
}

#[test]
fn test_env_servers_reject_invalid_server_name() {
    let toml = r#"
[envs.production]
route = "api.example.com"
servers = ["INVALID_NAME"]
"#;
    assert!(Config::parse(toml).is_err());
}

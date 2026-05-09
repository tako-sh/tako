use super::*;
use crate::config::EnvConfig;
use std::collections::HashMap;

fn config_with(top: Option<&str>, env_release: Option<Option<&str>>) -> TakoToml {
    let mut envs = HashMap::new();
    envs.insert(
        "production".to_string(),
        EnvConfig {
            route: Some("api.example.com".into()),
            servers: vec!["la".into()],
            release: env_release.map(|opt| opt.unwrap_or("").to_string()),
            ..Default::default()
        },
    );
    TakoToml {
        release: top.map(String::from),
        envs,
        ..Default::default()
    }
}

#[test]
fn env_release_overrides_top_level() {
    let cfg = config_with(Some("bun migrate"), Some(Some("bun migrate:prod")));
    assert_eq!(
        resolve_release_command(&cfg, "production"),
        Some("bun migrate:prod".to_string())
    );
}

#[test]
fn env_release_falls_back_to_top_level() {
    let cfg = config_with(Some("bun migrate"), None);
    assert_eq!(
        resolve_release_command(&cfg, "production"),
        Some("bun migrate".to_string())
    );
}

#[test]
fn empty_env_release_clears_top_level() {
    let cfg = config_with(Some("bun migrate"), Some(Some("")));
    assert_eq!(resolve_release_command(&cfg, "production"), None);
}

#[test]
fn no_release_returns_none() {
    let cfg = config_with(None, None);
    assert_eq!(resolve_release_command(&cfg, "production"), None);
}

#[test]
fn missing_env_falls_back_to_top_level() {
    let cfg = config_with(Some("bun migrate"), None);
    assert_eq!(
        resolve_release_command(&cfg, "staging"),
        Some("bun migrate".to_string())
    );
}

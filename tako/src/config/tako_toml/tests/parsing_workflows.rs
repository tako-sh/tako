use super::super::*;

// ==================== Parsing Tests ====================

#[test]
fn test_parse_empty_file() {
    let config = Config::parse("").unwrap();
    assert_eq!(config, Config::default());
}

// ==================== [workflows] / [servers.*.workflows] Tests ====================

#[test]
fn test_parse_top_level_workflows_base() {
    let toml = r#"
name = "app"

[workflows]
workers = 3
concurrency = 20
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.workflows.base.workers, Some(3));
    assert_eq!(config.workflows.base.concurrency, Some(20));
    assert!(config.workflows.groups.is_empty());
    assert!(config.servers.per_server.is_empty());
}

#[test]
fn test_parse_top_level_named_workflow_group() {
    let toml = r#"
[workflows]
workers = 5
concurrency = 10

[workflows.email]
run = ["./worker", "email"]
workers = 2
"#;
    let config = Config::parse(toml).unwrap();
    let email = config.workflows.groups.get("email").unwrap();
    assert_eq!(
        email.run,
        Some(vec!["./worker".to_string(), "email".to_string()])
    );
    assert_eq!(email.workers, Some(2));
    assert_eq!(email.concurrency, None);

    let effective = config.workflows_for_server_worker("lax", Some("email"));
    assert_eq!(effective.workers, 2);
    assert_eq!(effective.concurrency, 10);
}

#[test]
fn test_parse_server_workflows_override() {
    let toml = r#"
name = "app"

[servers.lax.workflows]
workers = 2
"#;
    let config = Config::parse(toml).unwrap();
    let lax = config.servers.per_server.get("lax").unwrap();
    let wf = lax.workflows.as_ref().unwrap();
    assert_eq!(wf.base.workers, Some(2));
    assert_eq!(wf.base.concurrency, None);
}

#[test]
fn test_workflows_for_server_inherits_top_level_then_server_override() {
    let toml = r#"
[workflows]
workers = 1
concurrency = 5

[servers.lax.workflows]
workers = 4
"#;
    let config = Config::parse(toml).unwrap();
    let lax = config.workflows_for_server("lax");
    assert_eq!(lax.workers, 4);
    assert_eq!(lax.concurrency, 5);
}

#[test]
fn parses_images_config() {
    let toml = r#"
name = "app"

[images]
local_patterns = ["/images/**"]
remote_patterns = ["cdn.example.com/uploads/**", "https://assets.example.com/*"]
sizes = [320, 640]
qualities = [75, 90]
formats = ["avif", "webp"]
"#;

    let config = Config::parse(toml).unwrap();

    assert_eq!(
        config.images.local_patterns,
        Some(vec!["/images/**".to_string()])
    );
    assert_eq!(
        config.images.remote_patterns,
        vec![
            "cdn.example.com/uploads/**".to_string(),
            "https://assets.example.com/*".to_string()
        ]
    );
    assert_eq!(config.images.sizes, vec![320, 640]);
    assert_eq!(config.images.qualities, vec![75, 90]);
    assert_eq!(
        config.images.formats,
        vec![
            tako_images::OutputFormat::Avif,
            tako_images::OutputFormat::Webp
        ]
    );
}

#[test]
fn rejects_invalid_images_config() {
    let toml = r#"
name = "app"

[images]
remote_patterns = ["ftp://cdn.example.com/**"]
"#;

    let err = Config::parse(toml).unwrap_err();

    assert!(format!("{err}").contains("[images]"), "{err}");
}

#[test]
fn test_workflows_for_server_falls_back_to_top_level() {
    let toml = r#"
[workflows]
workers = 1
concurrency = 5

[servers.lax]
# no workflows override
"#;
    let config = Config::parse(toml).unwrap();
    let lax = config.workflows_for_server("lax");
    assert_eq!(lax.workers, 1);
    assert_eq!(lax.concurrency, 5);
}

#[test]
fn test_workflows_for_server_falls_back_to_zero_config() {
    let config = Config::parse("name = \"x\"").unwrap();
    let wf = config.workflows_for_server("any");
    assert_eq!(wf.workers, 0); // scale-to-zero default
    assert_eq!(wf.concurrency, 10);
}

#[test]
fn test_workflows_for_named_worker_inherits_all_layers() {
    let toml = r#"
[workflows]
workers = 5
concurrency = 10

[workflows.email]
workers = 2

[servers.lax.workflows]
concurrency = 20

[servers.lax.workflows.email]
workers = 4
"#;
    let config = Config::parse(toml).unwrap();
    let email = config.workflows_for_server_worker("lax", Some("email"));
    assert_eq!(email.workers, 4);
    assert_eq!(email.concurrency, 20);

    let default = config.workflows_for_server("lax");
    assert_eq!(default.workers, 5);
    assert_eq!(default.concurrency, 20);
}

#[test]
fn test_empty_workflows_sections_use_built_in_defaults() {
    let toml = r#"
[workflows]

[servers.lax.workflows]
"#;
    let config = Config::parse(toml).unwrap();
    let wf = config.workflows_for_server("lax");
    assert_eq!(wf.workers, 0);
    assert_eq!(wf.concurrency, 10);
}

#[test]
fn test_parse_server_with_unknown_field_errors() {
    let toml = r#"
[servers.lax]
unknown_field = 1
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("unknown"));
}

#[test]
fn test_parse_workflows_with_unknown_field_errors() {
    let toml = r#"
[workflows]
workers = 1
bogus = true
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("bogus"));
}

#[test]
fn test_parse_workflows_rejects_empty_run() {
    let toml = r#"
[workflows.video]
run = []
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("workflows.video.run"));
}

#[test]
fn test_parse_workflows_rejects_invalid_worker_group_name() {
    let toml = r#"
[workflows.Email]
workers = 1
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("Workflow worker group"));
}

#[test]
fn test_servers_workflows_reserved_default_is_rejected() {
    let toml = r#"
[servers.workflows]
workers = 1
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("[workflows]"));
}

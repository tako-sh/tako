use super::validation::{validate_app_name, validate_route_pattern, validate_server_name};
use super::*;
use std::fs;

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
workers = 2
"#;
    let config = Config::parse(toml).unwrap();
    let email = config.workflows.groups.get("email").unwrap();
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

#[test]
fn test_parse_top_level_metadata_fields() {
    let toml = r#"
name = "my-app"
main = "server/index.mjs"
preset = "bun"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.name, Some("my-app".to_string()));
    assert_eq!(config.main, Some("server/index.mjs".to_string()));
    assert_eq!(config.preset, Some("bun".to_string()));
}

#[test]
fn test_parse_dev_command() {
    let toml = r#"
dev = ["vite", "dev"]
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.dev, vec!["vite".to_string(), "dev".to_string()]);
}

#[test]
fn test_parse_environment_source_ip_mode() {
    let toml = r#"
name = "app"

[envs.production]
route = "example.com"
servers = ["prod"]
source_ip = "cloudflare-proxy"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(
        config.envs["production"].source_ip,
        Some(tako_core::SourceIpMode::CloudflareProxy)
    );
    assert_eq!(
        config.get_source_ip_mode("production"),
        tako_core::SourceIpMode::CloudflareProxy
    );
}

#[test]
fn test_parse_environment_trusted_proxy_source_ip_mode() {
    let toml = r#"
name = "app"

[envs.production]
route = "example.com"
servers = ["prod"]
source_ip = "trusted-proxy"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(
        config.envs["production"].source_ip,
        Some(tako_core::SourceIpMode::TrustedProxy)
    );
    assert_eq!(
        config.get_source_ip_mode("production"),
        tako_core::SourceIpMode::TrustedProxy
    );
}

#[test]
fn omitted_environment_source_ip_defaults_to_auto() {
    let toml = r#"
name = "app"

[envs.production]
route = "example.com"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(
        config.get_source_ip_mode("production"),
        tako_core::SourceIpMode::Auto
    );
}

#[test]
fn parse_unknown_environment_source_ip_mode_errors() {
    let toml = r#"
name = "app"

[envs.production]
route = "example.com"
source_ip = "unknown"
"#;

    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("unknown"));
}

#[test]
fn test_parse_build_arrays() {
    let toml = r#"
assets = ["public-assets", "shared/images"]

[build]
include = [".output/**", "dist/**"]
exclude = ["**/*.map"]
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(
        config.build.include,
        vec![".output/**".to_string(), "dist/**".to_string()]
    );
    assert_eq!(config.build.exclude, vec!["**/*.map".to_string()]);
    assert_eq!(
        config.assets,
        vec!["public-assets".to_string(), "shared/images".to_string()]
    );
    assert!(config.build_stages.is_empty());
}

#[test]
fn test_parse_build_stages() {
    let toml = r#"
[[build_stages]]
run = "bun run build"

[[build_stages]]
name = "frontend-assets"
cwd = "frontend"
install = "bun install"
run = "bun run build"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.build_stages.len(), 2);
    assert_eq!(config.build_stages[0].name, None);
    assert_eq!(config.build_stages[0].cwd, None);
    assert_eq!(config.build_stages[0].install, None);
    assert_eq!(config.build_stages[0].run, "bun run build");
    assert!(config.build_stages[0].exclude.is_empty());
    assert_eq!(
        config.build_stages[1],
        BuildStage {
            name: Some("frontend-assets".to_string()),
            cwd: Some("frontend".to_string()),
            install: Some("bun install".to_string()),
            run: "bun run build".to_string(),
            exclude: Vec::new(),
        }
    );
}

#[test]
fn test_parse_build_stages_with_exclude() {
    let toml = r#"
[[build_stages]]
name = "rust-service"
cwd = "rust-service"
run = "cargo build --release"
exclude = ["target/debug/**"]

[[build_stages]]
name = "frontend"
cwd = "apps/web"
install = "bun install"
run = "bun run build"
exclude = ["**/*.map", "node_modules/**"]
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.build_stages.len(), 2);
    assert_eq!(
        config.build_stages[0].exclude,
        vec!["target/debug/**".to_string()]
    );
    assert_eq!(
        config.build_stages[1].exclude,
        vec!["**/*.map".to_string(), "node_modules/**".to_string()]
    );
}

#[test]
fn test_build_stages_exclude_rejects_absolute_paths() {
    let toml = r#"
[[build_stages]]
run = "cargo build"
exclude = ["/tmp/out/**"]
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(
        err.to_string()
            .contains("build_stages[0].exclude entry '/tmp/out/**' must be relative")
    );
}

#[test]
fn test_build_stages_exclude_rejects_parent_traversal() {
    let toml = r#"
[[build_stages]]
run = "cargo build"
exclude = ["../secret/**"]
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(
        err.to_string()
            .contains("build_stages[0].exclude entry '../secret/**' must not contain '..'")
    );
}

#[test]
fn test_build_include_mutually_exclusive_with_stages() {
    let toml = r#"
[build]
include = ["dist/**"]

[[build_stages]]
run = "bun run build"
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("per-stage exclude"));
}

#[test]
fn test_build_exclude_mutually_exclusive_with_stages() {
    let toml = r#"
[build]
exclude = ["**/*.map"]

[[build_stages]]
run = "bun run build"
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("per-stage exclude"));
}

#[test]
fn test_parse_build_stages_requires_run() {
    let toml = r#"
[[build_stages]]
name = "frontend-assets"
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(
        err.to_string()
            .contains("'build_stages[0].run' is required")
    );
}

#[test]
fn test_parse_build_stages_rejects_empty_run() {
    let toml = r#"
[[build_stages]]
run = "   "
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(
        err.to_string()
            .contains("'build_stages[0].run' cannot be empty")
    );
}

#[test]
fn test_parse_build_stages_rejects_non_table_entries() {
    let toml = r#"
build_stages = ["bun run build"]
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(
        err.to_string()
            .contains("'build_stages[0]' must be a table")
    );
}

#[test]
fn test_parse_build_stages_rejects_unknown_keys() {
    let toml = r#"
[[build_stages]]
command = "bun run build"
run = "bun run build"
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(
        err.to_string()
            .contains("Unknown key 'build_stages[0].command'")
    );
}

#[test]
fn test_build_stages_mutually_exclusive_with_build_run() {
    let toml = r#"
[build]
run = "bun run build"

[[build_stages]]
run = "bun run other"
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("mutually exclusive"));
}

#[test]
fn test_parse_runtime() {
    let toml = r#"
runtime = "node"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.runtime, Some("node".to_string()));
    assert_eq!(config.runtime_version_pin, None);
}

#[test]
fn test_parse_runtime_with_version_pin() {
    let toml = r#"
runtime = "bun@1.2.3"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.runtime, Some("bun".to_string()));
    assert_eq!(config.runtime_version_pin, Some("1.2.3".to_string()));
}

#[test]
fn test_parse_runtime_version_pin_defaults_to_none() {
    let toml = r#"
runtime = "bun"
"#;
    let config = Config::parse(toml).unwrap();
    assert!(config.runtime_version_pin.is_none());
}

#[test]
fn test_parse_rejects_unknown_top_level_keys() {
    let top_level_adapter = r#"
adapter = "node"
"#;
    let err = Config::parse(top_level_adapter).unwrap_err();
    assert!(err.to_string().contains("Unknown key 'adapter'"));

    let top_level_dist = r#"
dist = ".tako/dist"
"#;
    let err = Config::parse(top_level_dist).unwrap_err();
    assert!(err.to_string().contains("Unknown key 'dist'"));

    // `servers` is now a valid top-level key (hosts `[servers.X.workflows]`).
    // Use a different unknown key to confirm rejection still happens.
    let top_level_broker = r#"
broker = "redis"
"#;
    let err = Config::parse(top_level_broker).unwrap_err();
    assert!(err.to_string().contains("Unknown key 'broker'"));
}

#[test]
fn test_parse_accepts_top_level_assets() {
    let toml = r#"
assets = ["dist/client"]
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.assets, vec!["dist/client".to_string()]);
}

#[test]
fn test_parse_accepts_top_level_preset() {
    let toml = r#"
preset = "tanstack-start"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.preset, Some("tanstack-start".to_string()));
}

#[test]
fn test_parse_rejects_unknown_build_keys() {
    let build_adapter = r#"
[build]
adapter = "bun"
"#;
    let err = Config::parse(build_adapter).unwrap_err();
    assert!(err.to_string().contains("Unknown key 'build.adapter'"));

    // preset is now top-level, not under [build]
    let build_preset = r#"
[build]
preset = "bun"
"#;
    let err = Config::parse(build_preset).unwrap_err();
    assert!(err.to_string().contains("Unknown key 'build.preset'"));
}

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

// ==================== Validation Tests ====================

#[test]
fn test_validate_app_name_valid() {
    assert!(validate_app_name("my-app").is_ok());
    assert!(validate_app_name("api").is_ok());
    assert!(validate_app_name("my-app-123").is_ok());
    assert!(validate_app_name("a").is_ok());
}

#[test]
fn test_validate_app_name_empty() {
    assert!(validate_app_name("").is_err());
}

#[test]
fn test_validate_app_name_too_long() {
    let long_name = "a".repeat(64);
    assert!(validate_app_name(&long_name).is_err());
}

#[test]
fn test_validate_app_name_must_start_lowercase() {
    assert!(validate_app_name("My-app").is_err());
    assert!(validate_app_name("1app").is_err());
    assert!(validate_app_name("-app").is_err());
}

#[test]
fn test_validate_app_name_invalid_chars() {
    assert!(validate_app_name("my_app").is_err());
    assert!(validate_app_name("my.app").is_err());
    assert!(validate_app_name("my app").is_err());
    assert!(validate_app_name("MY-APP").is_err());
}

#[test]
fn test_validate_app_name_cannot_end_with_hyphen() {
    assert!(validate_app_name("my-app-").is_err());
}

#[test]
fn test_validate_route_pattern_valid() {
    assert!(validate_route_pattern("api.example.com").is_ok());
    assert!(validate_route_pattern("*.example.com").is_ok());
    assert!(validate_route_pattern("example.com/api/*").is_ok());
    assert!(validate_route_pattern("*.example.com/admin/*").is_ok());
}

#[test]
fn test_validate_route_pattern_empty() {
    assert!(validate_route_pattern("").is_err());
}

#[test]
fn test_validate_route_pattern_invalid_wildcard() {
    assert!(validate_route_pattern("api*.example.com").is_err());
    assert!(validate_route_pattern("example.com/api*").is_err());
}

#[test]
fn test_validate_route_pattern_invalid_chars() {
    assert!(validate_route_pattern("api@example.com").is_err());
    assert!(validate_route_pattern("api example.com").is_err());
}

#[test]
fn test_cannot_have_both_route_and_routes() {
    let toml = r#"
[envs.production]
route = "api.example.com"
routes = ["staging.example.com"]
"#;
    assert!(Config::parse(toml).is_err());
}

#[test]
fn test_validate_idle_timeout_cannot_be_zero() {
    let toml = r#"
[envs.production]
route = "api.example.com"
idle_timeout = 0
"#;
    assert!(Config::parse(toml).is_err());
}

#[test]
fn test_validate_assets_rejects_absolute_path() {
    let toml = r#"
assets = ["/tmp/assets"]
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(
        err.to_string()
            .contains("assets entry '/tmp/assets' must be relative to project root")
    );
}

#[test]
fn test_validate_assets_rejects_parent_directory_reference() {
    let toml = r#"
assets = ["../shared-assets"]
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(
        err.to_string()
            .contains("assets entry '../shared-assets' must not contain '..'")
    );
}

#[test]
fn test_validate_build_globs_reject_invalid_paths() {
    let absolute = r#"
[build]
include = ["/tmp/out/**"]
"#;
    let err = Config::parse(absolute).unwrap_err();
    assert!(
        err.to_string()
            .contains("build.include entry '/tmp/out/**' must be relative to project root")
    );

    let parent = r#"
[build]
exclude = ["../secret/**"]
"#;
    let err = Config::parse(parent).unwrap_err();
    assert!(
        err.to_string()
            .contains("build.exclude entry '../secret/**' must not contain '..'")
    );
}

#[test]
fn test_validate_build_stage_cwd_rejects_absolute_paths() {
    let absolute = r#"
[[build_stages]]
cwd = "/tmp"
run = "bun run build"
"#;
    let err = Config::parse(absolute).unwrap_err();
    assert!(
        err.to_string()
            .contains("'build_stages[0].cwd' must be relative")
    );
}

#[test]
fn test_validate_build_stage_cwd_allows_parent_within_root() {
    // cwd = "packages/../packages/ui" stays within root
    let toml = r#"
[[build_stages]]
cwd = "packages/../packages/ui"
run = "bun run build"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(
        config.build_stages[0].cwd,
        Some("packages/../packages/ui".to_string())
    );
}

#[test]
fn test_validate_build_stage_cwd_allows_parent_traversal() {
    // Parse-time validation allows ".." — escape check happens at deploy time
    // when the workspace root is known.
    let toml = r#"
[[build_stages]]
cwd = "../../sdk/javascript"
run = "bun run build"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(
        config.build_stages[0].cwd,
        Some("../../sdk/javascript".to_string())
    );
}

#[test]
fn test_validate_runtime_rejects_empty_and_unknown_values() {
    let empty = r#"
runtime = ""
"#;
    let err = Config::parse(empty).unwrap_err();
    assert!(err.to_string().contains("runtime cannot be empty"));

    let unknown = r#"
runtime = "python"
"#;
    let err = Config::parse(unknown).unwrap_err();
    assert!(
        err.to_string()
            .contains("runtime must be one of: bun, node, go")
    );

    let empty_version = r#"
runtime = "bun@"
"#;
    let err = Config::parse(empty_version).unwrap_err();
    assert!(err.to_string().contains("runtime version cannot be empty"));
}

#[test]
fn test_validate_preset_rejects_namespaced_alias_in_tako_toml() {
    let raw = r#"
preset = "js/tanstack-start"
"#;
    let err = Config::parse(raw).unwrap_err();
    assert!(
        err.to_string()
            .contains("preset must not include runtime namespace")
    );
}

#[test]
fn test_validate_preset_rejects_github_reference() {
    let raw = r#"
preset = "github:owner/repo/presets/custom.toml"
"#;
    let err = Config::parse(raw).unwrap_err();
    assert!(
        err.to_string()
            .contains("github preset references are not supported")
    );
}

#[test]
fn test_validate_preset_rejects_colon_references() {
    let raw = r#"
preset = "custom:tanstack-start"
"#;
    let err = Config::parse(raw).unwrap_err();
    assert!(err.to_string().contains("':' references are not supported"));
}

#[test]
fn test_parse_rejects_non_table_build_property() {
    let toml = r#"
build = "bun run build"
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("'build' must be a table"));
}

#[test]
fn test_validate_main_rejects_empty_value() {
    let toml = r#"
main = "   "
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("main cannot be empty"));
}

#[test]
fn test_parse_app_root() {
    let toml = r#"
app_root = "app/server"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.app_root.as_deref(), Some("app/server"));
    assert_eq!(config.js_app_root(), "app/server");
}

#[test]
fn test_js_app_root_defaults_to_src() {
    let config = Config::default();
    assert_eq!(config.js_app_root(), "src");
}

#[test]
fn test_validate_app_root_rejects_empty_absolute_and_parent_paths() {
    let empty = r#"
app_root = ""
"#;
    let err = Config::parse(empty).unwrap_err();
    assert!(err.to_string().contains("'app_root' cannot be empty"));

    let absolute = r#"
app_root = "/tmp/app"
"#;
    let err = Config::parse(absolute).unwrap_err();
    assert!(
        err.to_string()
            .contains("'app_root' must be a relative path")
    );

    let parent = r#"
app_root = "../app"
"#;
    let err = Config::parse(parent).unwrap_err();
    assert!(err.to_string().contains("'app_root' must not contain '..'"));
}

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

// ==================== Error Handling Tests ====================

#[test]
fn test_invalid_toml_syntax() {
    let toml = r#"
[tako
name = "broken"
"#;
    assert!(Config::parse(toml).is_err());
}

#[test]
fn test_wrong_type() {
    let toml = r#"
name = 123
"#;
    assert!(Config::parse(toml).is_err());
}

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

// ==================== Server Name Validation Tests ====================

#[test]
fn test_validate_server_name_valid() {
    assert!(validate_server_name("la").is_ok());
    assert!(validate_server_name("prod-server").is_ok());
    assert!(validate_server_name("server1").is_ok());
    assert!(validate_server_name("my-prod-server-1").is_ok());
}

#[test]
fn test_validate_server_name_empty() {
    assert!(validate_server_name("").is_err());
}

#[test]
fn test_validate_server_name_too_long() {
    let long_name = "a".repeat(64);
    assert!(validate_server_name(&long_name).is_err());
}

#[test]
fn test_validate_server_name_invalid_start() {
    assert!(validate_server_name("1server").is_err());
    assert!(validate_server_name("-server").is_err());
    assert!(validate_server_name("Server").is_err());
}

#[test]
fn test_validate_server_name_invalid_chars() {
    assert!(validate_server_name("my_server").is_err());
    assert!(validate_server_name("my.server").is_err());
    assert!(validate_server_name("MY-SERVER").is_err());
}

// ==================== build.cwd Tests ====================

#[test]
fn test_parse_build_cwd() {
    let toml = r#"
[build]
cwd = "."
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.build.cwd, Some(".".to_string()));
}

#[test]
fn test_build_cwd_accepts_subdirectory() {
    let toml = r#"
[build]
cwd = "packages/web"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.build.cwd, Some("packages/web".to_string()));
}

#[test]
fn test_build_cwd_rejects_empty() {
    let toml = r#"
[build]
cwd = ""
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("'build.cwd' cannot be empty"));
}

#[test]
fn test_build_cwd_rejects_absolute_path() {
    let toml = r#"
[build]
cwd = "/tmp/build"
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(
        err.to_string()
            .contains("'build.cwd' must be a relative path")
    );
}

#[test]
fn test_build_cwd_rejects_parent_dir() {
    let toml = r#"
[build]
cwd = "../parent"
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(
        err.to_string()
            .contains("'build.cwd' must not contain '..'")
    );
}

#[test]
fn test_parse_build_with_run_and_install() {
    let toml = r#"
[build]
run = "vinxi build"
install = "bun install"
cwd = "."
include = ["dist/**"]
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.build.run, Some("vinxi build".to_string()));
    assert_eq!(config.build.install, Some("bun install".to_string()));
    assert_eq!(config.build.cwd, Some(".".to_string()));
    assert_eq!(config.build.include, vec!["dist/**".to_string()]);
}

#[test]
fn parses_top_level_release() {
    let toml = r#"
name = "my-app"
release = "bun run db:migrate"
"#;
    let config = Config::parse(toml).unwrap();
    assert_eq!(config.release.as_deref(), Some("bun run db:migrate"));
}

#[test]
fn release_is_none_when_unset() {
    let config = Config::parse(r#"name = "my-app""#).unwrap();
    assert!(config.release.is_none());
}

#[test]
fn parses_per_env_release_override() {
    let toml = r#"
name = "my-app"
release = "bun run db:migrate"

[envs.production]
route = "api.example.com"
servers = ["la"]
release = "bun run db:migrate:prod"

[envs.staging]
route = "staging.example.com"
servers = ["staging"]
"#;
    let config = Config::parse(toml).unwrap();
    let prod = config.envs.get("production").unwrap();
    assert_eq!(prod.release.as_deref(), Some("bun run db:migrate:prod"));
    let staging = config.envs.get("staging").unwrap();
    assert!(staging.release.is_none());
}

#[test]
fn empty_release_string_is_preserved() {
    // An empty per-env release explicitly blanks the inherited top-level value.
    let toml = r#"
release = "bun run db:migrate"

[envs.production]
route = "api.example.com"
servers = ["la"]
release = ""
"#;
    let config = Config::parse(toml).unwrap();
    let prod = config.envs.get("production").unwrap();
    assert_eq!(prod.release.as_deref(), Some(""));
}

#[test]
fn rejects_unknown_key_release_command() {
    // Sanity: a typo should still fail (deny_unknown_fields stays in effect).
    let toml = r#"
[envs.production]
route = "api.example.com"
servers = ["la"]
release_command = "bun run db:migrate"
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(format!("{err}").contains("release_command"), "{err}");
}

#[test]
fn parses_storage_resources_and_env_bindings() {
    let config = Config::parse(
        r#"
name = "demo"

[storages.prod_uploads]
provider = "s3"
bucket = "demo-prod-uploads"
endpoint = "https://s3.example.com"
region = "us-east-1"
force_path_style = true
public_base_url = "https://cdn.example.com/uploads"

[envs.production]
route = "demo.example.com"
storages = { uploads = "prod_uploads", cache = "local" }
"#,
    )
    .unwrap();

    let prod = config.envs.get("production").unwrap();
    assert_eq!(
        prod.storages.get("uploads").map(String::as_str),
        Some("prod_uploads")
    );
    assert_eq!(
        prod.storages.get("cache").map(String::as_str),
        Some("local")
    );

    let uploads = config.storages.get("prod_uploads").unwrap();
    assert_eq!(uploads.provider, tako_core::StorageProvider::S3);
    assert_eq!(uploads.bucket.as_deref(), Some("demo-prod-uploads"));
    assert!(uploads.force_path_style);
    assert_eq!(
        uploads.public_base_url.as_deref(),
        Some("https://cdn.example.com/uploads")
    );
}

#[test]
fn non_development_storage_bindings_must_reference_configured_resources() {
    let err = Config::parse(
        r#"
name = "demo"

[envs.production]
route = "demo.example.com"
storages = { uploads = "prod_uploads" }
"#,
    )
    .unwrap_err();

    assert!(
        err.to_string().contains(
            "Environment 'production' storage 'uploads' references missing storage resource 'prod_uploads'"
        ),
        "{err}"
    );
}

#[test]
fn storage_resources_reject_local_provider() {
    let err = Config::parse(
        r#"
name = "demo"

[storages.cache]
provider = "local"

[envs.production]
route = "demo.example.com"
storages = { cache = "cache" }
"#,
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("Storage resource 'cache' cannot set provider 'local'"),
        "{err}"
    );
}

#[test]
fn storage_resources_reject_builtin_local_resource_table() {
    let err = Config::parse(
        r#"
name = "demo"

[storages.local]
provider = "s3"
bucket = "demo-prod-uploads"
endpoint = "https://s3.example.com"
region = "us-east-1"

[envs.production]
route = "demo.example.com"
storages = { uploads = "local" }
"#,
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("Storage resource 'local' is built in and cannot be declared"),
        "{err}"
    );
}

#[test]
fn non_development_storage_bindings_allow_implicit_local_resource() {
    let config = Config::parse(
        r#"
name = "demo"

[envs.production]
route = "demo.example.com"
storages = { uploads = "local" }
"#,
    )
    .unwrap();

    assert_eq!(
        config
            .envs
            .get("production")
            .unwrap()
            .storages
            .get("uploads")
            .map(String::as_str),
        Some("local")
    );
    assert!(!config.storages.contains_key("local"));
}

#[test]
fn development_storage_bindings_allow_implicit_local_resources() {
    let config = Config::parse(
        r#"
name = "demo"

[envs.development]
storages = { uploads = "uploads" }
"#,
    )
    .unwrap();

    assert_eq!(
        config
            .envs
            .get("development")
            .unwrap()
            .storages
            .get("uploads")
            .map(String::as_str),
        Some("uploads")
    );
    assert!(!config.storages.contains_key("uploads"));
}

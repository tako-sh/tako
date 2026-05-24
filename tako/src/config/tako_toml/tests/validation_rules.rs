use super::super::validation::{validate_app_name, validate_route_pattern};
use super::super::*;

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
fn test_parse_env_ssl_provider() {
    let toml = r#"
name = "app"

[envs.production]
route = "api.example.com"
ssl = "cloudflare"
"#;
    let config = Config::parse(toml).unwrap();

    assert_eq!(
        config.get_ssl_provider("production"),
        tako_core::SslProvider::Cloudflare
    );
}

#[test]
fn test_parse_env_ssl_provider_rejects_unknown_value() {
    let toml = r#"
name = "app"

[envs.production]
route = "api.example.com"
ssl = "self-signed"
"#;
    let err = Config::parse(toml).unwrap_err();
    assert!(err.to_string().contains("unknown variant"));
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

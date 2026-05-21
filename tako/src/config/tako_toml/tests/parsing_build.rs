use super::super::*;

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

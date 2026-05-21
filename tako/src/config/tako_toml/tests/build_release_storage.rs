use super::super::*;

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

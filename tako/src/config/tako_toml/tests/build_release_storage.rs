use super::super::*;

// ==================== build.cwd Tests ====================

#[test]
fn test_parse_build_cwd() {
    let toml = r#"
[build]
cwd = "."
"#;
    let config = TakoToml::parse(toml).unwrap();
    assert_eq!(config.build.cwd, Some(".".to_string()));
}

#[test]
fn test_build_cwd_accepts_subdirectory() {
    let toml = r#"
[build]
cwd = "packages/web"
"#;
    let config = TakoToml::parse(toml).unwrap();
    assert_eq!(config.build.cwd, Some("packages/web".to_string()));
}

#[test]
fn test_build_cwd_rejects_empty() {
    let toml = r#"
[build]
cwd = ""
"#;
    let err = TakoToml::parse(toml).unwrap_err();
    assert!(err.to_string().contains("'build.cwd' cannot be empty"));
}

#[test]
fn test_build_cwd_rejects_absolute_path() {
    let toml = r#"
[build]
cwd = "/tmp/build"
"#;
    let err = TakoToml::parse(toml).unwrap_err();
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
    let err = TakoToml::parse(toml).unwrap_err();
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
    let config = TakoToml::parse(toml).unwrap();
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
    let config = TakoToml::parse(toml).unwrap();
    assert_eq!(config.release.as_deref(), Some("bun run db:migrate"));
}

#[test]
fn release_is_none_when_unset() {
    let config = TakoToml::parse(r#"name = "my-app""#).unwrap();
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
    let config = TakoToml::parse(toml).unwrap();
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
    let config = TakoToml::parse(toml).unwrap();
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
    let err = TakoToml::parse(toml).unwrap_err();
    assert!(format!("{err}").contains("release_command"), "{err}");
}

#[test]
fn parses_storage_resources_and_env_bindings() {
    let config = TakoToml::parse(
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
fn channel_replay_database_is_optional_and_scoped_to_each_environment() {
    let config = TakoToml::parse(
        r#"
[envs.production]
route = "chat.example.com"
[envs.production.channels]
db = "chat-prod-replay"

[envs.staging]
route = "staging.example.com"
[envs.staging.channels]
db = "chat_stage2-"

[envs.development]
"#,
    )
    .unwrap();

    for (name, expected) in [
        ("production", Some("chat-prod-replay")),
        ("staging", Some("chat_stage2-")),
        ("development", None),
    ] {
        let env = &config.envs[name];
        assert_eq!(env.channels.as_ref().map(|c| c.db.as_str()), expected);
        assert!(env.databases.is_empty());
    }
}

#[test]
fn channel_replay_database_rejects_invalid_names_and_fields() {
    let table = "[envs.development.channels]";
    for name in ["", "Chat", "9chat", "chat.db", " chat ", &"a".repeat(64)] {
        let err = TakoToml::parse(&format!("{table}\ndb = '{name}'")).unwrap_err();
        assert!(err.to_string().contains("database name"), "{err}");
    }
    for (fields, expected) in [
        ("", "missing field `db`"),
        ("database = 'chat'", "unknown field `database`"),
    ] {
        let err = TakoToml::parse(&format!("{table}\n{fields}")).unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
    }
}

#[test]
fn parses_backup_storage_reference() {
    let config = TakoToml::parse(
        r#"
name = "demo"

[storages.r2]
provider = "s3"
bucket = "demo-backups"
endpoint = "https://account.r2.cloudflarestorage.com"
region = "auto"

[envs.production]
route = "demo.example.com"
backup = { storage = "r2" }
"#,
    )
    .unwrap();

    let prod = config.envs.get("production").unwrap();
    assert_eq!(
        prod.backup.as_ref().map(|backup| backup.storage.as_str()),
        Some("r2")
    );
}

#[test]
fn backup_database_lists_point_to_hosted_databases() {
    for paths in [
        r#"databases = ["app.db"]"#,
        r#"shared_databases = ["team/orders.db"]"#,
    ] {
        let err = TakoToml::parse(&format!(
            r#"
name = "demo"
[storages.backups]
provider = "s3"
bucket = "private"
endpoint = "https://s3.example.test"
region = "local"
[envs.production]
route = "demo.example.test"
backup = {{ storage = "backups", {paths} }}
"#
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("tako db import"), "{err}");
    }
}

#[test]
fn backup_storage_must_reference_configured_resource() {
    let err = TakoToml::parse(
        r#"
name = "demo"

[envs.production]
route = "demo.example.com"
backup = { storage = "r2" }
"#,
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("Environment 'production' backup references missing storage resource 'r2'"),
        "{err}"
    );
}

#[test]
fn backup_storage_rejects_names_with_whitespace() {
    let err = TakoToml::parse(
        r#"
name = "demo"

[storages.r2]
provider = "s3"
bucket = "demo-backups"
endpoint = "https://account.r2.cloudflarestorage.com"
region = "auto"

[envs.production]
route = "demo.example.com"
backup = { storage = " r2 " }
"#,
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("Storage name can only contain lowercase letters"),
        "{err}"
    );
}

#[test]
fn backup_storage_cannot_use_local_resource() {
    let err = TakoToml::parse(
        r#"
name = "demo"

[envs.production]
route = "demo.example.com"
backup = { storage = "local" }
"#,
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("Environment 'production' backup uses local storage"),
        "{err}"
    );
}

#[test]
fn backup_storage_resource_must_be_private() {
    let err = TakoToml::parse(
        r#"
name = "demo"

[storages.r2]
provider = "s3"
bucket = "demo-backups"
endpoint = "https://account.r2.cloudflarestorage.com"
region = "auto"
public_base_url = "https://cdn.example.com"

[envs.production]
route = "demo.example.com"
backup = { storage = "r2" }
"#,
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("Environment 'production' backup storage resource 'r2' must be private"),
        "{err}"
    );
}

#[test]
fn backup_rejects_unknown_fields() {
    let err = TakoToml::parse(
        r#"
name = "demo"

[storages.r2]
provider = "s3"
bucket = "demo-backups"
endpoint = "https://account.r2.cloudflarestorage.com"
region = "auto"

[envs.production]
route = "demo.example.com"
backup = { storage = "r2", retention = "30d" }
"#,
    )
    .unwrap_err();

    assert!(err.to_string().contains("retention"), "{err}");
}

#[test]
fn non_development_storage_bindings_must_reference_configured_resources() {
    let err = TakoToml::parse(
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
    let err = TakoToml::parse(
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
    let err = TakoToml::parse(
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
    let config = TakoToml::parse(
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
    let config = TakoToml::parse(
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

#[test]
fn plain_image_has_explicit_runtime_settings() {
    let config = TakoToml::parse(
        r#"
image = "docker.io/library/nginx:alpine"
[container_options]
sdk = false
port = 80
health_path = "/"
data_dir = "/var/cache/app"
secret_env = ["DATABASE_URL"]
user = { uid = 1000, gid = 1000 }
"#,
    )
    .unwrap();
    assert_eq!(config.container_options.port, 80);
    assert!(!config.container_options.sdk);
    assert_eq!(config.container_options.user.unwrap().uid, 1000);
}

#[test]
fn compose_validates_deployment_boundaries() {
    let base = r#"
[compose]
file = "compose.yaml"
[envs.production]
servers = ["one"]
"#;
    TakoToml::parse(base).unwrap();
    assert!(TakoToml::parse(&base.replace("[\"one\"]", "[\"one\", \"two\"]")).is_err());
    assert!(TakoToml::parse(&base.replace("compose.yaml", "../compose.yaml")).is_err());
    assert!(TakoToml::parse(&format!("container = \"Dockerfile\"\n{base}")).is_err());
    assert!(TakoToml::parse(&format!("main = \"app.js\"\n{base}")).is_err());
}

#[test]
fn container_runtime_rejects_invalid_paths_and_health() {
    for options in [
        "data_dir = \"/\"",
        "data_dir = \"/a/../b\"",
        "port = 0",
        "sdk = true\nhealth_path = \"/health\"",
        "secret_env = [\"TAKO_BOOTSTRAP_DATA\"]",
    ] {
        assert!(
            TakoToml::parse(&format!(
                "container = \"Dockerfile\"\n[container_options]\n{options}\n"
            ))
            .is_err(),
            "{options}"
        );
    }
}

#[test]
fn compose_loads_service_routes_and_local_app_defaults() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("tako.toml"),
        "name = 'shop'\n[compose]\nfile = 'compose.yaml'\n[envs.production]\nservers = ['prod']\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("compose.yaml"), "services:\n  web:\n    image: nginx\n    x-tako:\n      envs:\n        production:\n          route: example.com\n  api:\n    image: nginx\n    x-tako:\n      envs:\n        production:\n          route: api.example.com\n  private:\n    image: nginx\n    x-tako: {http: true}\n  redis:\n    image: redis\n").unwrap();
    let config = TakoToml::load_from_dir(dir.path()).unwrap();
    assert_eq!(
        config.get_routes("production").unwrap(),
        ["api.example.com", "example.com"]
    );
    let compose = config.compose.unwrap();
    assert_eq!(compose.services.len(), 3);
    assert!(!compose.services.contains_key("redis"));
    let dev = compose.for_local_environment("development", "shop-preview.tako", Some("shop.tako"));
    assert_eq!(
        dev.routes("development"),
        [
            "api.shop-preview.tako",
            "private.shop-preview.tako",
            "web.shop-preview.tako"
        ]
    );
    assert!(TakoToml::parse("[compose]\nfile='compose.yaml'\nport=3000").is_err());
    std::fs::write(
        dir.path().join("compose.yaml"),
        "services:\n  private:\n    image: nginx\n    x-tako: {}\n",
    )
    .unwrap();
    let error = TakoToml::load_from_dir(dir.path()).unwrap_err().to_string();
    assert!(error.contains("`http: true`"), "{error}");
    assert!(
        TakoToml::parse("[compose]\nfile='compose.yaml'\n[envs.production]\nroute='example.com'")
            .is_err()
    );
}

#[test]
fn kv_bindings_parse_and_share_names_with_databases() {
    let config = TakoToml::parse(
        "[envs.production]\nroute = \"shop.example.com\"\ndb = { main = \"main\" }\nkv = { auth = \"sessions\" }\n",
    )
    .unwrap();
    assert_eq!(config.envs["production"].kv["auth"], "sessions");

    let shared = TakoToml::parse(
        "[envs.production]\nroute = \"shop.example.com\"\ndb = { main = \"main\" }\nkv = { main = \"sessions\" }\n",
    )
    .unwrap_err()
    .to_string();
    assert!(
        shared.contains("binds 'main' in both db and kv"),
        "{shared}"
    );

    let invalid = TakoToml::parse(
        "[envs.production]\nroute = \"shop.example.com\"\nkv = { Auth = \"sessions\" }\n",
    )
    .unwrap_err()
    .to_string();
    assert!(
        invalid.contains("must start with a lowercase letter"),
        "{invalid}"
    );

    let replay = TakoToml::parse(
        "[envs.production]\nroute = \"shop.example.com\"\nkv = { auth = \"sessions\" }\nchannels.db = \"sessions\"\n",
    )
    .unwrap_err()
    .to_string();
    assert!(
        replay.contains("channel replay needs a SQL database"),
        "{replay}"
    );
}

#[test]
fn kv_stores_take_their_backup_storage_from_their_own_table_or_the_environment() {
    let storages = "[storages.kv_backups]\nprovider = \"s3\"\nbucket = \"b\"\nendpoint = \"https://s3.example.test\"\nregion = \"auto\"\n\n[storages.env_backups]\nprovider = \"s3\"\nbucket = \"e\"\nendpoint = \"https://s3.example.test\"\nregion = \"auto\"\n";
    let config = TakoToml::parse(&format!(
        "[envs.production]\nroute = \"shop.example.com\"\nkv = {{ auth = \"sessions\", limits = \"rate-limits\" }}\nbackup = {{ storage = \"env_backups\" }}\n\n[kv.sessions]\nbackup = \"kv_backups\"\n\n{storages}"
    ))
    .unwrap();
    assert_eq!(config.kv["sessions"].backup.as_deref(), Some("kv_backups"));
    assert_eq!(
        crate::commands::storage::database_backup_setting(&config, "production", "sessions"),
        Some(("kv_backups".into(), "[kv.sessions].backup".into()))
    );
    assert_eq!(
        crate::commands::storage::database_backup_setting(&config, "production", "rate-limits"),
        Some((
            "env_backups".into(),
            "[envs.production].backup.storage".into()
        ))
    );

    let missing = TakoToml::parse("[kv.sessions]\nbackup = \"nowhere\"\n")
        .unwrap_err()
        .to_string();
    assert!(
        missing.contains("[kv.sessions].backup references missing storage"),
        "{missing}"
    );
    let both = TakoToml::parse(&format!(
        "[db.sessions]\nbackup = \"kv_backups\"\n\n[kv.sessions]\nbackup = \"kv_backups\"\n\n{storages}"
    ))
    .unwrap_err()
    .to_string();
    assert!(
        both.contains("both [db.sessions] and [kv.sessions]"),
        "{both}"
    );
}

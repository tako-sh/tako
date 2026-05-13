use super::*;
use tempfile::TempDir;

// ==================== Parsing Tests ====================

#[test]
fn test_parse_empty_file() {
    let config = ServersToml::parse("").unwrap();
    assert!(config.servers.is_empty());
}

#[test]
fn test_parse_single_server() {
    let toml = r#"
[[servers]]
name = "la"
host = "1.2.3.4"
"#;
    let config = ServersToml::parse(toml).unwrap();
    assert_eq!(config.servers.len(), 1);

    let server = config.get("la").unwrap();
    assert_eq!(server.host, "1.2.3.4");
    assert_eq!(server.port, 22);
    assert_eq!(server.http_port, 80);
    assert_eq!(server.https_port, 443);
}

#[test]
fn test_parse_server_with_all_fields() {
    let toml = r#"
[[servers]]
name = "production"
host = "prod.example.com"
port = 2222
http_port = 8080
https_port = 8443
description = "Primary production server"
"#;
    let config = ServersToml::parse(toml).unwrap();
    let server = config.get("production").unwrap();

    assert_eq!(server.host, "prod.example.com");
    assert_eq!(server.port, 2222);
    assert_eq!(server.http_port, 8080);
    assert_eq!(server.https_port, 8443);
    assert_eq!(
        server.description.as_deref(),
        Some("Primary production server")
    );
}

#[test]
fn test_parse_multiple_servers() {
    let toml = r#"
[[servers]]
name = "la"
host = "1.2.3.4"

[[servers]]
name = "nyc"
host = "5.6.7.8"

[[servers]]
name = "kyoto"
host = "9.10.11.12"
port = 2222
"#;
    let config = ServersToml::parse(toml).unwrap();
    assert_eq!(config.servers.len(), 3);

    assert!(config.contains("la"));
    assert!(config.contains("nyc"));
    assert!(config.contains("kyoto"));

    assert_eq!(config.get("kyoto").unwrap().port, 2222);
}

#[test]
fn test_parse_server_entry_target_fields() {
    let toml = r#"
[[servers]]
name = "la"
host = "1.2.3.4"
arch = "x86_64"
libc = "glibc"
"#;
    let config = ServersToml::parse(toml).unwrap();
    let target = config.get_target("la").unwrap();
    assert_eq!(target.arch, "x86_64");
    assert_eq!(target.libc, "glibc");
}

#[test]
fn test_parse_rejects_partial_server_target_fields() {
    let toml = r#"
[[servers]]
name = "la"
host = "1.2.3.4"
arch = "x86_64"
"#;
    let err = ServersToml::parse(toml).unwrap_err();
    assert!(err.to_string().contains("both `arch` and `libc`"));
}

#[test]
fn test_parse_rejects_invalid_public_ports() {
    let toml = r#"
[[servers]]
name = "la"
host = "1.2.3.4"
http_port = 0
https_port = 8443
"#;
    let err = ServersToml::parse(toml).unwrap_err();
    assert!(err.to_string().contains("HTTP port"));

    let toml = r#"
[[servers]]
name = "nyc"
host = "5.6.7.8"
http_port = 8080
https_port = 8080
"#;
    let err = ServersToml::parse(toml).unwrap_err();
    assert!(err.to_string().contains("must differ"));
}

#[test]
fn test_target_normalization_accepts_common_aliases() {
    assert_eq!(
        ServerTarget::normalize_arch("amd64").as_deref(),
        Some("x86_64")
    );
    assert_eq!(
        ServerTarget::normalize_arch("arm64").as_deref(),
        Some("aarch64")
    );
    assert_eq!(
        ServerTarget::normalize_libc("GNU libc").as_deref(),
        Some("glibc")
    );
    assert_eq!(
        ServerTarget::normalize_libc("musl").as_deref(),
        Some("musl")
    );
}

#[test]
fn test_target_normalization_rejects_unknown_values() {
    assert!(ServerTarget::normalize_arch("sparc").is_none());
    assert!(ServerTarget::normalize_libc("uclibc").is_none());
}

#[test]
fn test_parse_ignores_unrelated_top_level_tables() {
    let toml = r#"
[dev]
port = 55555

[server_targets.ghost]
arch = "x86_64"
libc = "glibc"

[[servers]]
name = "la"
host = "1.2.3.4"
"#;
    let config = ServersToml::parse(toml).unwrap();
    assert_eq!(config.len(), 1);
    assert!(config.contains("la"));
    assert!(config.get_target("ghost").is_none());
}

#[test]
fn test_default_values() {
    let entry = ServerEntry::default();
    assert_eq!(entry.port, 22);
    assert_eq!(entry.http_port, 80);
    assert_eq!(entry.https_port, 443);
}

// ==================== Validation Tests ====================

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

#[test]
fn test_duplicate_server_names() {
    let toml = r#"
[[servers]]
name = "la"
host = "1.2.3.4"

[[servers]]
name = "la"
host = "5.6.7.8"
"#;
    let result = ServersToml::parse(toml);
    assert!(matches!(result, Err(ConfigError::DuplicateServerName(_))));
}

#[test]
fn test_duplicate_hosts() {
    let toml = r#"
[[servers]]
name = "la"
host = "1.2.3.4"

[[servers]]
name = "nyc"
host = "1.2.3.4"
"#;
    let result = ServersToml::parse(toml);
    assert!(matches!(result, Err(ConfigError::DuplicateServerHost(_))));
}

#[test]
fn test_missing_name_field() {
    let toml = r#"
[[servers]]
host = "1.2.3.4"
"#;
    let result = ServersToml::parse(toml);
    assert!(result.is_err());
}

#[test]
fn test_missing_host_field() {
    let toml = r#"
[[servers]]
name = "la"
"#;
    let result = ServersToml::parse(toml);
    assert!(result.is_err());
}

// ==================== CRUD Operation Tests ====================

#[test]
fn test_add_server() {
    let mut config = ServersToml::default();

    config
        .add(
            "la".to_string(),
            ServerEntry {
                host: "1.2.3.4".to_string(),
                port: 22,
                description: None,
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(config.len(), 1);
    assert!(config.contains("la"));
}

#[test]
fn test_add_duplicate_name_fails() {
    let mut config = ServersToml::default();

    config
        .add(
            "la".to_string(),
            ServerEntry {
                host: "1.2.3.4".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

    let result = config.add(
        "la".to_string(),
        ServerEntry {
            host: "5.6.7.8".to_string(),
            ..Default::default()
        },
    );

    assert!(matches!(result, Err(ConfigError::DuplicateServerName(_))));
}

#[test]
fn test_add_duplicate_host_fails() {
    let mut config = ServersToml::default();

    config
        .add(
            "la".to_string(),
            ServerEntry {
                host: "1.2.3.4".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

    let result = config.add(
        "nyc".to_string(),
        ServerEntry {
            host: "1.2.3.4".to_string(),
            ..Default::default()
        },
    );

    assert!(matches!(result, Err(ConfigError::DuplicateServerHost(_))));
}

#[test]
fn test_remove_server() {
    let mut config = ServersToml::default();

    config
        .add(
            "la".to_string(),
            ServerEntry {
                host: "1.2.3.4".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
    config
        .set_target(
            "la",
            ServerTarget {
                arch: "x86_64".to_string(),
                libc: "glibc".to_string(),
            },
        )
        .unwrap();

    let removed = config.remove("la").unwrap();
    assert_eq!(removed.host, "1.2.3.4");
    assert!(config.is_empty());
    assert!(config.get_target("la").is_none());
}

#[test]
fn test_set_target_normalizes_arch_and_libc_aliases() {
    let mut config = ServersToml::default();
    config
        .add(
            "la".to_string(),
            ServerEntry {
                host: "1.2.3.4".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

    config
        .set_target(
            "la",
            ServerTarget {
                arch: "amd64".to_string(),
                libc: "gnu libc".to_string(),
            },
        )
        .unwrap();

    let target = config.get_target("la").unwrap();
    assert_eq!(target.arch, "x86_64");
    assert_eq!(target.libc, "glibc");
    assert_eq!(target.label(), "linux-x86_64-glibc");
}

#[test]
fn test_set_target_rejects_unknown_metadata_values() {
    let mut config = ServersToml::default();
    config
        .add(
            "la".to_string(),
            ServerEntry {
                host: "1.2.3.4".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

    let err = config
        .set_target(
            "la",
            ServerTarget {
                arch: "sparc".to_string(),
                libc: "glibc".to_string(),
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("Invalid target metadata"));
}

#[test]
fn test_remove_nonexistent_fails() {
    let mut config = ServersToml::default();
    let result = config.remove("la");
    assert!(matches!(result, Err(ConfigError::ServerNotFound(_))));
}

#[test]
fn test_update_server() {
    let mut config = ServersToml::default();

    config
        .add(
            "la".to_string(),
            ServerEntry {
                host: "1.2.3.4".to_string(),
                port: 22,
                http_port: 80,
                https_port: 443,
                description: None,
            },
        )
        .unwrap();

    config
        .update(
            "la",
            ServerEntry {
                host: "5.6.7.8".to_string(),
                port: 2222,
                http_port: 8080,
                https_port: 8443,
                description: None,
            },
        )
        .unwrap();

    let server = config.get("la").unwrap();
    assert_eq!(server.host, "5.6.7.8");
    assert_eq!(server.port, 2222);
    assert_eq!(server.http_port, 8080);
    assert_eq!(server.https_port, 8443);
}

#[test]
fn test_find_by_host() {
    let mut config = ServersToml::default();

    config
        .add(
            "la".to_string(),
            ServerEntry {
                host: "1.2.3.4".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(config.find_by_host("1.2.3.4"), Some("la"));
    assert_eq!(config.find_by_host("5.6.7.8"), None);
}

// ==================== File I/O Tests ====================

#[test]
fn test_save_and_load() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("config.toml");

    let mut config = ServersToml::default();
    config
        .add(
            "la".to_string(),
            ServerEntry {
                host: "1.2.3.4".to_string(),
                port: 2222,
                http_port: 8080,
                https_port: 8443,
                description: Some("west coast".to_string()),
            },
        )
        .unwrap();
    config
        .add(
            "nyc".to_string(),
            ServerEntry {
                host: "5.6.7.8".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
    config
        .set_target(
            "la",
            ServerTarget {
                arch: "x86_64".to_string(),
                libc: "glibc".to_string(),
            },
        )
        .unwrap();

    config.save_to_file(&path).unwrap();
    let written = fs::read_to_string(&path).unwrap();
    assert!(written.contains("http_port = 8080"));
    assert!(written.contains("https_port = 8443"));
    assert!(written.contains("arch = \"x86_64\""));
    assert!(written.contains("libc = \"glibc\""));
    assert!(!written.contains("[server_targets."));

    let loaded = ServersToml::load_from_file(&path).unwrap();
    assert_eq!(loaded.len(), 2);

    let la = loaded.get("la").unwrap();
    assert_eq!(la.host, "1.2.3.4");
    assert_eq!(la.port, 2222);
    assert_eq!(la.http_port, 8080);
    assert_eq!(la.https_port, 8443);
    assert_eq!(la.description.as_deref(), Some("west coast"));
    let la_target = loaded.get_target("la").unwrap();
    assert_eq!(la_target.arch, "x86_64");
    assert_eq!(la_target.libc, "glibc");

    let nyc = loaded.get("nyc").unwrap();
    assert_eq!(nyc.host, "5.6.7.8");
    assert_eq!(nyc.port, 22); // default
    assert_eq!(nyc.http_port, 80);
    assert_eq!(nyc.https_port, 443);
    assert!(loaded.get_target("nyc").is_none());
}

#[test]
fn test_load_nonexistent_returns_default() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("nonexistent.toml");

    // load_from_file should fail for nonexistent
    assert!(ServersToml::load_from_file(&path).is_err());
}

#[test]
fn test_creates_parent_directory() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("subdir").join("config.toml");

    let mut config = ServersToml::default();
    config
        .add(
            "la".to_string(),
            ServerEntry {
                host: "1.2.3.4".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

    config.save_to_file(&path).unwrap();
    assert!(path.exists());
}

#[test]
fn test_save_preserves_dev_section() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("config.toml");
    fs::write(
        &path,
        r#"
[dev]
port = 61234
"#,
    )
    .unwrap();

    let mut config = ServersToml::default();
    config
        .add(
            "la".to_string(),
            ServerEntry {
                host: "1.2.3.4".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

    config.save_to_file(&path).unwrap();
    let written = fs::read_to_string(&path).unwrap();
    assert!(written.contains("[dev]"));
    assert!(written.contains("port = 61234"));
    assert!(written.contains("[[servers]]"));
}

#[test]
fn test_load_prefers_config_when_adjacent_servers_file_exists() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("config.toml");
    let adjacent_servers_path = temp_dir.path().join("servers.toml");

    fs::write(
        &config_path,
        r#"
[[servers]]
name = "from-config"
host = "1.1.1.1"
"#,
    )
    .unwrap();
    fs::write(
        &adjacent_servers_path,
        r#"
[[servers]]
name = "from-old-path"
host = "2.2.2.2"
"#,
    )
    .unwrap();

    let loaded = ServersToml::load_from_paths(&config_path).unwrap();
    assert!(loaded.contains("from-config"));
    assert!(!loaded.contains("from-old-path"));
}

#[test]
fn test_load_does_not_read_adjacent_servers_file_when_config_has_no_servers() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("config.toml");
    let adjacent_servers_path = temp_dir.path().join("servers.toml");

    fs::write(
        &config_path,
        r#"
[dev]
port = 55555
"#,
    )
    .unwrap();
    fs::write(
        &adjacent_servers_path,
        r#"
[[servers]]
name = "from-old-path"
host = "2.2.2.2"
"#,
    )
    .unwrap();

    let loaded = ServersToml::load_from_paths(&config_path).unwrap();
    assert!(loaded.is_empty());
}

#[test]
fn test_load_returns_empty_when_config_missing_even_if_adjacent_servers_file_exists() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("config.toml");
    let adjacent_servers_path = temp_dir.path().join("servers.toml");

    fs::write(
        &adjacent_servers_path,
        r#"
[[servers]]
name = "from-old-path"
host = "2.2.2.2"
"#,
    )
    .unwrap();

    let loaded = ServersToml::load_from_paths(&config_path).unwrap();
    assert!(loaded.is_empty());
}

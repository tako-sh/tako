use super::*;

#[test]
fn cached_inventory_preserves_same_named_networks_and_skips_inactive_unselected_profiles() {
    use crate::network::tests::{TestHome, fixture};
    let _home = TestHome::new();
    let mut first = fixture();
    let second = fixture();
    let locals = [&first, &second].map(|fixture| {
        crate::network::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
        crate::network::save_profile(&fixture.profile).unwrap()
    });
    let inventory = ServerInventory::load(None).unwrap();
    assert_eq!(inventory.servers.len(), 2);
    for local in &locals {
        let name = format!("test/{}/server", local.id);
        assert!(inventory.contains(&name));
        assert!(
            ServerInventory::load(Some(&local.id))
                .unwrap()
                .contains(&name)
        );
    }
    let crate::network::Profile::Active { profile } = &mut first.profile else {
        panic!()
    };
    let own = profile.snapshot.network.initial_admin.clone();
    profile
        .snapshot
        .members
        .iter_mut()
        .find(|member| member.device_id == own)
        .unwrap()
        .expires_at = Some(1);
    crate::network::write_json(&locals[0].profile_path().unwrap(), &first.profile, true).unwrap();
    assert_eq!(ServerInventory::load(None).unwrap().servers.len(), 1);
    assert!(ServerInventory::load(Some(&locals[0].id)).is_err());
    assert_eq!(
        ServerInventory::load(Some(&locals[1].id))
            .unwrap()
            .servers
            .len(),
        1
    );
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
}

#[test]
fn test_target_normalization_rejects_unknown_values() {
    assert!(ServerTarget::normalize_arch("sparc").is_none());
    assert!(ServerTarget::normalize_libc("uclibc").is_none());
    assert!(ServerTarget::normalized("x86_64", "musl").is_err());
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
fn test_add_server() {
    let mut config = ServerInventory::default();

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
    let mut config = ServerInventory::default();

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
    let mut config = ServerInventory::default();

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
    let mut config = ServerInventory::default();

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
    let mut config = ServerInventory::default();
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
    let mut config = ServerInventory::default();
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
    let mut config = ServerInventory::default();
    let result = config.remove("la");
    assert!(matches!(result, Err(ConfigError::ServerNotFound(_))));
}

#[test]
fn test_update_server() {
    let mut config = ServerInventory::default();

    config
        .add(
            "la".to_string(),
            ServerEntry {
                native: None,
                host: "1.2.3.4".to_string(),
                port: 22,
                http_port: 80,
                https_port: 443,
                description: None,
                key_path: None,
            },
        )
        .unwrap();

    config
        .update(
            "la",
            ServerEntry {
                native: None,
                host: "5.6.7.8".to_string(),
                port: 2222,
                http_port: 8080,
                https_port: 8443,
                description: None,
                key_path: None,
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
    let mut config = ServerInventory::default();

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

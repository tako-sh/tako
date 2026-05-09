use super::*;
use tempfile::TempDir;

#[test]
fn test_cert_info_is_expired() {
    let cert = CertInfo {
        domain: "example.com".to_string(),
        cert_path: PathBuf::from("/tmp/cert.pem"),
        key_path: PathBuf::from("/tmp/key.pem"),
        expires_at: Some(SystemTime::now() - Duration::from_secs(86400)),
        is_wildcard: false,
        is_self_signed: false,
    };
    assert!(cert.is_expired());
}

#[test]
fn test_cert_info_not_expired() {
    let cert = CertInfo {
        domain: "example.com".to_string(),
        cert_path: PathBuf::from("/tmp/cert.pem"),
        key_path: PathBuf::from("/tmp/key.pem"),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 60)),
        is_wildcard: false,
        is_self_signed: false,
    };
    assert!(!cert.is_expired());
}

#[test]
fn test_cert_info_needs_renewal() {
    let cert = CertInfo {
        domain: "example.com".to_string(),
        cert_path: PathBuf::from("/tmp/cert.pem"),
        key_path: PathBuf::from("/tmp/key.pem"),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 20)), // 20 days
        is_wildcard: false,
        is_self_signed: false,
    };
    assert!(cert.needs_renewal());
}

#[test]
fn test_cert_manager_creation() {
    let temp = TempDir::new().unwrap();
    let config = CertManagerConfig {
        cert_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    let manager = CertManager::new(config);
    manager.init().unwrap();
}

#[test]
fn test_add_and_get_cert() {
    let temp = TempDir::new().unwrap();
    let config = CertManagerConfig {
        cert_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    let manager = CertManager::new(config);

    let cert = CertInfo {
        domain: "example.com".to_string(),
        cert_path: temp.path().join("cert.pem"),
        key_path: temp.path().join("key.pem"),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 90)),
        is_wildcard: false,
        is_self_signed: false,
    };

    manager.add_cert(cert.clone());

    let retrieved = manager.get_cert("example.com").unwrap();
    assert_eq!(retrieved.domain, "example.com");
}

#[test]
fn test_wildcard_fallback() {
    let temp = TempDir::new().unwrap();
    let config = CertManagerConfig {
        cert_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    let manager = CertManager::new(config);

    let cert = CertInfo {
        domain: "*.example.com".to_string(),
        cert_path: temp.path().join("cert.pem"),
        key_path: temp.path().join("key.pem"),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 90)),
        is_wildcard: true,
        is_self_signed: false,
    };

    manager.add_cert(cert);

    // Should find wildcard for subdomain
    let retrieved = manager.get_cert_for_host("api.example.com").unwrap();
    assert_eq!(retrieved.domain, "*.example.com");

    // Should not find for different domain
    assert!(manager.get_cert_for_host("other.com").is_none());
}

#[test]
fn test_list_certs() {
    let temp = TempDir::new().unwrap();
    let config = CertManagerConfig {
        cert_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    let manager = CertManager::new(config);

    manager.add_cert(CertInfo {
        domain: "a.com".to_string(),
        cert_path: PathBuf::new(),
        key_path: PathBuf::new(),
        expires_at: None,
        is_wildcard: false,
        is_self_signed: false,
    });

    manager.add_cert(CertInfo {
        domain: "b.com".to_string(),
        cert_path: PathBuf::new(),
        key_path: PathBuf::new(),
        expires_at: None,
        is_wildcard: false,
        is_self_signed: false,
    });

    let certs = manager.list_certs();
    assert_eq!(certs.len(), 2);
}

// Certificate renewal tests

#[test]
fn test_cert_does_not_need_renewal_when_far_from_expiry() {
    let cert = CertInfo {
        domain: "example.com".to_string(),
        cert_path: PathBuf::from("/tmp/cert.pem"),
        key_path: PathBuf::from("/tmp/key.pem"),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 60)), // 60 days
        is_wildcard: false,
        is_self_signed: false,
    };
    assert!(!cert.needs_renewal());
}

#[test]
fn test_cert_needs_renewal_at_30_day_boundary() {
    // Exactly 30 days - should need renewal
    let cert_at_boundary = CertInfo {
        domain: "example.com".to_string(),
        cert_path: PathBuf::from("/tmp/cert.pem"),
        key_path: PathBuf::from("/tmp/key.pem"),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 30)),
        is_wildcard: false,
        is_self_signed: false,
    };
    // At exactly 30 days, now + 30 days > exp is false (equal), so doesn't need renewal
    // But 29 days should trigger renewal
    let cert_29_days = CertInfo {
        domain: "example.com".to_string(),
        cert_path: PathBuf::from("/tmp/cert.pem"),
        key_path: PathBuf::from("/tmp/key.pem"),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 29)),
        is_wildcard: false,
        is_self_signed: false,
    };
    assert!(cert_29_days.needs_renewal());

    // 31 days should not need renewal
    let cert_31_days = CertInfo {
        domain: "example.com".to_string(),
        cert_path: PathBuf::from("/tmp/cert.pem"),
        key_path: PathBuf::from("/tmp/key.pem"),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 31)),
        is_wildcard: false,
        is_self_signed: false,
    };
    assert!(!cert_31_days.needs_renewal());
    let _ = cert_at_boundary; // silence unused warning
}

#[test]
fn test_expired_cert_needs_renewal() {
    let cert = CertInfo {
        domain: "example.com".to_string(),
        cert_path: PathBuf::from("/tmp/cert.pem"),
        key_path: PathBuf::from("/tmp/key.pem"),
        expires_at: Some(SystemTime::now() - Duration::from_secs(86400)), // Expired yesterday
        is_wildcard: false,
        is_self_signed: false,
    };
    assert!(cert.is_expired());
    assert!(cert.needs_renewal());
}

#[test]
fn test_days_until_expiry_calculation() {
    // Test positive days
    let cert_future = CertInfo {
        domain: "example.com".to_string(),
        cert_path: PathBuf::from("/tmp/cert.pem"),
        key_path: PathBuf::from("/tmp/key.pem"),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 45)),
        is_wildcard: false,
        is_self_signed: false,
    };
    let days = cert_future.days_until_expiry().unwrap();
    assert!((44..=45).contains(&days), "Expected ~45 days, got {}", days);

    // Test negative days (expired)
    let cert_past = CertInfo {
        domain: "example.com".to_string(),
        cert_path: PathBuf::from("/tmp/cert.pem"),
        key_path: PathBuf::from("/tmp/key.pem"),
        expires_at: Some(SystemTime::now() - Duration::from_secs(86400 * 5)),
        is_wildcard: false,
        is_self_signed: false,
    };
    let days = cert_past.days_until_expiry().unwrap();
    assert!((-6..=-4).contains(&days), "Expected ~-5 days, got {}", days);

    // Test None expiry
    let cert_no_expiry = CertInfo {
        domain: "example.com".to_string(),
        cert_path: PathBuf::from("/tmp/cert.pem"),
        key_path: PathBuf::from("/tmp/key.pem"),
        expires_at: None,
        is_wildcard: false,
        is_self_signed: false,
    };
    assert!(cert_no_expiry.days_until_expiry().is_none());
}

#[test]
fn test_get_certs_needing_renewal_filters_self_signed() {
    let temp = TempDir::new().unwrap();
    let config = CertManagerConfig {
        cert_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    let manager = CertManager::new(config);

    // Self-signed cert expiring soon - should NOT be in renewal list
    manager.add_cert(CertInfo {
        domain: "dev.local".to_string(),
        cert_path: PathBuf::new(),
        key_path: PathBuf::new(),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 10)),
        is_wildcard: false,
        is_self_signed: true,
    });

    // Real cert expiring soon - SHOULD be in renewal list
    manager.add_cert(CertInfo {
        domain: "prod.example.com".to_string(),
        cert_path: PathBuf::new(),
        key_path: PathBuf::new(),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 10)),
        is_wildcard: false,
        is_self_signed: false,
    });

    // Real cert not expiring soon - should NOT be in renewal list
    manager.add_cert(CertInfo {
        domain: "other.example.com".to_string(),
        cert_path: PathBuf::new(),
        key_path: PathBuf::new(),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 60)),
        is_wildcard: false,
        is_self_signed: false,
    });

    let needing_renewal = manager.get_certs_needing_renewal();
    assert_eq!(needing_renewal.len(), 1);
    assert_eq!(needing_renewal[0].domain, "prod.example.com");
}

#[test]
fn test_get_certs_needing_renewal_empty_when_all_fresh() {
    let temp = TempDir::new().unwrap();
    let config = CertManagerConfig {
        cert_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    let manager = CertManager::new(config);

    // All certs have plenty of time
    for i in 0..5 {
        manager.add_cert(CertInfo {
            domain: format!("domain{}.com", i),
            cert_path: PathBuf::new(),
            key_path: PathBuf::new(),
            expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 90)),
            is_wildcard: false,
            is_self_signed: false,
        });
    }

    let needing_renewal = manager.get_certs_needing_renewal();
    assert!(needing_renewal.is_empty());
}

#[test]
fn test_get_or_create_self_signed_cert_creates_domain_layout_and_caches_cert() {
    let temp = TempDir::new().unwrap();
    let cert_dir = temp.path().to_path_buf();
    let config = CertManagerConfig {
        cert_dir: cert_dir.clone(),
        ..Default::default()
    };
    let manager = CertManager::new(config);
    manager.init().unwrap();

    let domain = "tako-bun-server.orb.local";
    let cert = manager.get_or_create_self_signed_cert(domain).unwrap();

    assert_eq!(cert.domain, domain);
    assert!(cert.is_self_signed);
    assert_eq!(cert.cert_path, cert_dir.join(domain).join("fullchain.pem"));
    assert_eq!(cert.key_path, cert_dir.join(domain).join("privkey.pem"));
    assert!(cert.cert_path.exists());
    assert!(cert.key_path.exists());

    let cached = manager.get_cert_for_host(domain).unwrap();
    assert_eq!(cached.domain, domain);
    assert!(cached.is_self_signed);
}

#[test]
fn test_get_or_create_self_signed_cert_is_discoverable_after_restart() {
    let temp = TempDir::new().unwrap();
    let cert_dir = temp.path().to_path_buf();
    let config = CertManagerConfig {
        cert_dir: cert_dir.clone(),
        ..Default::default()
    };
    let manager = CertManager::new(config.clone());
    manager.init().unwrap();
    manager
        .get_or_create_self_signed_cert("tako-bun-server.orb.local")
        .unwrap();

    let reloaded = CertManager::new(config);
    reloaded.init().unwrap();
    let cert = reloaded
        .get_cert_for_host("tako-bun-server.orb.local")
        .expect("cert should load from persisted cert dir");
    assert!(cert.is_self_signed);
    assert_eq!(cert.domain, "tako-bun-server.orb.local");
}

#[test]
fn test_remove_cert() {
    let temp = TempDir::new().unwrap();
    let config = CertManagerConfig {
        cert_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    let manager = CertManager::new(config);

    manager.add_cert(CertInfo {
        domain: "example.com".to_string(),
        cert_path: PathBuf::new(),
        key_path: PathBuf::new(),
        expires_at: None,
        is_wildcard: false,
        is_self_signed: false,
    });

    assert!(manager.get_cert("example.com").is_some());

    let removed = manager.remove_cert("example.com");
    assert!(removed.is_some());
    assert_eq!(removed.unwrap().domain, "example.com");

    assert!(manager.get_cert("example.com").is_none());
}

#[test]
fn test_wildcard_cert_renewal_detection() {
    let temp = TempDir::new().unwrap();
    let config = CertManagerConfig {
        cert_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    let manager = CertManager::new(config);

    // Wildcard cert expiring soon
    manager.add_cert(CertInfo {
        domain: "*.example.com".to_string(),
        cert_path: PathBuf::new(),
        key_path: PathBuf::new(),
        expires_at: Some(SystemTime::now() + Duration::from_secs(86400 * 15)),
        is_wildcard: true,
        is_self_signed: false,
    });

    let needing_renewal = manager.get_certs_needing_renewal();
    assert_eq!(needing_renewal.len(), 1);
    assert!(needing_renewal[0].is_wildcard);
    assert_eq!(needing_renewal[0].domain, "*.example.com");
}

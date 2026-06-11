use super::*;
use tempfile::TempDir;

#[test]
fn test_generate_ca() {
    let ca = LocalCA::generate().unwrap();
    assert!(ca.ca_cert_pem.contains("BEGIN CERTIFICATE"));
    assert!(ca.ca_key_pem.contains("BEGIN PRIVATE KEY"));
}

#[test]
fn test_generate_leaf_cert() {
    let ca = LocalCA::generate().unwrap();
    let domain = "my-app.test";

    let leaf = ca.generate_leaf_cert(domain).unwrap();

    assert!(leaf.cert_pem.contains("BEGIN CERTIFICATE"));
    assert!(leaf.key_pem.contains("BEGIN PRIVATE KEY"));
}

#[test]
fn test_generate_multiple_leaf_certs() {
    let ca = LocalCA::generate().unwrap();

    let leaf1 = ca.generate_leaf_cert("app1.test").unwrap();
    let leaf2 = ca.generate_leaf_cert("app2.test").unwrap();

    // Each leaf cert should be unique
    assert_ne!(leaf1.cert_pem, leaf2.cert_pem);
    assert_ne!(leaf1.key_pem, leaf2.key_pem);
}

#[test]
fn test_app_domain() {
    assert_eq!(LocalCA::app_domain("my-app"), "my-app.tako.test");
    assert_eq!(LocalCA::app_domain("dashboard"), "dashboard.tako.test");
}

#[test]
fn validate_keypair_accepts_matching_pair() {
    let ca = LocalCA::generate().unwrap();
    validate_keypair(&ca.ca_cert_pem, &ca.ca_key_pem).unwrap();
}

#[test]
fn validate_keypair_rejects_mismatched_pair() {
    let a = LocalCA::generate().unwrap();
    let b = LocalCA::generate().unwrap();
    let err = validate_keypair(&a.ca_cert_pem, &b.ca_key_pem).unwrap_err();
    assert!(
        matches!(err, CaError::Validation(_)),
        "expected Validation, got {err:?}"
    );
}

#[test]
fn test_ca_store_save_and_load() {
    let temp_dir = TempDir::new().unwrap();
    let ca_cert_path = temp_dir.path().join("ca").join("ca.crt");

    let store = LocalCAStore { ca_cert_path };

    let ca = LocalCA::generate().unwrap();
    store.save_ca(&ca).unwrap();
    let loaded = store.load_ca().unwrap();

    assert_eq!(ca.ca_cert_pem, loaded.ca_cert_pem);
    assert_eq!(ca.ca_key_pem, loaded.ca_key_pem);
}

#[test]
fn test_ca_store_get_or_create() {
    let temp_dir = TempDir::new().unwrap();
    let ca_cert_path = temp_dir.path().join("ca").join("ca.crt");

    let store = LocalCAStore {
        ca_cert_path: ca_cert_path.clone(),
    };

    let ca1 = store.get_or_create_ca().unwrap();
    assert!(ca_cert_path.exists());

    let ca2 = store.get_or_create_ca().unwrap();
    assert_eq!(ca1.ca_cert_pem, ca2.ca_cert_pem);
}

#[test]
fn test_ca_store_regenerates_on_mismatched_pair() {
    let temp_dir = TempDir::new().unwrap();
    let ca_cert_path = temp_dir.path().join("ca").join("ca.crt");
    let store = LocalCAStore {
        ca_cert_path: ca_cert_path.clone(),
    };

    // Plant a cert from one CA next to a key from a DIFFERENT CA —
    // exactly the split-brain the old keychain-backed storage could
    // produce. `get_or_create_ca` should detect the mismatch and
    // regenerate rather than silently sign leafs with the wrong key.
    let a = LocalCA::generate().unwrap();
    let b = LocalCA::generate().unwrap();
    std::fs::create_dir_all(ca_cert_path.parent().unwrap()).unwrap();
    std::fs::write(&ca_cert_path, &a.ca_cert_pem).unwrap();
    std::fs::write(ca_cert_path.with_extension("key"), &b.ca_key_pem).unwrap();

    let recovered = store.get_or_create_ca().unwrap();
    // Should not be either of the original halves — must be a fresh pair.
    assert_ne!(recovered.ca_cert_pem, a.ca_cert_pem);
    assert_ne!(recovered.ca_cert_pem, b.ca_cert_pem);
    // And the new pair itself must validate.
    validate_keypair(&recovered.ca_cert_pem, &recovered.ca_key_pem).unwrap();
}

#[test]
fn test_leaf_cert_has_correct_san() {
    let ca = LocalCA::generate().unwrap();
    let domain = "test-app.test";
    let leaf = ca.generate_leaf_cert(domain).unwrap();

    // Parse the certificate to verify SAN
    let (_, cert) = x509_parser::pem::parse_x509_pem(leaf.cert_pem.as_bytes()).unwrap();
    let cert = cert.parse_x509().unwrap();

    // Check Subject Alternative Name extension includes our expected entry.
    let san_ext = cert
        .extensions()
        .iter()
        .find(|ext| ext.oid == x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME)
        .expect("Certificate should have SAN extension");

    let san = match san_ext.parsed_extension() {
        x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) => san,
        other => panic!("Expected SubjectAlternativeName, got {:?}", other),
    };

    let mut has_domain = false;

    for name in san.general_names.iter() {
        if let x509_parser::extensions::GeneralName::DNSName(d) = name
            && *d == domain
        {
            has_domain = true;
        }
    }

    assert!(has_domain, "SAN should include {}", domain);
}

#[test]
fn test_ca_cert_is_ca() {
    let ca = LocalCA::generate().unwrap();

    // Parse and verify it's a CA certificate
    let (_, cert) = x509_parser::pem::parse_x509_pem(ca.ca_cert_pem.as_bytes()).unwrap();
    let cert = cert.parse_x509().unwrap();

    // Check Basic Constraints
    let bc_ext = cert
        .extensions()
        .iter()
        .find(|ext| ext.oid == x509_parser::oid_registry::OID_X509_EXT_BASIC_CONSTRAINTS);

    assert!(
        bc_ext.is_some(),
        "CA certificate should have Basic Constraints"
    );
}

#[test]
fn test_ca_store_loads_from_disk() {
    let temp_dir = TempDir::new().unwrap();
    let ca_cert_path = temp_dir.path().join("ca").join("ca.crt");
    let store = LocalCAStore {
        ca_cert_path: ca_cert_path.clone(),
    };

    let ca = LocalCA::generate().unwrap();
    std::fs::create_dir_all(ca_cert_path.parent().unwrap()).unwrap();
    std::fs::write(&ca_cert_path, ca.ca_cert_pem()).unwrap();
    std::fs::write(ca_cert_path.with_extension("key"), &ca.ca_key_pem).unwrap();

    let loaded = store.load_ca().unwrap();
    assert_eq!(loaded.ca_cert_pem(), ca.ca_cert_pem());
    assert_eq!(loaded.ca_key_pem, ca.ca_key_pem);
}

#[test]
fn test_ca_exists_requires_both_cert_and_key_on_disk() {
    let temp_dir = TempDir::new().unwrap();
    let ca_cert_path = temp_dir.path().join("ca").join("ca.crt");
    let store = LocalCAStore {
        ca_cert_path: ca_cert_path.clone(),
    };

    std::fs::create_dir_all(ca_cert_path.parent().unwrap()).unwrap();
    let ca = LocalCA::generate().unwrap();

    // Only cert on disk → not present.
    std::fs::write(&ca_cert_path, ca.ca_cert_pem()).unwrap();
    assert!(!store.ca_exists());

    // Both present → present.
    std::fs::write(ca_cert_path.with_extension("key"), &ca.ca_key_pem).unwrap();
    assert!(store.ca_exists());
}

#[test]
fn test_delete_ca_removes_both_files() {
    let temp_dir = TempDir::new().unwrap();
    let ca_cert_path = temp_dir.path().join("ca").join("ca.crt");
    let ca_key_path = ca_cert_path.with_extension("key");

    let store = LocalCAStore {
        ca_cert_path: ca_cert_path.clone(),
    };

    let ca = LocalCA::generate().unwrap();
    std::fs::create_dir_all(ca_cert_path.parent().unwrap()).unwrap();
    std::fs::write(&ca_cert_path, ca.ca_cert_pem()).unwrap();
    std::fs::write(&ca_key_path, &ca.ca_key_pem).unwrap();

    store.delete_ca().unwrap();

    assert!(!ca_cert_path.exists());
    assert!(!ca_key_path.exists());
}

#[test]
fn test_load_ca_rejects_old_filenames() {
    let temp_dir = TempDir::new().unwrap();
    let current_ca_cert_path = temp_dir.path().join("ca").join("ca.crt");
    let old_ca_cert_path = temp_dir.path().join("ca").join("tako-ca.crt");

    let store = LocalCAStore {
        ca_cert_path: current_ca_cert_path.clone(),
    };

    let ca = LocalCA::generate().unwrap();
    std::fs::create_dir_all(old_ca_cert_path.parent().unwrap()).unwrap();
    std::fs::write(&old_ca_cert_path, ca.ca_cert_pem()).unwrap();
    std::fs::write(old_ca_cert_path.with_extension("key"), &ca.ca_key_pem).unwrap();

    let err = match store.load_ca() {
        Ok(_) => panic!("old CA filenames should not be loaded"),
        Err(err) => err,
    };
    match err {
        CaError::FileRead(path, _) => assert_eq!(path, current_ca_cert_path),
        other => panic!("expected FileRead error, got {other:?}"),
    }
}

#[test]
fn load_ca_rejects_unexpected_ca_identity() {
    let temp_dir = TempDir::new().unwrap();
    let ca_cert_path = temp_dir.path().join("ca").join("ca.crt");
    let store = LocalCAStore {
        ca_cert_path: ca_cert_path.clone(),
    };

    std::fs::create_dir_all(ca_cert_path.parent().unwrap()).unwrap();
    let wrong = generate_custom_ca("Tako Local Development CA", "Tako");
    std::fs::write(&ca_cert_path, &wrong.ca_cert_pem).unwrap();
    std::fs::write(ca_cert_path.with_extension("key"), &wrong.ca_key_pem).unwrap();

    let err = match store.load_ca() {
        Ok(_) => panic!("invalid CA identity should fail to load"),
        Err(err) => err,
    };
    assert!(matches!(err, CaError::Validation(_)));
}

#[test]
fn get_or_create_ca_regenerates_on_identity_mismatch() {
    let temp_dir = TempDir::new().unwrap();
    let ca_cert_path = temp_dir.path().join("ca").join("ca.crt");
    let store = LocalCAStore {
        ca_cert_path: ca_cert_path.clone(),
    };

    std::fs::create_dir_all(ca_cert_path.parent().unwrap()).unwrap();
    let wrong = generate_custom_ca("Tako Local Development CA", "Tako");
    std::fs::write(&ca_cert_path, &wrong.ca_cert_pem).unwrap();
    std::fs::write(ca_cert_path.with_extension("key"), &wrong.ca_key_pem).unwrap();

    let recovered = store.get_or_create_ca().unwrap();
    assert_ne!(recovered.ca_cert_pem, wrong.ca_cert_pem);
    validate_keypair(&recovered.ca_cert_pem, &recovered.ca_key_pem).unwrap();
    validate_ca_identity(&recovered.ca_cert_pem).unwrap();
}

#[test]
fn effective_trust_prefers_first_explicit_result() {
    assert_eq!(
        effective_trust_by_precedence(&[TrustState::Unspecified, TrustState::Trusted]),
        Some(true)
    );
    assert_eq!(
        effective_trust_by_precedence(&[TrustState::Denied, TrustState::Trusted]),
        Some(false)
    );
    assert_eq!(
        effective_trust_by_precedence(&[TrustState::Trusted, TrustState::Denied]),
        Some(true)
    );
    assert_eq!(
        effective_trust_by_precedence(&[TrustState::Unspecified, TrustState::Unspecified]),
        None
    );
}

#[test]
fn effective_trust_returns_none_when_only_unspecified() {
    assert_eq!(
        effective_trust_by_precedence(&[
            TrustState::Unspecified,
            TrustState::Unspecified,
            TrustState::Unspecified
        ]),
        None
    );
}

#[test]
fn effective_trust_returns_some_for_explicit_values() {
    assert_eq!(
        effective_trust_by_precedence(&[
            TrustState::Unspecified,
            TrustState::Trusted,
            TrustState::Denied
        ]),
        Some(true)
    );
    assert_eq!(
        effective_trust_by_precedence(&[
            TrustState::Unspecified,
            TrustState::Denied,
            TrustState::Trusted
        ]),
        Some(false)
    );
}

#[test]
fn effective_trust_prefers_first_explicit_result_legacy_assertions() {
    assert_eq!(
        effective_trust_by_precedence(&[TrustState::Unspecified, TrustState::Trusted]),
        Some(true)
    );
    assert_eq!(
        effective_trust_by_precedence(&[TrustState::Denied, TrustState::Trusted]),
        Some(false)
    );
    assert_eq!(
        effective_trust_by_precedence(&[TrustState::Trusted, TrustState::Denied]),
        Some(true)
    );
    assert_eq!(
        effective_trust_by_precedence(&[TrustState::Unspecified, TrustState::Unspecified]),
        None
    );
}

fn generate_custom_ca(common_name: &str, organization: &str) -> LocalCA {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    dn.push(DnType::OrganizationName, organization);
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let now = OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + Duration::days(CA_VALIDITY_DAYS);

    let key_pair = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    LocalCA::new(cert.pem(), key_pair.serialize_pem())
}

/// Manual diagnostic — inspects the real Tako CA state on this
/// machine. Never runs in CI (gated by `#[ignore]`). Useful when
/// debugging trust problems:
/// `cargo test -p tako-cli check_real_trust_state -- --ignored --nocapture`
#[cfg(target_os = "macos")]
#[test]
#[ignore = "manual — reads the real user's Tako CA"]
fn check_real_trust_state() {
    let store = LocalCAStore::new().unwrap();
    println!("ca_exists: {}", store.ca_exists());
    println!("is_ca_trusted: {}", store.is_ca_trusted());
}

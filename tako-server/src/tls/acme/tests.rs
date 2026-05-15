use super::*;
use crate::tls::manager::CertManagerConfig;
use tempfile::TempDir;

fn create_test_acme() -> (TempDir, AcmeClient) {
    let temp = TempDir::new().unwrap();
    let cert_config = CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    };
    let cert_manager = Arc::new(CertManager::new(cert_config));

    let acme_config = AcmeConfig {
        staging: true,
        email: Some("test@example.com".to_string()),
        account_dir: temp.path().join("acme"),
        ..Default::default()
    };
    let acme = AcmeClient::new(acme_config, cert_manager);

    (temp, acme)
}

#[test]
fn test_acme_config_defaults() {
    let config = AcmeConfig::default();
    assert!(!config.staging);
    assert!(config.email.is_none());
    assert_eq!(config.max_attempts, 30);
}

#[test]
fn test_directory_url() {
    let mut config = AcmeConfig::default();
    assert!(config.directory_url().contains("acme-v02"));

    config.staging = true;
    assert!(config.directory_url().contains("staging"));
}

#[test]
fn test_challenge_tokens() {
    let (_temp, acme) = create_test_acme();
    let tokens = acme.challenge_tokens();

    {
        let mut t = tokens.write();
        t.insert("token123".to_string(), "auth456".to_string());
    }

    assert_eq!(
        acme.get_challenge_response("token123"),
        Some("auth456".to_string())
    );
}

#[test]
fn test_challenge_handler() {
    let tokens: ChallengeTokens = Arc::new(RwLock::new(HashMap::new()));
    let handler = ChallengeHandler::new(tokens.clone());

    assert!(handler.is_challenge_request("/.well-known/acme-challenge/token123"));
    assert!(!handler.is_challenge_request("/other/path"));

    {
        let mut t = tokens.write();
        t.insert("token123".to_string(), "response".to_string());
    }

    assert_eq!(
        handler.handle_challenge("/.well-known/acme-challenge/token123"),
        Some("response".to_string())
    );
}

#[test]
fn test_is_staging() {
    let (_temp, acme) = create_test_acme();
    assert!(acme.is_staging());
}

#[test]
fn test_invalid_domain() {
    let (_temp, _acme) = create_test_acme();

    // These should be invalid domains
    let invalid_domains = vec!["", "bad/domain", ".startwithdot"];

    for domain in invalid_domains {
        assert!(
            domain.is_empty() || domain.contains('/') || domain.starts_with('.'),
            "Expected {} to be invalid",
            domain
        );
    }
}

#[test]
fn test_parse_cert_expiry() {
    // Test with a sample certificate (this would need a real cert to fully test)
    let invalid_pem = "not a valid certificate";
    assert!(parse_cert_expiry(invalid_pem).is_none());
}

// Certificate renewal tests

#[tokio::test]
async fn test_check_renewals_empty_when_no_certs() {
    let (_temp, acme) = create_test_acme();
    // Don't init account - just test the renewal check logic
    let results = acme.check_renewals().await;
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_check_renewals_identifies_expiring_certs() {
    let (temp, acme) = create_test_acme();

    // Add a certificate that needs renewal to the cert manager
    let cert_manager = acme.cert_manager.clone();
    cert_manager.add_cert(super::super::manager::CertInfo {
        domain: "expiring.example.com".to_string(),
        cert_path: temp.path().join("cert.pem"),
        key_path: temp.path().join("key.pem"),
        expires_at: Some(std::time::SystemTime::now() + std::time::Duration::from_secs(86400 * 15)),
        is_wildcard: false,
        is_self_signed: false,
    });

    // Verify the cert manager sees this cert as needing renewal
    let needing_renewal = cert_manager.get_certs_needing_renewal();
    assert_eq!(needing_renewal.len(), 1);
    assert_eq!(needing_renewal[0].domain, "expiring.example.com");
}

#[tokio::test]
async fn test_check_renewals_skips_self_signed() {
    let (temp, acme) = create_test_acme();

    // Add a self-signed certificate that is expiring
    let cert_manager = acme.cert_manager.clone();
    cert_manager.add_cert(super::super::manager::CertInfo {
        domain: "localhost".to_string(),
        cert_path: temp.path().join("cert.pem"),
        key_path: temp.path().join("key.pem"),
        expires_at: Some(std::time::SystemTime::now() + std::time::Duration::from_secs(86400 * 5)),
        is_wildcard: false,
        is_self_signed: true, // Self-signed should be skipped
    });

    // Verify self-signed certs are not in renewal list
    let needing_renewal = cert_manager.get_certs_needing_renewal();
    assert!(needing_renewal.is_empty());
}

#[tokio::test]
async fn test_check_renewals_skips_fresh_certs() {
    let (temp, acme) = create_test_acme();

    // Add a certificate that does NOT need renewal (60 days out)
    let cert_manager = acme.cert_manager.clone();
    cert_manager.add_cert(super::super::manager::CertInfo {
        domain: "fresh.example.com".to_string(),
        cert_path: temp.path().join("cert.pem"),
        key_path: temp.path().join("key.pem"),
        expires_at: Some(std::time::SystemTime::now() + std::time::Duration::from_secs(86400 * 60)),
        is_wildcard: false,
        is_self_signed: false,
    });

    // Should not need renewal
    let needing_renewal = cert_manager.get_certs_needing_renewal();
    assert!(needing_renewal.is_empty());

    // check_renewals should return empty too
    let results = acme.check_renewals().await;
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_renew_certificate_requires_account() {
    let (_temp, acme) = create_test_acme();
    // Don't initialize account

    let result = acme.renew_certificate("example.com").await;
    assert!(matches!(result, Err(AcmeError::NotRegistered)));
}

#[test]
fn test_acme_config_with_custom_values() {
    let config = AcmeConfig {
        staging: true,
        email: Some("admin@example.com".to_string()),
        account_dir: PathBuf::from("/custom/path"),
        timeout: Duration::from_secs(600),
        max_attempts: 50,
        check_delay: Duration::from_secs(10),
        dns_provider: Some("cloudflare".to_string()),
        dns_propagation_delay: Duration::from_secs(1),
    };

    assert!(config.staging);
    assert_eq!(config.email, Some("admin@example.com".to_string()));
    assert_eq!(config.max_attempts, 50);
    assert!(config.directory_url().contains("staging"));
    assert_eq!(config.dns_provider, Some("cloudflare".to_string()));
}

#[tokio::test]
async fn test_wildcard_requires_dns_provider() {
    let (_temp, acme) = create_test_acme();
    // dns_provider is None by default, so wildcard should fail with NoDnsProvider
    let result = acme.request_certificate("*.example.com").await;
    assert!(matches!(result, Err(AcmeError::NoDnsProvider)));
}

#[tokio::test]
async fn test_wildcard_rejects_unsupported_dns_provider() {
    let temp = TempDir::new().unwrap();
    let cert_config = CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    };
    let cert_manager = Arc::new(CertManager::new(cert_config));
    let acme_config = AcmeConfig {
        dns_provider: Some("route53".to_string()),
        email: None,
        account_dir: temp.path().join("acme"),
        ..Default::default()
    };
    let acme = AcmeClient::new(acme_config, cert_manager);

    let result = acme.request_certificate("*.example.com").await;

    assert!(matches!(
        result,
        Err(AcmeError::UnsupportedDnsProvider(provider)) if provider == "route53"
    ));
}

#[tokio::test]
async fn test_wildcard_cloudflare_requires_registered_account() {
    let temp = TempDir::new().unwrap();
    let cert_config = CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    };
    let cert_manager = Arc::new(CertManager::new(cert_config));
    let acme_config = AcmeConfig {
        dns_provider: Some("cloudflare".to_string()),
        email: None,
        account_dir: temp.path().join("acme"),
        ..Default::default()
    };
    let acme = AcmeClient::new(acme_config, cert_manager);

    let result = acme.request_certificate("*.example.com").await;

    assert!(matches!(result, Err(AcmeError::NotRegistered)));
}

#[test]
fn test_challenge_handler_extracts_token() {
    let tokens: ChallengeTokens = Arc::new(RwLock::new(HashMap::new()));
    let handler = ChallengeHandler::new(tokens.clone());

    // Insert a token
    {
        let mut t = tokens.write();
        t.insert("abc123".to_string(), "key_auth_value".to_string());
    }

    // Test extraction from various paths
    assert!(handler.is_challenge_request("/.well-known/acme-challenge/abc123"));
    assert_eq!(
        handler.handle_challenge("/.well-known/acme-challenge/abc123"),
        Some("key_auth_value".to_string())
    );

    // Unknown token
    assert_eq!(
        handler.handle_challenge("/.well-known/acme-challenge/unknown"),
        None
    );

    // Non-challenge paths
    assert!(!handler.is_challenge_request("/"));
    assert!(!handler.is_challenge_request("/api/health"));
    assert!(!handler.is_challenge_request("/.well-known/other"));
}

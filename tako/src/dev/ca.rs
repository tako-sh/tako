//! Local Certificate Authority for Development
//!
//! Generates and manages a local CA for trusted HTTPS in development.
//! Apps are accessible at `https://{app-name}.test` (with `.tako.test` as a
//! fallback) using certificates signed by the local CA.
//!
//! Storage model:
//! - Root CA cert  → `<tako-data>/ca/ca.crt` (0644, public).
//! - Root CA key   → `<tako-data>/ca/ca.key` (0600, paired with the cert).
//! - Root CA trust → system trust store (installed once via sudo).
//!
//! Cert and key live side-by-side and are always written/regenerated
//! together. On load, the pair is validated (see `validate_keypair`) so a
//! mismatched cert/key combination errors out loudly rather than silently
//! signing leafs that browsers will reject.

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use thiserror::Error;
use time::{Duration, OffsetDateTime};

use super::domain::{SHORT_DEV_DOMAIN, TAKO_DEV_DOMAIN};

/// Root CA certificate validity period (10 years)
const CA_VALIDITY_DAYS: i64 = 3650;

/// Leaf certificate validity period (1 year)
const LEAF_VALIDITY_DAYS: i64 = 365;

/// Root CA common name
const CA_COMMON_NAME: &str = "Tako Development CA";

/// Root CA organization
const CA_ORGANIZATION: &str = "Tako";
const LOCAL_CA_CERT_FILENAME: &str = "ca.crt";

/// Errors that can occur during CA operations
#[derive(Debug, Error)]
pub enum CaError {
    #[error("Failed to generate keypair: {0}")]
    KeypairGeneration(String),

    #[error("Failed to generate certificate: {0}")]
    CertificateGeneration(String),

    #[error("Failed to parse certificate/key: {0}")]
    Parse(String),

    #[error("Failed to read file {0}: {1}")]
    FileRead(PathBuf, std::io::Error),

    #[error("Failed to write file {0}: {1}")]
    FileWrite(PathBuf, std::io::Error),

    #[error("System trust store operation failed: {0}")]
    TrustStore(String),

    #[error("Validation error: {0}")]
    Validation(String),
}

pub type Result<T> = std::result::Result<T, CaError>;

/// A generated certificate with its private key
#[derive(Clone)]
pub struct Certificate {
    /// PEM-encoded certificate
    pub cert_pem: String,
    /// PEM-encoded private key
    pub key_pem: String,
}

/// Local Certificate Authority for development
pub struct LocalCA {
    /// Root CA certificate (PEM)
    ca_cert_pem: String,
    /// Root CA private key (PEM)
    ca_key_pem: String,
}

impl LocalCA {
    /// Create a new LocalCA from existing certificate and key
    pub fn new(ca_cert_pem: String, ca_key_pem: String) -> Self {
        Self {
            ca_cert_pem,
            ca_key_pem,
        }
    }

    /// Get the CA certificate PEM
    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    /// Generate a new Root CA keypair
    pub fn generate() -> Result<Self> {
        let mut params = CertificateParams::default();

        // Set distinguished name
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, CA_COMMON_NAME);
        dn.push(DnType::OrganizationName, CA_ORGANIZATION);
        params.distinguished_name = dn;

        // Set as CA certificate
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);

        // Set key usage for CA
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];

        // Set validity period
        let now = OffsetDateTime::now_utc();
        params.not_before = now;
        params.not_after = now + Duration::days(CA_VALIDITY_DAYS);

        // Generate keypair and certificate
        let key_pair =
            KeyPair::generate().map_err(|e| CaError::KeypairGeneration(e.to_string()))?;

        let cert = params
            .self_signed(&key_pair)
            .map_err(|e| CaError::CertificateGeneration(e.to_string()))?;

        Ok(Self {
            ca_cert_pem: cert.pem(),
            ca_key_pem: key_pair.serialize_pem(),
        })
    }

    /// Generate a leaf certificate for a domain
    ///
    /// The domain should be in the format `{app-name}.test` (or `.tako.test`)
    pub fn generate_leaf_cert(&self, domain: &str) -> Result<Certificate> {
        // Parse the CA key
        let ca_key = KeyPair::from_pem(&self.ca_key_pem)
            .map_err(|e| CaError::Parse(format!("Failed to parse CA private key: {}", e)))?;

        let now = OffsetDateTime::now_utc();
        let ca_params = ca_issuer_params(now);
        let issuer = Issuer::new(ca_params, ca_key);

        // Create leaf certificate parameters
        let mut params = CertificateParams::default();

        // Set distinguished name
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, domain);
        dn.push(DnType::OrganizationName, CA_ORGANIZATION);
        params.distinguished_name = dn;

        // Not a CA
        params.is_ca = IsCa::NoCa;

        // Set key usage for server certificate
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];

        // Extended key usage for TLS server
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

        // Subject Alternative Names
        params.subject_alt_names = vec![SanType::DnsName(domain.try_into().map_err(|e| {
            CaError::Validation(format!("Invalid domain name '{}': {:?}", domain, e))
        })?)];

        // Set validity period
        params.not_before = now;
        params.not_after = now + Duration::days(LEAF_VALIDITY_DAYS);

        // Generate keypair for leaf certificate
        let leaf_key =
            KeyPair::generate().map_err(|e| CaError::KeypairGeneration(e.to_string()))?;

        // Sign with CA
        let leaf_cert = params.signed_by(&leaf_key, &issuer).map_err(|e| {
            CaError::CertificateGeneration(format!("Failed to sign leaf certificate: {}", e))
        })?;

        Ok(Certificate {
            cert_pem: leaf_cert.pem(),
            key_pem: leaf_key.serialize_pem(),
        })
    }

    /// Get the full Tako domain for an app name (`{app}.tako.test`)
    pub fn app_domain(app_name: &str) -> String {
        format!("{}.{}", app_name, TAKO_DEV_DOMAIN)
    }

    /// Get the short domain for an app name (`{app}.test`)
    pub fn app_short_domain(app_name: &str) -> String {
        format!("{}.{}", app_name, SHORT_DEV_DOMAIN)
    }

    /// Generate a leaf certificate with multiple SANs (DNS names and/or IPs).
    ///
    /// The first entry is used as the certificate's Common Name.
    pub fn generate_leaf_cert_for_names(&self, names: &[&str]) -> Result<Certificate> {
        let primary = names
            .first()
            .ok_or_else(|| CaError::Validation("At least one name is required".to_string()))?;

        // Parse the CA key
        let ca_key = KeyPair::from_pem(&self.ca_key_pem)
            .map_err(|e| CaError::Parse(format!("Failed to parse CA private key: {}", e)))?;

        let now = OffsetDateTime::now_utc();
        let ca_params = ca_issuer_params(now);
        let issuer = Issuer::new(ca_params, ca_key);

        // Create leaf certificate parameters
        let mut params = CertificateParams::default();

        // Set distinguished name
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, *primary);
        dn.push(DnType::OrganizationName, CA_ORGANIZATION);
        params.distinguished_name = dn;

        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

        let mut sans = Vec::new();
        for name in names {
            if let Ok(ip) = name.parse::<std::net::IpAddr>() {
                sans.push(SanType::IpAddress(ip));
            } else {
                let dns = (*name).try_into().map_err(|e| {
                    CaError::Validation(format!("Invalid DNS name '{}': {:?}", name, e))
                })?;
                sans.push(SanType::DnsName(dns));
            }
        }
        params.subject_alt_names = sans;

        params.not_before = now;
        params.not_after = now + Duration::days(LEAF_VALIDITY_DAYS);

        let leaf_key =
            KeyPair::generate().map_err(|e| CaError::KeypairGeneration(e.to_string()))?;

        let leaf_cert = params.signed_by(&leaf_key, &issuer).map_err(|e| {
            CaError::CertificateGeneration(format!("Failed to sign leaf certificate: {}", e))
        })?;

        Ok(Certificate {
            cert_pem: leaf_cert.pem(),
            key_pem: leaf_key.serialize_pem(),
        })
    }
}

/// Manages the local CA storage and trust
pub struct LocalCAStore {
    /// Path to the CA certificate file
    ca_cert_path: PathBuf,
}

impl LocalCAStore {
    /// Create a new CA store with default paths
    pub fn new() -> Result<Self> {
        let data_dir = crate::paths::tako_data_dir().map_err(|e| {
            CaError::Validation(format!("Could not determine tako data directory: {}", e))
        })?;

        let ca_dir = data_dir.join("ca");
        let ca_cert_path = ca_dir.join(LOCAL_CA_CERT_FILENAME);

        Ok(Self { ca_cert_path })
    }

    /// Get path to CA certificate
    pub fn ca_cert_path(&self) -> &PathBuf {
        &self.ca_cert_path
    }

    fn ca_key_path(&self) -> PathBuf {
        self.ca_cert_path.with_extension("key")
    }

    fn write_ca_key(&self, key_pem: &str) -> Result<()> {
        let key_path = self.ca_key_path();
        fs::write(&key_path, key_pem).map_err(|e| CaError::FileWrite(key_path.clone(), e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| CaError::FileWrite(key_path.clone(), e))?;
        }
        Ok(())
    }

    fn read_ca_key(&self) -> Result<String> {
        let path = self.ca_key_path();
        fs::read_to_string(&path).map_err(|e| CaError::FileRead(path.clone(), e))
    }

    /// Check if the CA exists (both cert and key present on disk).
    pub fn ca_exists(&self) -> bool {
        self.ca_cert_path.exists() && self.ca_key_path().exists()
    }

    /// Get or create the local CA. If an existing CA is found but its cert
    /// and key don't pair up (a state that used to arise from the
    /// keychain-backed storage; see `validate_keypair`), the broken pair
    /// is replaced with a freshly generated CA.
    pub fn get_or_create_ca(&self) -> Result<LocalCA> {
        match self.load_ca() {
            Ok(ca) => Ok(ca),
            Err(CaError::FileRead(_, _)) | Err(CaError::Validation(_)) => {
                let ca = LocalCA::generate()?;
                self.save_ca(&ca)?;
                Ok(ca)
            }
            Err(err) => Err(err),
        }
    }

    /// Load the existing CA, verifying that the cert and key form a
    /// valid pair. A mismatch returns `CaError::Validation` so callers
    /// can regenerate rather than silently sign with a broken keypair.
    pub fn load_ca(&self) -> Result<LocalCA> {
        let ca_cert_pem = fs::read_to_string(&self.ca_cert_path)
            .map_err(|e| CaError::FileRead(self.ca_cert_path.clone(), e))?;
        let ca_key_pem = self.read_ca_key()?;
        validate_keypair(&ca_cert_pem, &ca_key_pem)?;
        validate_ca_identity(&ca_cert_pem)?;
        Ok(LocalCA::new(ca_cert_pem, ca_key_pem))
    }

    /// Save CA to storage. Cert and key are written together; if either
    /// write fails the partial state is cleaned up so a subsequent load
    /// doesn't see a mismatched pair.
    pub fn save_ca(&self, ca: &LocalCA) -> Result<()> {
        if let Some(parent) = self.ca_cert_path.parent() {
            fs::create_dir_all(parent).map_err(|e| CaError::FileWrite(parent.to_path_buf(), e))?;
        }

        fs::write(&self.ca_cert_path, &ca.ca_cert_pem)
            .map_err(|e| CaError::FileWrite(self.ca_cert_path.clone(), e))?;

        if let Err(err) = self.write_ca_key(&ca.ca_key_pem) {
            // Partial write: remove the cert so ca_exists() stays false
            // and the next run regenerates cleanly.
            let _ = fs::remove_file(&self.ca_cert_path);
            return Err(err);
        }

        Ok(())
    }

    /// Check whether the local CA cert has a usable SSL trust policy in
    /// macOS's trust settings domains.
    ///
    /// Mere presence in the keychain is NOT enough: `add-trusted-cert -d
    /// -r trustRoot` writes both a keychain entry AND a trust-settings
    /// entry, and the two can diverge (e.g. if the settings were cleared
    /// via Keychain Access, or the cert was imported with a non-trust
    /// command). A cert sitting in the keychain with no trust settings
    /// is accepted by `security verify-cert` but rejected by browsers —
    /// this check mirrors the browser's behavior by querying trust
    /// settings directly via SecTrustSettingsCopyTrustSettings. We
    /// evaluate domains in effective precedence order (User → Admin →
    /// System) and use the first explicit result.
    #[cfg(target_os = "macos")]
    pub fn is_ca_trusted(&self) -> bool {
        use security_framework::certificate::SecCertificate;
        use security_framework::trust_settings::{
            Domain, TrustSettings, TrustSettingsForCertificate,
        };

        if !self.ca_cert_path.exists() {
            return false;
        }

        let pem_str = match fs::read_to_string(&self.ca_cert_path) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let der = match pem::parse(pem_str.as_bytes()) {
            Ok(p) => p.contents().to_vec(),
            Err(_) => return false,
        };
        let cert = match SecCertificate::from_der(&der) {
            Ok(c) => c,
            Err(_) => return false,
        };

        let domain_states = [Domain::User, Domain::Admin, Domain::System].map(|domain| {
            match TrustSettings::new(domain).tls_trust_settings_for_certificate(&cert) {
                Ok(Some(TrustSettingsForCertificate::TrustRoot))
                | Ok(Some(TrustSettingsForCertificate::TrustAsRoot)) => TrustState::Trusted,
                Ok(Some(TrustSettingsForCertificate::Deny)) => TrustState::Denied,
                Ok(Some(TrustSettingsForCertificate::Unspecified))
                | Ok(Some(TrustSettingsForCertificate::Invalid))
                | Ok(None)
                | Err(_) => TrustState::Unspecified,
            }
        });

        match effective_trust_by_precedence(&domain_states) {
            Some(explicit) => explicit,
            None => security_verify_cert(&self.ca_cert_path),
        }
    }

    /// Check if CA is trusted - Linux
    ///
    /// Checks both Debian/Ubuntu and Fedora/RHEL trust store paths.
    #[cfg(not(target_os = "macos"))]
    pub fn is_ca_trusted(&self) -> bool {
        // Debian/Ubuntu path
        if PathBuf::from("/usr/local/share/ca-certificates/tako-ca.crt").exists() {
            return true;
        }
        // Fedora/RHEL/SUSE path
        if PathBuf::from("/etc/pki/ca-trust/source/anchors/tako-ca.crt").exists() {
            return true;
        }
        false
    }

    /// Install CA in system trust store (requires sudo)
    #[cfg(target_os = "macos")]
    pub fn install_ca_trust(&self) -> Result<()> {
        let cert_path = self.ca_cert_path.clone();
        if !cert_path.exists() {
            return Err(CaError::Validation(
                "CA certificate not found. Run get_or_create_ca() first.".to_string(),
            ));
        }

        // Skip if this exact cert is already trusted (avoid duplicate entries).
        if self.is_ca_trusted() {
            return Ok(());
        }

        let output = Command::new("sudo")
            .args([
                "security",
                "add-trusted-cert",
                "-d",
                "-r",
                "trustRoot",
                "-k",
                "/Library/Keychains/System.keychain",
                cert_path.to_str().unwrap_or(""),
            ])
            .output()
            .map_err(|e| CaError::TrustStore(format!("Failed to run security command: {}", e)))?;

        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(CaError::TrustStore(if detail.is_empty() {
                "Failed to install CA in trust store".to_string()
            } else {
                format!("Failed to install CA in trust store: {detail}")
            }));
        }
        Ok(())
    }

    /// Install CA in system trust store - Linux
    ///
    /// Detects distro family and uses the appropriate trust store path:
    /// - Debian/Ubuntu: /usr/local/share/ca-certificates/ + update-ca-certificates
    /// - Fedora/RHEL/SUSE: /etc/pki/ca-trust/source/anchors/ + update-ca-trust
    #[cfg(not(target_os = "macos"))]
    pub fn install_ca_trust(&self) -> Result<()> {
        let cert_path = self.ca_cert_path.clone();
        if !cert_path.exists() {
            return Err(CaError::Validation(
                "CA certificate not found. Run get_or_create_ca() first.".to_string(),
            ));
        }

        let cert_str = cert_path.to_str().unwrap_or("");

        // Try Debian/Ubuntu first (most common)
        let debian_dir = PathBuf::from("/usr/local/share/ca-certificates");
        let fedora_dir = PathBuf::from("/etc/pki/ca-trust/source/anchors");

        if debian_dir.exists() {
            let dest = "/usr/local/share/ca-certificates/tako-ca.crt";
            run_sudo_captured(
                &["cp", cert_str, dest],
                "Failed to copy CA to system directory",
            )?;
            run_sudo_captured(
                &["update-ca-certificates"],
                "Failed to update system CA certificates",
            )?;
        } else if fedora_dir.exists() {
            let dest = "/etc/pki/ca-trust/source/anchors/tako-ca.crt";
            run_sudo_captured(
                &["cp", cert_str, dest],
                "Failed to copy CA to system directory",
            )?;
            run_sudo_captured(&["update-ca-trust"], "Failed to update system CA trust")?;
        } else {
            return Err(CaError::TrustStore(
                "Could not find system CA trust store. Manually trust the CA at: ".to_string()
                    + cert_str,
            ));
        }

        Ok(())
    }

    /// Delete the CA from disk. The cert remains in the system trust
    /// store until explicitly removed via `security delete-certificate`
    /// (macOS) or `update-ca-trust` (Linux) — that's a sudo operation
    /// and not done here.
    pub fn delete_ca(&self) -> Result<()> {
        if self.ca_cert_path.exists() {
            fs::remove_file(&self.ca_cert_path)
                .map_err(|e| CaError::FileWrite(self.ca_cert_path.clone(), e))?;
        }
        let key_path = self.ca_key_path();
        if key_path.exists() {
            fs::remove_file(&key_path).map_err(|e| CaError::FileWrite(key_path.clone(), e))?;
        }
        Ok(())
    }
}

/// Run a sudo command with stdout/stderr captured. Surfaces captured stderr
/// in the error message on failure so diagnostics aren't lost.
#[cfg(not(target_os = "macos"))]
fn run_sudo_captured(args: &[&str], failure_message: &str) -> Result<()> {
    let output = Command::new("sudo")
        .args(args)
        .output()
        .map_err(|e| CaError::TrustStore(format!("Failed to run sudo {}: {}", args[0], e)))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(CaError::TrustStore(if detail.is_empty() {
            failure_message.to_string()
        } else {
            format!("{failure_message}: {detail}")
        }));
    }
    Ok(())
}

/// Verify that the CA cert's public key matches the private key.
///
/// Without this check, a cert/key divergence (e.g. the user regenerated
/// one file but not the other) goes unnoticed: the dev-server happily
/// signs leafs with the private key, but browsers reject them because
/// the signature doesn't verify against the trusted root's public key.
/// This surfaces the mismatch at load time as `CaError::Validation`.
fn validate_keypair(cert_pem: &str, key_pem: &str) -> Result<()> {
    // Parse the cert and extract its SubjectPublicKeyInfo bytes.
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|e| CaError::Parse(format!("ca.crt: {e}")))?;
    let cert = pem
        .parse_x509()
        .map_err(|e| CaError::Parse(format!("ca.crt x509: {e}")))?;
    let cert_spki = cert.tbs_certificate.subject_pki.raw;

    // Derive SPKI from the private key by re-serializing via rcgen, then
    // decode the emitted PEM back to DER so we can byte-compare against
    // the cert's raw SPKI bytes.
    let key = KeyPair::from_pem(key_pem).map_err(|e| CaError::Parse(format!("ca.key: {e}")))?;
    let key_spki_pem = key.public_key_pem();
    let key_spki = pem::parse(key_spki_pem.as_bytes())
        .map_err(|e| CaError::Parse(format!("ca.key spki: {e}")))?;
    let key_spki = key_spki.contents();

    if cert_spki == key_spki {
        Ok(())
    } else {
        Err(CaError::Validation(
            "CA cert and key don't pair (public keys differ). \
             The CA will be regenerated."
                .to_string(),
        ))
    }
}

fn validate_ca_identity(cert_pem: &str) -> Result<()> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|e| CaError::Parse(format!("ca.crt: {e}")))?;
    let cert = pem
        .parse_x509()
        .map_err(|e| CaError::Parse(format!("ca.crt x509: {e}")))?;
    let subject = cert.tbs_certificate.subject;

    let common_name = subject
        .iter_common_name()
        .next()
        .and_then(|attr| attr.as_str().ok());
    if common_name != Some(CA_COMMON_NAME) {
        return Err(CaError::Validation(format!(
            "Unexpected CA common name {:?}; expected {:?}. The CA will be regenerated.",
            common_name, CA_COMMON_NAME
        )));
    }

    let organization = subject
        .iter_organization()
        .next()
        .and_then(|attr| attr.as_str().ok());
    if organization != Some(CA_ORGANIZATION) {
        return Err(CaError::Validation(format!(
            "Unexpected CA organization {:?}; expected {:?}. The CA will be regenerated.",
            organization, CA_ORGANIZATION
        )));
    }

    Ok(())
}

fn ca_issuer_params(now: OffsetDateTime) -> CertificateParams {
    let mut ca_params = CertificateParams::default();
    let mut ca_dn = DistinguishedName::new();
    ca_dn.push(DnType::CommonName, CA_COMMON_NAME);
    ca_dn.push(DnType::OrganizationName, CA_ORGANIZATION);
    ca_params.distinguished_name = ca_dn;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    ca_params.not_before = now - Duration::days(1); // Allow for clock skew
    ca_params.not_after = now + Duration::days(CA_VALIDITY_DAYS);
    ca_params
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrustState {
    Unspecified,
    Trusted,
    Denied,
}

#[cfg(any(target_os = "macos", test))]
fn effective_trust_by_precedence(states: &[TrustState]) -> Option<bool> {
    for state in states {
        match state {
            TrustState::Trusted => return Some(true),
            TrustState::Denied => return Some(false),
            TrustState::Unspecified => {}
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn security_verify_cert(cert_path: &std::path::Path) -> bool {
    Command::new("security")
        .args([
            "verify-cert",
            "-c",
            cert_path.to_str().unwrap_or(""),
            "-p",
            "ssl",
        ])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

impl Default for LocalCAStore {
    fn default() -> Self {
        Self::new().expect("Failed to create LocalCAStore")
    }
}

#[cfg(test)]
mod tests {
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
    /// `cargo test -p tako check_real_trust_state -- --ignored --nocapture`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "manual — reads the real user's Tako CA"]
    fn check_real_trust_state() {
        let store = LocalCAStore::new().unwrap();
        println!("ca_exists: {}", store.ca_exists());
        println!("is_ca_trusted: {}", store.is_ca_trusted());
    }
}

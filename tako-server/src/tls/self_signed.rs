//! Self-signed certificate generation for development

#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur during self-signed cert generation
#[derive(Debug, Error)]
pub enum SelfSignedError {
    #[error("Failed to generate certificate: {0}")]
    GenerationError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Self-signed certificate for development
#[derive(Debug, Clone)]
pub struct SelfSignedCert {
    /// Path to certificate file (PEM)
    pub cert_path: PathBuf,
    /// Path to private key file (PEM)
    pub key_path: PathBuf,
}

impl SelfSignedCert {
    /// Check if certificate files exist
    pub fn exists(&self) -> bool {
        self.cert_path.exists() && self.key_path.exists()
    }
}

/// Generator for self-signed certificates
pub struct SelfSignedGenerator {
    /// Directory to store certificates
    cert_dir: PathBuf,
}

impl SelfSignedGenerator {
    pub fn new(cert_dir: impl Into<PathBuf>) -> Self {
        Self {
            cert_dir: cert_dir.into(),
        }
    }

    /// Get or create a self-signed certificate for localhost
    pub fn get_or_create_localhost(&self) -> Result<SelfSignedCert, SelfSignedError> {
        self.get_or_create_for_domain("localhost")
    }

    /// Get or create a self-signed certificate for an arbitrary hostname.
    pub fn get_or_create_for_domain(
        &self,
        domain: &str,
    ) -> Result<SelfSignedCert, SelfSignedError> {
        if domain.trim().is_empty() {
            return Err(SelfSignedError::GenerationError(
                "domain must not be empty".to_string(),
            ));
        }

        let cert = self.cert_paths_for_domain(domain);
        if cert.exists() {
            return Ok(cert);
        }

        self.generate_for_domain(&cert, domain)?;
        Ok(cert)
    }

    fn cert_paths_for_domain(&self, domain: &str) -> SelfSignedCert {
        if domain == "localhost" {
            return SelfSignedCert {
                cert_path: self.cert_dir.join("localhost.crt"),
                key_path: self.cert_dir.join("localhost.key"),
            };
        }

        let file_stem: String = domain
            .chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' => c,
                _ => '_',
            })
            .collect();

        let domain_dir = self.cert_dir.join("domains");
        SelfSignedCert {
            cert_path: domain_dir.join(format!("{}.crt", file_stem)),
            key_path: domain_dir.join(format!("{}.key", file_stem)),
        }
    }

    /// Generate a self-signed certificate for localhost
    fn generate_for_domain(
        &self,
        cert: &SelfSignedCert,
        domain: &str,
    ) -> Result<(), SelfSignedError> {
        if let Some(parent) = cert.cert_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(parent) = cert.key_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Use rcgen to generate certificate
        use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};

        let mut params = CertificateParams::default();

        // Set subject
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, domain);
        dn.push(DnType::OrganizationName, "Tako");
        params.distinguished_name = dn;

        let dns_name = domain.try_into().map_err(|e| {
            SelfSignedError::GenerationError(format!("Invalid DNS name '{}': {}", domain, e))
        })?;
        params.subject_alt_names = vec![SanType::DnsName(dns_name)];

        if domain == "localhost" {
            params
                .subject_alt_names
                .push(SanType::DnsName("*.localhost".try_into().unwrap()));
            params
                .subject_alt_names
                .push(SanType::IpAddress(std::net::IpAddr::V4(
                    std::net::Ipv4Addr::new(127, 0, 0, 1),
                )));
            params
                .subject_alt_names
                .push(SanType::IpAddress(std::net::IpAddr::V6(
                    std::net::Ipv6Addr::LOCALHOST,
                )));
        }

        // Generate key pair
        let key_pair = KeyPair::generate().map_err(|e| {
            SelfSignedError::GenerationError(format!("Failed to generate key pair: {}", e))
        })?;

        // Generate certificate
        let cert_der = params.self_signed(&key_pair).map_err(|e| {
            SelfSignedError::GenerationError(format!("Failed to generate certificate: {}", e))
        })?;

        // Write certificate
        std::fs::write(&cert.cert_path, cert_der.pem())?;

        // Write private key
        std::fs::write(&cert.key_path, key_pair.serialize_pem())?;

        // Set restrictive permissions on key file
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cert.key_path, std::fs::Permissions::from_mode(0o600))?;
        }

        tracing::info!(
            cert_path = %cert.cert_path.display(),
            key_path = %cert.key_path.display(),
            domain = %domain,
            "Generated self-signed certificate"
        );

        Ok(())
    }

    #[cfg(test)]
    /// Get path to certificate directory
    pub fn cert_dir(&self) -> &Path {
        &self.cert_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_self_signed_cert_exists() {
        let cert = SelfSignedCert {
            cert_path: PathBuf::from("/nonexistent/cert.pem"),
            key_path: PathBuf::from("/nonexistent/key.pem"),
        };
        assert!(!cert.exists());
    }

    #[test]
    fn test_generator_creation() {
        let temp = TempDir::new().unwrap();
        let generator = SelfSignedGenerator::new(temp.path());
        assert_eq!(generator.cert_dir(), temp.path());
    }

    #[test]
    fn test_generate_localhost_cert() {
        let temp = TempDir::new().unwrap();
        let generator = SelfSignedGenerator::new(temp.path());

        let cert = generator.get_or_create_localhost().unwrap();
        assert!(cert.exists());

        // Verify files have content
        let cert_content = std::fs::read_to_string(&cert.cert_path).unwrap();
        assert!(cert_content.contains("BEGIN CERTIFICATE"));

        let key_content = std::fs::read_to_string(&cert.key_path).unwrap();
        assert!(key_content.contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn test_reuse_existing_cert() {
        let temp = TempDir::new().unwrap();
        let generator = SelfSignedGenerator::new(temp.path());

        // Generate first time
        let cert1 = generator.get_or_create_localhost().unwrap();
        let content1 = std::fs::read_to_string(&cert1.cert_path).unwrap();

        // Get second time (should reuse)
        let cert2 = generator.get_or_create_localhost().unwrap();
        let content2 = std::fs::read_to_string(&cert2.cert_path).unwrap();

        // Should be the same certificate
        assert_eq!(content1, content2);
    }

    #[test]
    fn test_generate_custom_domain_cert() {
        let temp = TempDir::new().unwrap();
        let generator = SelfSignedGenerator::new(temp.path());

        let cert = generator.get_or_create_for_domain("example.test").unwrap();
        assert!(cert.exists());

        let cert_content = std::fs::read_to_string(&cert.cert_path).unwrap();
        assert!(cert_content.contains("BEGIN CERTIFICATE"));
    }
}

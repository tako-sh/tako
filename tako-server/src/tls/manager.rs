//! Certificate manager - handles certificate lifecycle

use super::SelfSignedGenerator;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use x509_parser::prelude::*;

/// Errors that can occur during certificate management
#[derive(Debug, Error)]
pub enum CertError {
    #[error("Certificate not found for domain: {0}")]
    NotFound(String),

    #[error("Certificate expired for domain: {0}")]
    Expired(String),

    #[error("Failed to load certificate: {0}")]
    LoadError(String),

    #[error("Failed to parse certificate: {0}")]
    ParseError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Information about a certificate
#[derive(Debug, Clone)]
pub struct CertInfo {
    /// Domain the certificate is for
    pub domain: String,
    /// Path to certificate file
    pub cert_path: PathBuf,
    /// Path to private key file
    pub key_path: PathBuf,
    /// When the certificate expires
    pub expires_at: Option<SystemTime>,
    /// Whether this is a wildcard certificate
    pub is_wildcard: bool,
    /// Whether this is self-signed (dev mode)
    pub is_self_signed: bool,
}

impl CertInfo {
    /// Check if certificate is expired
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .map(|exp| SystemTime::now() > exp)
            .unwrap_or(false)
    }

    /// Check if certificate needs renewal (expires within 30 days)
    pub fn needs_renewal(&self) -> bool {
        self.expires_at
            .map(|exp| {
                let thirty_days = Duration::from_secs(30 * 24 * 60 * 60);
                SystemTime::now() + thirty_days > exp
            })
            .unwrap_or(false)
    }

    /// Days until expiry
    pub fn days_until_expiry(&self) -> Option<i64> {
        self.expires_at
            .map(|exp| match exp.duration_since(SystemTime::now()) {
                Ok(duration) => (duration.as_secs() / 86400) as i64,
                Err(e) => -(e.duration().as_secs() as i64 / 86400),
            })
    }
}

/// Certificate manager configuration
#[derive(Debug, Clone)]
pub struct CertManagerConfig {
    /// Directory to store certificates
    pub cert_dir: PathBuf,
    /// How often to check for certificate renewal
    pub check_interval: Duration,
    /// Renew certificates this many days before expiry
    pub renewal_days: u32,
}

impl Default for CertManagerConfig {
    fn default() -> Self {
        Self {
            cert_dir: PathBuf::from("/opt/tako/certs"),
            check_interval: Duration::from_secs(24 * 60 * 60), // 24 hours
            renewal_days: 30,
        }
    }
}

/// Manages certificates for all domains
pub struct CertManager {
    config: CertManagerConfig,
    /// Cached certificate info by domain
    certs: RwLock<HashMap<String, CertInfo>>,
}

impl CertManager {
    pub fn new(config: CertManagerConfig) -> Self {
        Self {
            config,
            certs: RwLock::new(HashMap::new()),
        }
    }

    /// Initialize by loading existing certificates
    pub fn init(&self) -> Result<(), CertError> {
        std::fs::create_dir_all(&self.config.cert_dir)?;
        self.load_all_certs()?;
        Ok(())
    }

    /// Load all certificates from disk
    fn load_all_certs(&self) -> Result<(), CertError> {
        let mut certs = self.certs.write();

        if !self.config.cert_dir.exists() {
            return Ok(());
        }

        for entry in std::fs::read_dir(&self.config.cert_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                let domain = path.file_name().unwrap().to_string_lossy().to_string();
                if let Ok(cert_info) = self.load_cert_info(&domain) {
                    certs.insert(domain, cert_info);
                }
            }
        }

        Ok(())
    }

    /// Load certificate info for a domain
    fn load_cert_info(&self, domain: &str) -> Result<CertInfo, CertError> {
        let domain_dir = self.config.cert_dir.join(domain);
        let cert_path = domain_dir.join("fullchain.pem");
        let key_path = domain_dir.join("privkey.pem");

        if !cert_path.exists() || !key_path.exists() {
            return Err(CertError::NotFound(domain.to_string()));
        }

        // Read PEM once and derive all certificate metadata from the same bytes
        let pem_data = std::fs::read(&cert_path)?;
        let expires_at = Self::parse_cert_expiry_from_bytes(&pem_data).ok();
        let is_self_signed = Self::check_self_signed_from_bytes(&pem_data).unwrap_or(false);

        Ok(CertInfo {
            domain: domain.to_string(),
            cert_path,
            key_path,
            expires_at,
            is_wildcard: domain.starts_with("*."),
            is_self_signed,
        })
    }

    /// Parse certificate expiry date from PEM bytes
    fn parse_cert_expiry_from_bytes(pem_data: &[u8]) -> Result<SystemTime, CertError> {
        for pem in Pem::iter_from_buffer(pem_data) {
            let pem = pem.map_err(|e| CertError::ParseError(e.to_string()))?;

            if pem.label == "CERTIFICATE" {
                let (_, cert) = X509Certificate::from_der(&pem.contents)
                    .map_err(|e| CertError::ParseError(e.to_string()))?;

                // Get the not_after time (expiry)
                let not_after = cert.validity().not_after;

                // Convert ASN1Time to SystemTime
                let timestamp = not_after.timestamp();
                if timestamp < 0 {
                    return Err(CertError::ParseError(
                        "certificate has negative expiry timestamp".to_string(),
                    ));
                }
                let system_time = UNIX_EPOCH + Duration::from_secs(timestamp as u64);

                return Ok(system_time);
            }
        }

        Err(CertError::ParseError(
            "No certificate found in PEM file".to_string(),
        ))
    }

    /// Check if certificate is self-signed from PEM bytes
    fn check_self_signed_from_bytes(pem_data: &[u8]) -> Result<bool, CertError> {
        for pem in Pem::iter_from_buffer(pem_data) {
            let pem = pem.map_err(|e| CertError::ParseError(e.to_string()))?;

            if pem.label == "CERTIFICATE" {
                let (_, cert) = X509Certificate::from_der(&pem.contents)
                    .map_err(|e| CertError::ParseError(e.to_string()))?;

                // Self-signed certificates have the same issuer and subject
                return Ok(cert.issuer() == cert.subject());
            }
        }

        Ok(false)
    }

    /// Get certificate for a domain
    pub fn get_cert(&self, domain: &str) -> Option<CertInfo> {
        let certs = self.certs.read();
        certs.get(domain).cloned()
    }

    /// Get certificate for a domain, falling back to wildcard
    pub fn get_cert_for_host(&self, host: &str) -> Option<CertInfo> {
        let certs = self.certs.read();

        // Try exact match first
        if let Some(cert) = certs.get(host) {
            return Some(cert.clone());
        }

        // Try wildcard match
        if let Some(dot_pos) = host.find('.') {
            let wildcard = format!("*.{}", &host[dot_pos + 1..]);
            if let Some(cert) = certs.get(&wildcard) {
                return Some(cert.clone());
            }
        }

        None
    }

    /// Add a certificate
    pub fn add_cert(&self, cert_info: CertInfo) {
        let mut certs = self.certs.write();
        certs.insert(cert_info.domain.clone(), cert_info);
    }

    /// Remove a certificate
    pub fn remove_cert(&self, domain: &str) -> Option<CertInfo> {
        let mut certs = self.certs.write();
        certs.remove(domain)
    }

    /// List all certificates
    pub fn list_certs(&self) -> Vec<CertInfo> {
        let certs = self.certs.read();
        certs.values().cloned().collect()
    }

    /// Get certificates that need renewal
    pub fn get_certs_needing_renewal(&self) -> Vec<CertInfo> {
        let certs = self.certs.read();
        certs
            .values()
            .filter(|c| c.needs_renewal() && !c.is_self_signed)
            .cloned()
            .collect()
    }

    /// Get certificate directory
    pub fn cert_dir(&self) -> &Path {
        &self.config.cert_dir
    }

    /// Get domain certificate directory
    pub fn domain_cert_dir(&self, domain: &str) -> PathBuf {
        self.config.cert_dir.join(domain)
    }

    /// Get or create a self-signed certificate stored in the standard domain layout.
    ///
    /// This keeps private/local domains usable over HTTPS even when ACME cannot issue for them.
    pub fn get_or_create_self_signed_cert(&self, domain: &str) -> Result<CertInfo, CertError> {
        let domain = domain.trim();
        if domain.is_empty() {
            return Err(CertError::LoadError("domain must not be empty".to_string()));
        }

        if let Some(existing) = self.get_cert(domain) {
            return Ok(existing);
        }

        let domain_dir = self.domain_cert_dir(domain);
        let cert_path = domain_dir.join("fullchain.pem");
        let key_path = domain_dir.join("privkey.pem");

        if cert_path.exists() && key_path.exists() {
            let cert_info = self.load_cert_info(domain)?;
            self.add_cert(cert_info.clone());
            return Ok(cert_info);
        }

        let generator = SelfSignedGenerator::new(self.config.cert_dir.clone());
        let generated = generator
            .get_or_create_for_domain(domain)
            .map_err(|e| CertError::LoadError(format!("self-signed generation failed: {}", e)))?;

        std::fs::create_dir_all(&domain_dir)?;
        std::fs::copy(&generated.cert_path, &cert_path)?;
        std::fs::copy(&generated.key_path, &key_path)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
        }

        let cert_info = self.load_cert_info(domain)?;
        self.add_cert(cert_info.clone());
        Ok(cert_info)
    }
}

#[cfg(test)]
mod tests;

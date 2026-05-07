//! SNI-based certificate selection for TLS
//!
//! Implements dynamic certificate selection during TLS handshake
//! based on the SNI (Server Name Indication) hostname.

use super::{CertInfo, CertManager};
use async_trait::async_trait;
use openssl::pkey::PKey;
use openssl::ssl::SslRef;
use openssl::x509::X509;
use pingora_core::listeners::TlsAccept;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Rate limiter for log messages that may fire on every TLS handshake.
/// Allows one log emission per `interval` seconds; suppressed events are
/// counted and reported in the next allowed emission.
struct LogRateLimiter {
    suppressed: AtomicU64,
    last_log: parking_lot::Mutex<Instant>,
    interval: std::time::Duration,
}

impl LogRateLimiter {
    fn new(interval: std::time::Duration) -> Self {
        Self {
            suppressed: AtomicU64::new(0),
            last_log: parking_lot::Mutex::new(Instant::now() - interval),
            interval,
        }
    }

    /// Returns `Some(suppressed_count)` if a log should be emitted now,
    /// `None` if the event should be suppressed.
    fn check(&self) -> Option<u64> {
        let now = Instant::now();
        let mut last = self.last_log.lock();
        if now.duration_since(*last) >= self.interval {
            *last = now;
            let count = self.suppressed.swap(0, Ordering::Relaxed);
            Some(count)
        } else {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

static NO_SNI_LIMITER: std::sync::LazyLock<LogRateLimiter> =
    std::sync::LazyLock::new(|| LogRateLimiter::new(std::time::Duration::from_secs(10)));

static UNKNOWN_HOST_LIMITER: std::sync::LazyLock<LogRateLimiter> =
    std::sync::LazyLock::new(|| LogRateLimiter::new(std::time::Duration::from_secs(10)));

/// Cached parsed certificate and key pair (leaf + intermediates).
#[derive(Clone)]
struct CachedCert {
    cert: X509,
    chain: Vec<X509>,
    key: PKey<openssl::pkey::Private>,
    /// File mtime when the cert was loaded, used to detect ACME renewals.
    mtime: std::time::SystemTime,
}

/// SNI-based certificate resolver that selects certificates based on hostname.
/// Caches parsed certs in memory so `certificate_callback` never reads from disk
/// on the hot path (critical under TLS connection floods).
pub struct SniCertResolver {
    cert_manager: Arc<CertManager>,
    cache: dashmap::DashMap<PathBuf, CachedCert>,
}

impl SniCertResolver {
    /// Create a new SNI certificate resolver
    pub fn new(cert_manager: Arc<CertManager>) -> Self {
        Self {
            cert_manager,
            cache: dashmap::DashMap::new(),
        }
    }

    /// Get or load a certificate from the in-memory cache.
    /// On first access the cert is read from disk and cached. On ACME renewal
    /// the CertManager updates the on-disk files; the next call here detects the
    /// mtime change and reloads.
    fn get_or_load_cert(
        &self,
        cert_path: &std::path::Path,
        key_path: &std::path::Path,
    ) -> Result<CachedCert, openssl::error::ErrorStack> {
        // Check cache, but reload if the cert file has been updated (e.g. ACME renewal).
        if let Some(cached) = self.cache.get(cert_path) {
            let current_mtime = std::fs::metadata(cert_path)
                .and_then(|m| m.modified())
                .unwrap_or(cached.mtime);
            if current_mtime == cached.mtime {
                return Ok(cached.clone());
            }
            // File changed on disk — drop cached entry and reload below.
            drop(cached);
            self.cache.remove(cert_path);
        }

        let mtime = std::fs::metadata(cert_path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let (cert, chain, key) = Self::load_cert_and_key(cert_path, key_path)?;
        let cached = CachedCert {
            cert,
            chain,
            key,
            mtime,
        };
        self.cache.insert(cert_path.to_path_buf(), cached.clone());
        Ok(cached)
    }

    /// Load certificate and key from files (cold path only)
    fn load_cert_and_key(
        cert_path: &std::path::Path,
        key_path: &std::path::Path,
    ) -> Result<(X509, Vec<X509>, PKey<openssl::pkey::Private>), openssl::error::ErrorStack> {
        let cert_pem = std::fs::read(cert_path).map_err(|e| {
            tracing::error!("Failed to read cert file {:?}: {}", cert_path, e);
            openssl::error::ErrorStack::get()
        })?;
        let key_pem = std::fs::read(key_path).map_err(|e| {
            tracing::error!("Failed to read key file {:?}: {}", key_path, e);
            openssl::error::ErrorStack::get()
        })?;

        // Load full chain: first cert is the leaf, rest are intermediates.
        let all_certs = X509::stack_from_pem(&cert_pem)?;
        let cert = all_certs
            .first()
            .cloned()
            .ok_or_else(openssl::error::ErrorStack::get)?;
        let chain = all_certs.into_iter().skip(1).collect();
        let key = PKey::private_key_from_pem(&key_pem)?;

        Ok((cert, chain, key))
    }

    fn default_cert_info(&self) -> Option<CertInfo> {
        let existing = self
            .cert_manager
            .get_cert("default")
            .or_else(|| self.cert_manager.list_certs().into_iter().next());
        if existing.is_some() {
            return existing;
        }

        match self.cert_manager.get_or_create_self_signed_cert("default") {
            Ok(cert) => Some(cert),
            Err(error) => {
                tracing::warn!("Failed to create fallback TLS certificate: {}", error);
                None
            }
        }
    }

    fn set_default_cert(&self, ssl: &mut SslRef, reason: &str) {
        if let Some(cert_info) = self.default_cert_info() {
            match self.get_or_load_cert(&cert_info.cert_path, &cert_info.key_path) {
                Ok(cached) => {
                    if let Err(e) = ssl.set_certificate(&cached.cert) {
                        tracing::error!("Failed to set default certificate ({reason}): {}", e);
                    }
                    for intermediate in &cached.chain {
                        if let Err(e) = ssl.add_chain_cert(intermediate.clone()) {
                            tracing::error!("Failed to add intermediate cert ({reason}): {}", e);
                        }
                    }
                    if let Err(e) = ssl.set_private_key(&cached.key) {
                        tracing::error!("Failed to set default private key ({reason}): {}", e);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to load default certificate ({reason}): {}", e);
                }
            }
        }
    }
}

impl std::fmt::Debug for SniCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SniCertResolver").finish()
    }
}

#[async_trait]
impl TlsAccept for SniCertResolver {
    async fn certificate_callback(&self, ssl: &mut SslRef) {
        // Get the SNI hostname from the TLS handshake
        let sni_hostname = match ssl.servername(openssl::ssl::NameType::HOST_NAME) {
            Some(name) => name.to_string(),
            None => {
                crate::metrics::record_tls_handshake_failure("no_sni");
                if let Some(suppressed) = NO_SNI_LIMITER.check() {
                    if suppressed > 0 {
                        tracing::warn!(suppressed, "No SNI hostname in TLS handshake (repeated)");
                    } else {
                        tracing::warn!("No SNI hostname in TLS handshake");
                    }
                }
                if should_allow_default_cert_fallback_for_missing_sni() {
                    self.set_default_cert(ssl, "no-sni");
                }
                return;
            }
        };

        tracing::debug!(hostname = %sni_hostname, "SNI certificate lookup");

        // Look up certificate for this hostname (with wildcard fallback)
        match self.cert_manager.get_cert_for_host(&sni_hostname) {
            Some(cert_info) => {
                tracing::debug!(
                    hostname = %sni_hostname,
                    cert_domain = %cert_info.domain,
                    "Found certificate for hostname"
                );

                match self.get_or_load_cert(&cert_info.cert_path, &cert_info.key_path) {
                    Ok(cached) => {
                        if let Err(e) = ssl.set_certificate(&cached.cert) {
                            tracing::error!(
                                hostname = %sni_hostname,
                                "Failed to set certificate: {}", e
                            );
                        }
                        for intermediate in &cached.chain {
                            if let Err(e) = ssl.add_chain_cert(intermediate.clone()) {
                                tracing::error!(hostname = %sni_hostname, "Failed to add intermediate cert: {}", e);
                            }
                        }
                        if let Err(e) = ssl.set_private_key(&cached.key) {
                            tracing::error!(
                                hostname = %sni_hostname,
                                "Failed to set private key: {}", e
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            hostname = %sni_hostname,
                            cert_path = ?cert_info.cert_path,
                            "Failed to load certificate: {}", e
                        );
                    }
                }
            }
            None => {
                crate::metrics::record_tls_handshake_failure("cert_missing");
                if let Some(suppressed) = UNKNOWN_HOST_LIMITER.check() {
                    if suppressed > 0 {
                        tracing::warn!(
                            hostname = %sni_hostname,
                            suppressed,
                            "No certificate found for hostname, TLS handshake will fail (repeated)"
                        );
                    } else {
                        tracing::warn!(
                            hostname = %sni_hostname,
                            "No certificate found for hostname, TLS handshake will fail"
                        );
                    }
                }
                // No fallback cert — let the handshake fail so misconfigurations
                // are immediately obvious rather than silently serving a mismatched cert.
            }
        }
    }
}

/// Create TLS callbacks for SNI-based certificate selection
pub fn create_sni_callbacks(cert_manager: Arc<CertManager>) -> Box<dyn TlsAccept + Send + Sync> {
    Box::new(SniCertResolver::new(cert_manager))
}

fn should_allow_default_cert_fallback_for_missing_sni() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cert_fallback_for_missing_sni_is_enabled() {
        assert!(should_allow_default_cert_fallback_for_missing_sni());
    }
}

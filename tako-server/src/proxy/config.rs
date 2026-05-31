use ipnet::IpNet;
use pingora_cache::MemCache;
use pingora_cache::eviction::simple_lru;
use pingora_cache::lock::{CacheKeyLockImpl, CacheLock};
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

/// Proxy configuration
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub http_port: u16,
    pub https_port: u16,
    pub enable_https: bool,
    pub dev_mode: bool,
    pub cert_dir: PathBuf,
    pub redirect_http_to_https: bool,
    pub response_cache: Option<ResponseCacheConfig>,
    pub metrics_port: Option<u16>,
    pub trusted_proxy: TrustedProxyConfig,
}

/// Upstream response cache configuration
#[derive(Debug, Clone)]
pub struct ResponseCacheConfig {
    pub max_size_bytes: usize,
    pub max_file_size_bytes: usize,
    pub lock_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrustedProxyConfig {
    pub proxy_protocol: bool,
    pub trusted_cidrs: Vec<IpNet>,
    pub client_ip_headers: Vec<TrustedClientIpHeader>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustedClientIpHeader {
    CfConnectingIp,
    Forwarded,
    XForwardedFor,
}

impl TrustedClientIpHeader {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CfConnectingIp => "cf-connecting-ip",
            Self::Forwarded => "forwarded",
            Self::XForwardedFor => "x-forwarded-for",
        }
    }
}

impl FromStr for TrustedClientIpHeader {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cf-connecting-ip" => Ok(Self::CfConnectingIp),
            "forwarded" => Ok(Self::Forwarded),
            "x-forwarded-for" => Ok(Self::XForwardedFor),
            other => Err(format!("Unsupported trusted client IP header '{other}'")),
        }
    }
}

impl TrustedProxyConfig {
    pub fn from_raw(
        proxy_protocol: bool,
        trusted_cidrs: &[String],
        client_ip_headers: &[String],
    ) -> Result<Self, String> {
        let trusted_cidrs = trusted_cidrs
            .iter()
            .map(|cidr| {
                cidr.parse::<IpNet>()
                    .map_err(|e| format!("Invalid trusted proxy CIDR '{cidr}': {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let client_ip_headers = client_ip_headers
            .iter()
            .map(|header| header.parse())
            .collect::<Result<Vec<_>, _>>()?;
        if (proxy_protocol || !client_ip_headers.is_empty()) && trusted_cidrs.is_empty() {
            return Err(
                "trusted_proxy.trusted_cidrs is required when trusting proxy source IP metadata"
                    .to_string(),
            );
        }

        Ok(Self {
            proxy_protocol,
            trusted_cidrs,
            client_ip_headers,
        })
    }

    pub fn trusts_proxy_ip(&self, ip: &IpAddr) -> bool {
        self.trusted_cidrs.iter().any(|cidr| cidr.contains(ip))
    }
}

impl Default for ResponseCacheConfig {
    fn default() -> Self {
        Self {
            max_size_bytes: 256 * 1024 * 1024,
            max_file_size_bytes: 8 * 1024 * 1024,
            lock_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct ResponseCacheRuntime {
    pub(super) storage: &'static MemCache,
    pub(super) eviction: &'static simple_lru::Manager,
    pub(super) cache_lock: &'static CacheKeyLockImpl,
    pub(super) max_file_size_bytes: usize,
}

impl ResponseCacheRuntime {
    pub(super) fn new(config: &ResponseCacheConfig) -> Self {
        let storage = Box::leak(Box::new(MemCache::new()));
        let eviction = Box::leak(Box::new(simple_lru::Manager::new(config.max_size_bytes)));
        let cache_lock = Box::leak(Box::new(CacheLock::new(config.lock_timeout)));
        let cache_lock: &'static CacheKeyLockImpl = cache_lock;
        Self {
            storage,
            eviction,
            cache_lock,
            max_file_size_bytes: config.max_file_size_bytes,
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            http_port: 80,
            https_port: 443,
            enable_https: true,
            dev_mode: false,
            cert_dir: PathBuf::from("/opt/tako/certs"),
            redirect_http_to_https: true,
            response_cache: None,
            metrics_port: Some(9898),
            trusted_proxy: TrustedProxyConfig::default(),
        }
    }
}

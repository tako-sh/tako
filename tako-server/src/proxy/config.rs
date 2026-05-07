use pingora_cache::MemCache;
use pingora_cache::eviction::simple_lru;
use pingora_cache::lock::{CacheKeyLockImpl, CacheLock};
use std::path::PathBuf;
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
}

/// Upstream response cache configuration
#[derive(Debug, Clone)]
pub struct ResponseCacheConfig {
    pub max_size_bytes: usize,
    pub max_file_size_bytes: usize,
    pub lock_timeout: Duration,
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
            response_cache: Some(ResponseCacheConfig::default()),
            metrics_port: Some(9898),
        }
    }
}

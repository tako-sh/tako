//! HTTP/HTTPS Proxy using Pingora
//!
//! Routes incoming HTTP requests to app instances based on Host header.
//! Supports TLS termination with automatic certificate management.
//! Handles ACME HTTP-01 challenges for Let's Encrypt certificate issuance.

mod cloudflare_ips;
mod config;
mod limits;
mod proxy_protocol;
mod proxy_protocol_service;
mod request;
mod server;
mod service;
mod static_files;

pub(crate) use cloudflare_ips::CloudflareIpRanges;
pub use config::{ProxyConfig, ResponseCacheConfig, TrustedClientIpHeader, TrustedProxyConfig};
pub use server::build_server_with_acme;
#[allow(unused_imports)]
pub use static_files::*;

use crate::channels::{ChannelStore, registry::ChannelRegistry};
use crate::lb::LoadBalancer;
use crate::routing::RouteTable;
use crate::scaling::ColdStartManager;
use crate::tls::{ChallengeHandler, ChallengeTokens};
use config::ResponseCacheRuntime;
use limits::IpRequestTracker;
use parking_lot::RwLock as SyncRwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[cfg(test)]
use pingora_http::{RequestHeader, ResponseHeader};
#[cfg(test)]
use pingora_proxy::ProxyHttp;
#[cfg(test)]
use request::{
    build_proxy_cache_key, insert_body_headers, is_effective_request_https,
    path_looks_like_static_asset, production_error_body, request_is_proxy_cacheable,
    response_cacheability, should_assume_forwarded_private_request_https,
    should_redirect_http_request, static_lookup_paths,
};
#[cfg(test)]
use service::BackendResolution;

pub(crate) use limits::MAX_REQUEST_BODY_BYTES;

/// Tako HTTP proxy service
pub struct TakoProxy {
    /// Load balancer
    lb: Arc<LoadBalancer>,
    /// Route table (app_name -> route patterns)
    routes: Arc<RwLock<RouteTable>>,
    /// Configuration
    config: ProxyConfig,
    /// ACME challenge handler (optional)
    challenge_handler: Option<ChallengeHandler>,

    /// Cold start coordinator for on-demand apps
    cold_start: Arc<ColdStartManager>,
    /// Shared upstream response cache runtime (optional)
    response_cache: Option<ResponseCacheRuntime>,
    /// Reused per-app static file server state for hot path requests
    static_servers: SyncRwLock<HashMap<String, Arc<AppStaticServer>>>,
    /// Reused per-app channel stores. Keyed by app name; opened lazily
    /// the first time an app's channel route is hit and dropped when the
    /// app is unregistered.
    channel_stores: SyncRwLock<HashMap<String, Arc<ChannelStore>>>,
    /// Per-IP concurrent request limiter (DDoS mitigation)
    ip_tracker: IpRequestTracker,
    /// Cloudflare proxy CIDRs used for app-level source IP modes.
    cloudflare_ips: CloudflareIpRanges,
    /// Channel metadata cache hydrated from app internal endpoints.
    channel_registry: ChannelRegistry,
}

impl TakoProxy {
    pub fn new(
        lb: Arc<LoadBalancer>,
        routes: Arc<RwLock<RouteTable>>,
        config: ProxyConfig,
        cold_start: Arc<ColdStartManager>,
        cloudflare_ips: CloudflareIpRanges,
    ) -> Self {
        let response_cache = config
            .response_cache
            .as_ref()
            .map(ResponseCacheRuntime::new);
        Self {
            lb,
            routes,
            config,
            challenge_handler: None,
            cold_start,
            response_cache,
            static_servers: SyncRwLock::new(HashMap::new()),
            channel_stores: SyncRwLock::new(HashMap::new()),
            ip_tracker: IpRequestTracker::new(),
            cloudflare_ips,
            channel_registry: ChannelRegistry::new(),
        }
    }

    /// Create proxy with ACME challenge handling
    pub fn with_acme(
        lb: Arc<LoadBalancer>,
        routes: Arc<RwLock<RouteTable>>,
        config: ProxyConfig,
        tokens: ChallengeTokens,
        cold_start: Arc<ColdStartManager>,
        cloudflare_ips: CloudflareIpRanges,
    ) -> Self {
        let response_cache = config
            .response_cache
            .as_ref()
            .map(ResponseCacheRuntime::new);
        Self {
            lb,
            routes,
            config,
            challenge_handler: Some(ChallengeHandler::new(tokens)),
            cold_start,
            response_cache,
            static_servers: SyncRwLock::new(HashMap::new()),
            channel_stores: SyncRwLock::new(HashMap::new()),
            ip_tracker: IpRequestTracker::new(),
            cloudflare_ips,
            channel_registry: ChannelRegistry::new(),
        }
    }
}

#[cfg(test)]
mod tests;

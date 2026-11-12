mod backend;
mod channels;
#[cfg(test)]
mod drain_tests;
mod image;
mod native;
mod policy;
mod static_handler;

pub(crate) use backend::{BackendResolution, resolve_app_backend};
pub(crate) use native::invoke_network_backend;

use super::TakoProxy;
use super::compression::ResponseCompression;
use super::observe::RequestObservation;
use super::request::{
    ClientIpResolution, ForwardedHeaderTrust, build_proxy_cache_key, client_ip_for_source_ip_mode,
    client_ip_from_session, create_production_error_response, https_redirect_host,
    insert_body_headers, ip_header_value, is_effective_request_https, path_looks_like_static_asset,
    path_uses_tako_handler, request_host, request_is_proxy_cacheable, response_cacheability,
    should_redirect_http_request,
};
use crate::lb::Backend;
use crate::metrics::RequestTimer;
use async_trait::async_trait;
use bytes::Bytes;
use pingora_cache::{CacheKey, CacheMeta, ForcedFreshness, HitHandler, RespCacheable};
use pingora_core::prelude::*;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{FailToProxy, ProxyHttp, Session};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

impl TakoProxy {
    pub(crate) async fn load_balancer_cleanup(&self, app_name: &str) {
        self.lb.unregister_app(app_name);
        self.routes.write().remove_app_routes(app_name);
        self.static_servers.write().remove(app_name);
        self.channel_stores.write().remove(app_name);
        self.channel_credentials.write().remove(app_name);
        self.channel_registry.invalidate(app_name);
    }
}

pub struct RequestCtx {
    pub(super) request_guard: Option<crate::request_drain::RequestGuard>,
    pub(super) private_request: bool,
    pub(super) native_remote: Option<std::net::Ipv6Addr>,
    pub(super) backend: Option<Backend>,
    pub(super) backend_request_started: bool,
    /// The upstream answered `101 Switching Protocols`.
    pub(super) upgraded: bool,
    /// The started backend request is counted as upgraded on its instance.
    pub(super) backend_long_lived: bool,
    pub(super) is_https: bool,
    pub(super) matched_route_path: Option<Arc<str>>,
    pub(super) request_timer: Option<RequestTimer>,
    /// Client IP for per-IP rate limit tracking (released in logging phase)
    pub(super) client_ip: Option<IpAddr>,
    /// Accumulated request body bytes (for chunked transfer size enforcement)
    pub(super) body_bytes_received: u64,
    /// Set when the upstream request is sent; observed when response headers arrive.
    pub(super) upstream_start: Option<Instant>,
    pub(super) compression: ResponseCompression,
    pub(super) observation: RequestObservation,
}

impl RequestCtx {
    pub(super) fn start_request_metrics(&mut self, app_name: &str, enabled: bool) {
        if enabled {
            self.request_timer = Some(RequestTimer::start(app_name.to_string()));
        }
    }

    pub(super) fn start_upstream_metrics(&mut self, enabled: bool) {
        if enabled {
            self.upstream_start = Some(Instant::now());
        }
        self.observation.start_upstream();
    }

    pub(super) fn mark_backend_request_started(&mut self) {
        if self.backend_request_started {
            return;
        }

        if let Some(ref backend) = self.backend {
            backend.request_started();
            self.backend_request_started = true;
            self.count_backend_long_lived();
        }
    }

    /// Classify this request as long-lived for the server-wide drain only.
    pub(super) fn mark_long_lived(&self) {
        if let Some(guard) = &self.request_guard {
            guard.mark_long_lived();
        }
    }

    /// The upstream switched protocols. Only an upgraded connection stops
    /// holding its instance's drain; an Upgrade header alone, or an event
    /// stream (often finite, such as token streaming), is still waited for.
    pub(super) fn mark_upgraded(&mut self) {
        self.upgraded = true;
        self.count_backend_long_lived();
    }

    fn count_backend_long_lived(&mut self) {
        if !self.upgraded || !self.backend_request_started || self.backend_long_lived {
            return;
        }
        if let Some(ref backend) = self.backend {
            backend.request_became_long_lived();
            self.backend_long_lived = true;
        }
    }

    pub(super) fn finish_backend_request(&mut self) {
        if !self.backend_request_started {
            return;
        }

        if let Some(ref backend) = self.backend {
            if self.backend_long_lived {
                backend.long_lived_request_finished();
            } else {
                backend.request_finished();
            }
        }
        self.backend_request_started = false;
        self.backend_long_lived = false;
    }

    pub(super) fn record_backend(&mut self, backend: &Backend) {
        self.observation.set_instance(backend.instance_id());
    }
}

impl Drop for RequestCtx {
    fn drop(&mut self) {
        self.finish_backend_request();
    }
}

#[async_trait]
impl ProxyHttp for TakoProxy {
    type CTX = RequestCtx;

    fn new_ctx(&self) -> Self::CTX {
        RequestCtx {
            request_guard: None,
            private_request: false,
            native_remote: None,
            backend: None,
            backend_request_started: false,
            upgraded: false,
            backend_long_lived: false,
            is_https: false,
            matched_route_path: None,
            request_timer: None,
            client_ip: None,
            body_bytes_received: 0,
            upstream_start: None,
            compression: ResponseCompression::new(),
            observation: RequestObservation::new(),
        }
    }

    fn init_downstream_modules(&self, _modules: &mut pingora_core::modules::http::HttpModules) {}

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        self.filter_app_request(session, ctx).await
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(data) = body {
            ctx.body_bytes_received += data.len() as u64;
            if ctx.body_bytes_received > super::MAX_REQUEST_BODY_BYTES {
                return Err(Error::explain(
                    ErrorType::InvalidHTTPHeader,
                    "Request body exceeds maximum allowed size",
                ));
            }
        }
        Ok(())
    }

    fn request_cache_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<()> {
        if ctx.private_request {
            return Ok(());
        }
        let Some(cache) = self.response_cache else {
            return Ok(());
        };

        if !request_is_proxy_cacheable(session.req_header()) {
            return Ok(());
        }

        session.cache.enable(
            cache.storage,
            Some(cache.eviction),
            None,
            Some(cache.cache_lock),
            None,
        );
        session
            .cache
            .set_max_file_size_bytes(cache.max_file_size_bytes);

        Ok(())
    }

    fn cache_miss(&self, session: &mut Session, ctx: &mut Self::CTX) {
        session.cache.cache_miss();
        ctx.observation.set_cache_result("miss");
    }

    async fn cache_hit_filter(
        &self,
        _session: &mut Session,
        _meta: &CacheMeta,
        _hit_handler: &mut HitHandler,
        is_fresh: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<ForcedFreshness>>
    where
        Self::CTX: Send + Sync,
    {
        if is_fresh {
            ctx.observation.set_cache_result("hit");
        } else {
            ctx.observation.set_cache_result("stale");
        }
        Ok(None)
    }

    fn cache_key_callback(&self, session: &Session, _ctx: &mut Self::CTX) -> Result<CacheKey> {
        let host = request_host(session.req_header());
        Ok(build_proxy_cache_key(
            host,
            &session.req_header().uri.to_string(),
        ))
    }

    fn response_cache_filter(
        &self,
        session: &Session,
        resp: &ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<RespCacheable> {
        if self.response_cache.is_none() {
            return Ok(RespCacheable::Uncacheable(
                pingora_cache::NoCacheReason::Custom("proxy_cache_disabled"),
            ));
        }

        let authorization_present = session.req_header().headers.contains_key("authorization");
        Ok(response_cacheability(resp, authorization_present))
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        if let Some(address) = ctx.native_remote {
            let local = self
                .native_services
                .as_ref()
                .ok_or_else(|| Error::new(ErrorType::ConnectNoRoute))?
                .address;
            let mut peer = HttpPeer::new(
                std::net::SocketAddr::from((address, 80)),
                false,
                String::new(),
            );
            let mut bind = pingora_core::connectors::l4::BindTo::default();
            bind.addr = Some((local, 0).into());
            peer.options.bind_to = Some(bind);
            peer.options.connection_timeout = Some(Duration::from_secs(5));
            peer.options.read_timeout = Some(Duration::from_secs(60));
            peer.options.write_timeout = Some(Duration::from_secs(30));
            return Ok(Box::new(peer));
        }
        // ctx.is_https was already computed in request_filter

        let backend = ctx
            .backend
            .as_ref()
            .ok_or_else(|| Error::new(ErrorType::ConnectNoRoute))?;

        let mut peer = if let Some(endpoint) = backend.endpoint() {
            HttpPeer::new(endpoint, false, String::new())
        } else {
            return Err(Error::explain(
                ErrorType::ConnectNoRoute,
                format!(
                    "Missing upstream endpoint for app '{}' instance {}",
                    backend.app_name,
                    backend.instance_id()
                ),
            ));
        };

        peer.options.connection_timeout = Some(Duration::from_secs(5));
        peer.options.read_timeout = Some(Duration::from_secs(60));
        peer.options.write_timeout = Some(Duration::from_secs(30));

        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let proto = if ctx.is_https { "https" } else { "http" };
        upstream_request
            .insert_header("X-Forwarded-Proto", proto)
            .unwrap();
        upstream_request
            .insert_header("X-Request-ID", ctx.observation.request_id())
            .unwrap();

        if let Some(ip) = ctx.client_ip {
            upstream_request
                .insert_header("X-Forwarded-For", ip_header_value(ip))
                .unwrap();
        } else {
            let _ = upstream_request.remove_header("X-Forwarded-For");
        }

        let _ = upstream_request.remove_header("Forwarded");
        let _ = upstream_request.remove_header("X-Tako-Internal-Token");
        native::strip_credentials(upstream_request);

        ctx.mark_backend_request_started();
        if self.draining.load(std::sync::atomic::Ordering::Acquire) {
            ctx.finish_backend_request();
            return Err(Error::explain(
                ErrorType::HTTPStatus(503),
                "Server is draining",
            ));
        }

        ctx.start_upstream_metrics(self.metrics_enabled());

        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let upgraded = upstream_response.status.as_u16() == 101;
        if upgraded || native::is_event_stream(upstream_response) {
            ctx.mark_long_lived();
        }
        if upgraded {
            ctx.mark_upgraded();
        }
        upstream_response.insert_header("X-Request-ID", ctx.observation.request_id())?;
        ctx.observation.finish_upstream_response();
        if let (Some(start), Some(backend)) = (ctx.upstream_start.take(), ctx.backend.as_ref()) {
            crate::metrics::record_upstream_duration(
                &backend.app_name,
                start.elapsed().as_secs_f64(),
            );
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if ctx.backend.is_some() {
            ctx.compression
                .prepare(session.req_header(), upstream_response)?;
        }
        Ok(())
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        ctx.compression.filter_body(body, end_of_stream)
    }

    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        e: &Error,
        _ctx: &mut Self::CTX,
    ) -> FailToProxy {
        let status = match e.etype() {
            ErrorType::HTTPStatus(code) => *code,
            _ => match e.esource() {
                ErrorSource::Downstream => match e.etype() {
                    ErrorType::WriteError | ErrorType::ReadError | ErrorType::ConnectionClosed => 0,
                    _ => 400,
                },
                ErrorSource::Upstream => 502,
                ErrorSource::Internal | ErrorSource::Unset => 500,
            },
        };

        if status >= 500 {
            if let Err(error) = create_production_error_response(session, status).await {
                tracing::error!("failed to send production {status} error response: {error}");
            }
        } else if status > 0
            && let Err(error) = session.respond_error(status).await
        {
            tracing::error!("failed to send error response to downstream: {error}");
        }

        FailToProxy {
            error_code: status,
            can_reuse_downstream: false,
        }
    }

    async fn logging(&self, session: &mut Session, _e: Option<&Error>, ctx: &mut Self::CTX) {
        if let Some(ip) = ctx.client_ip.take() {
            self.ip_tracker.release(ip);
        }

        ctx.finish_backend_request();

        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);

        if let Some(timer) = ctx.request_timer.take() {
            timer.finish(status);
        }

        let host = request_host(session.req_header());
        let host = if host.is_empty() { "-" } else { host };

        let path = session.req_header().uri.path();
        let method = session.req_header().method.as_str();
        let app = ctx.observation.app();
        let instance = ctx.observation.instance();

        tracing::info!(
            app = app,
            source = "proxy",
            request_id = ctx.observation.request_id(),
            host = host,
            method = method,
            path = path,
            route = ctx.observation.route(),
            instance = instance,
            status = status,
            https = ctx.is_https,
            handler = ctx.observation.handler(),
            handler_result = ctx.observation.handler_result(),
            cache = ctx.observation.cache_result(),
            total_ms = ctx.observation.total_ms(),
            route_lookup_ms = ctx.observation.route_lookup_ms(),
            cold_start_wait_ms = ctx.observation.cold_start_wait_ms(),
            upstream_response_ms = ctx.observation.upstream_response_ms(),
            compression_algorithm = ctx.compression.algorithm_log_value(),
            compression_skip_reason = ctx.compression.skip_reason_log_value(),
            compression_vary_required = ctx.compression.vary_required(),
            compression_uncompressed_bytes = ctx.compression.uncompressed_bytes(),
            compression_compressed_bytes = ctx.compression.compressed_bytes(),
            "Request completed"
        );
    }
}

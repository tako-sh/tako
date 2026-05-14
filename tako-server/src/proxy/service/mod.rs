mod backend;
mod channels;
mod image;
mod static_handler;

pub(crate) use backend::BackendResolution;

use super::TakoProxy;
use super::request::{
    build_proxy_cache_key, client_ip_from_session, client_ip_from_trusted_headers,
    create_production_error_response, https_redirect_host, insert_body_headers,
    is_effective_request_https, path_looks_like_static_asset, request_host,
    request_is_proxy_cacheable, response_cacheability,
    should_assume_forwarded_private_request_https, should_redirect_http_request,
};
use crate::lb::Backend;
use crate::metrics::RequestTimer;
use async_trait::async_trait;
use bytes::Bytes;
use pingora_cache::{CacheKey, RespCacheable};
use pingora_core::prelude::*;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{FailToProxy, ProxyHttp, Session};
use std::net::IpAddr;
use std::time::{Duration, Instant};

impl TakoProxy {
    pub(crate) async fn load_balancer_cleanup(&self, app_name: &str) {
        self.lb.unregister_app(app_name);
        self.routes.write().await.remove_app_routes(app_name);
        self.static_servers.write().remove(app_name);
        self.channel_stores.write().remove(app_name);
        self.channel_registry.invalidate(app_name);
    }
}

pub struct RequestCtx {
    pub(super) backend: Option<Backend>,
    pub(super) backend_request_started: bool,
    pub(super) is_https: bool,
    pub(super) matched_route_path: Option<String>,
    pub(super) request_timer: Option<RequestTimer>,
    /// Client IP for per-IP rate limit tracking (released in logging phase)
    pub(super) client_ip: Option<IpAddr>,
    /// Accumulated request body bytes (for chunked transfer size enforcement)
    pub(super) body_bytes_received: u64,
    /// Set when the upstream request is sent; observed when response headers arrive.
    pub(super) upstream_start: Option<Instant>,
}

impl RequestCtx {
    pub(super) fn mark_backend_request_started(&mut self) {
        if let Some(ref backend) = self.backend {
            backend.request_started();
            self.backend_request_started = true;
        }
    }

    pub(super) fn finish_backend_request(&mut self) {
        if !self.backend_request_started {
            return;
        }

        if let Some(ref backend) = self.backend {
            backend.request_finished();
        }
        self.backend_request_started = false;
    }
}

#[async_trait]
impl ProxyHttp for TakoProxy {
    type CTX = RequestCtx;

    fn new_ctx(&self) -> Self::CTX {
        RequestCtx {
            backend: None,
            backend_request_started: false,
            is_https: false,
            matched_route_path: None,
            request_timer: None,
            client_ip: None,
            body_bytes_received: 0,
            upstream_start: None,
        }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        if let Some(peer_ip) = client_ip_from_session(session) {
            let ip = client_ip_from_trusted_headers(
                session.req_header(),
                peer_ip,
                &self.config.trusted_proxy,
            )
            .unwrap_or(peer_ip);
            if !self.ip_tracker.try_acquire(ip) {
                let body = "Too Many Requests";
                let mut header = ResponseHeader::build(429, None)?;
                header.insert_header("Retry-After", "1")?;
                insert_body_headers(&mut header, "text/plain", body)?;
                session
                    .write_response_header(Box::new(header), false)
                    .await?;
                session.write_response_body(Some(body.into()), true).await?;
                return Ok(true);
            }
            ctx.client_ip = Some(ip);
        }

        if let Some(cl) = session
            .req_header()
            .headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            && cl > super::MAX_REQUEST_BODY_BYTES
        {
            let body = "Payload Too Large";
            let mut header = ResponseHeader::build(413, None)?;
            insert_body_headers(&mut header, "text/plain", body)?;
            session
                .write_response_header(Box::new(header), false)
                .await?;
            session.write_response_body(Some(body.into()), true).await?;
            return Ok(true);
        }

        let path = session.req_header().uri.path().to_string();
        let host = request_host(session.req_header()).to_string();
        let hostname = host.split(':').next().unwrap_or(&host);

        if let Some(ref handler) = self.challenge_handler
            && handler.is_challenge_request(&path)
        {
            if let Some(response) = handler.handle_challenge(&path) {
                tracing::info!(path = %path, "Serving ACME challenge response");
                let mut header = ResponseHeader::build(200, None)?;
                insert_body_headers(&mut header, "text/plain", &response)?;
                session
                    .write_response_header(Box::new(header), false)
                    .await?;
                session
                    .write_response_body(Some(response.into()), true)
                    .await?;
                return Ok(true);
            } else {
                tracing::warn!(path = %path, "ACME challenge token not found");
                let body = "Token not found";
                let mut header = ResponseHeader::build(404, None)?;
                insert_body_headers(&mut header, "text/plain", body)?;
                session
                    .write_response_header(Box::new(header), false)
                    .await?;
                session.write_response_body(Some(body.into()), true).await?;
                return Ok(true);
            }
        }

        if !path.starts_with("/.well-known/acme-challenge/") {
            let transport_https = session
                .digest()
                .map(|d| d.ssl_digest.is_some())
                .unwrap_or(false);
            let request_headers = &session.req_header().headers;
            let x_forwarded_for = request_headers
                .get("x-forwarded-for")
                .and_then(|h| h.to_str().ok());
            let x_forwarded_proto = request_headers
                .get("x-forwarded-proto")
                .and_then(|h| h.to_str().ok());
            let forwarded = request_headers
                .get("forwarded")
                .and_then(|h| h.to_str().ok());
            let is_effective_https =
                is_effective_request_https(transport_https, x_forwarded_proto, forwarded)
                    || should_assume_forwarded_private_request_https(
                        hostname,
                        x_forwarded_for,
                        x_forwarded_proto,
                        forwarded,
                    );
            ctx.is_https = is_effective_https;

            if should_redirect_http_request(is_effective_https, self.config.redirect_http_to_https)
            {
                let path_and_query = session
                    .req_header()
                    .uri
                    .path_and_query()
                    .map(|pq| pq.as_str())
                    .unwrap_or(&path);
                let redirect_host = https_redirect_host(&host, self.config.https_port);
                let redirect_url = format!("https://{}{}", redirect_host, path_and_query);
                let body = "Redirecting to HTTPS";

                let mut header = ResponseHeader::build(307, None)?;
                header.insert_header("Location", &redirect_url)?;
                header.insert_header("Cache-Control", "no-store")?;
                insert_body_headers(&mut header, "text/plain", body)?;
                session
                    .write_response_header(Box::new(header), false)
                    .await?;
                session.write_response_body(Some(body.into()), true).await?;
                return Ok(true);
            }
        }

        let route_match = match self.routes.read().await.select_with_route(hostname, &path) {
            Some(route_match) => route_match,
            None => {
                let body = "Not Found";
                let mut header = ResponseHeader::build(404, None)?;
                insert_body_headers(&mut header, "text/plain", body)?;
                session
                    .write_response_header(Box::new(header), false)
                    .await?;
                session.write_response_body(Some(body.into()), true).await?;
                return Ok(true);
            }
        };
        let app_name = route_match.app;
        ctx.matched_route_path = route_match.path;

        let matched_route_path = ctx.matched_route_path.clone();
        if self
            .try_handle_image_request(
                session,
                ctx,
                &app_name,
                &path,
                &host,
                matched_route_path.as_deref(),
            )
            .await?
        {
            return Ok(true);
        }

        if self
            .try_handle_channel_request(session, ctx, &app_name, &path, &host)
            .await?
        {
            return Ok(true);
        }

        if path_looks_like_static_asset(&path)
            && self
                .try_serve_static_asset(
                    session,
                    &app_name,
                    &path,
                    ctx.matched_route_path.as_deref(),
                )
                .await?
        {
            return Ok(true);
        }

        let backend = match self.resolve_backend(&app_name).await {
            BackendResolution::Ready(backend) => backend,
            BackendResolution::StartupTimeout => {
                tracing::warn!(app = %app_name, "App startup timed out");
                return create_production_error_response(session, 504).await;
            }
            BackendResolution::StartupFailed => {
                tracing::warn!(app = %app_name, "App failed to start");
                return create_production_error_response(session, 502).await;
            }
            BackendResolution::QueueFull => {
                tracing::warn!(app = %app_name, "App startup queue is full");
                return create_production_error_response(session, 503).await;
            }
            BackendResolution::Unavailable => {
                tracing::warn!(app = %app_name, "No healthy backend");
                return create_production_error_response(session, 503).await;
            }
            BackendResolution::AppMissing => {
                self.load_balancer_cleanup(&app_name).await;
                let body = "Not Found";
                let mut header = ResponseHeader::build(404, None)?;
                insert_body_headers(&mut header, "text/plain", body)?;
                session
                    .write_response_header(Box::new(header), false)
                    .await?;
                session.write_response_body(Some(body.into()), true).await?;
                return Ok(true);
            }
        };

        ctx.request_timer = Some(RequestTimer::start(app_name));
        ctx.backend = Some(backend);

        Ok(false)
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

    fn request_cache_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<()> {
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

        if let Some(ip) = ctx.client_ip {
            upstream_request
                .insert_header("X-Forwarded-For", ip.to_string())
                .unwrap();
        } else {
            let _ = upstream_request.remove_header("X-Forwarded-For");
        }

        let _ = upstream_request.remove_header("Forwarded");
        let _ = upstream_request.remove_header("X-Tako-Internal-Token");

        ctx.mark_backend_request_started();

        ctx.upstream_start = Some(Instant::now());

        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
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
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        Ok(())
    }

    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        Ok(None)
    }

    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        e: &Error,
        _ctx: &mut Self::CTX,
    ) -> FailToProxy {
        let status = match e.etype() {
            ErrorType::HTTPStatus(code) if *code < 500 => *code,
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

        tracing::debug!(
            host = host,
            method = method,
            path = path,
            status = status,
            https = ctx.is_https,
            "Request completed"
        );
    }
}

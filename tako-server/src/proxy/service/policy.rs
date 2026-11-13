use super::*;

impl TakoProxy {
    pub(super) async fn filter_app_request(
        &self,
        session: &mut Session,
        ctx: &mut RequestCtx,
    ) -> Result<bool> {
        ctx.observation.initialize_request_id(session.req_header());

        if let Some(control) = &self.native_control
            && control.matches_host(request_host(session.req_header()))
        {
            ctx.observation.set_handler("network", "control");
            return control.handle(session).await;
        }
        if self.draining.load(std::sync::atomic::Ordering::Acquire) {
            return native::reject(session, 503).await;
        }

        let socket = match native::duplicate_socket(session) {
            Ok(socket) => socket,
            Err(error) => {
                tracing::warn!(%error, "Could not retain application transport for drain");
                return native::reject(session, 503).await;
            }
        };
        let Some(guard) = self.proxy_requests.register(socket) else {
            return native::reject(session, 503).await;
        };
        if session.req_header().headers.contains_key("upgrade") {
            guard.mark_long_lived();
        }
        ctx.request_guard = Some(guard);
        if self.draining.load(std::sync::atomic::Ordering::Acquire) {
            return native::reject(session, 503).await;
        }

        if let Some(cl) = session
            .req_header()
            .headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            && cl > super::super::MAX_REQUEST_BODY_BYTES
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

        let private_app = match self.native_route(session) {
            crate::native_services::PrivateRoute::Public => None,
            crate::native_services::PrivateRoute::Forbidden => {
                return native::reject(session, 403).await;
            }
            crate::native_services::PrivateRoute::Unavailable => {
                return native::reject(session, 503).await;
            }
            crate::native_services::PrivateRoute::Local(app) => {
                ctx.private_request = true;
                native::strip_credentials(session.req_header_mut());
                Some(app.to_string())
            }
            crate::native_services::PrivateRoute::Remote { app, address } => {
                // App invocations are signed for one destination server. Ordinary
                // private HTTP may route through another server; proofs may not.
                if session.req_header().uri.path() == tako_core::network::NETWORK_HTTP_PATH {
                    return native::reject(session, 403).await;
                }
                ctx.private_request = true;
                ctx.native_remote = Some(address);
                native::strip_credentials(session.req_header_mut());
                let peer = native::socket_addresses(session).1;
                if let Some(peer) = peer {
                    if !self.ip_tracker.try_acquire(peer) {
                        return native::reject(session, 429).await;
                    }
                    ctx.client_ip = Some(peer);
                }
                ctx.observation.set_app_route(&app.to_string(), None);
                ctx.observation.set_handler("network", "remote_service");
                ctx.start_request_metrics(&app.to_string(), self.metrics_enabled());
                return Ok(false);
            }
        };
        let path = session.req_header().uri.path();
        if path == tako_core::network::NETWORK_HTTP_PATH && !ctx.private_request {
            return native::reject(session, 403).await;
        }
        let host = request_host(session.req_header());
        let hostname = host.split(':').next().unwrap_or(host);

        let route_lookup_started_at = Instant::now();
        let route_match = private_app
            .map(|app| crate::routing::SelectedRoute {
                app: app.into(),
                path: None,
                source_ip: tako_core::SourceIpMode::Direct,
            })
            .or_else(|| self.routes.read().select_with_route(hostname, path));
        ctx.observation
            .set_route_lookup_elapsed(route_lookup_started_at.elapsed());

        if let Some(route_match) = route_match.as_ref() {
            ctx.observation
                .set_app_route(&route_match.app, route_match.path.as_deref());
            let resolution = match client_ip_from_session(session) {
                Some(peer_ip) => Some(client_ip_for_source_ip_mode(
                    session.req_header(),
                    peer_ip,
                    route_match.source_ip,
                    &self.cloudflare_ips,
                    &self.config.trusted_proxy,
                )),
                None => match route_match.source_ip {
                    tako_core::SourceIpMode::CloudflareProxy => {
                        Some(ClientIpResolution::RejectCloudflareProxy)
                    }
                    tako_core::SourceIpMode::TrustedProxy => {
                        Some(ClientIpResolution::RejectTrustedProxy)
                    }
                    _ => None,
                },
            };

            if let Some(resolution) = resolution {
                let ip = match resolution {
                    ClientIpResolution::Accepted(ip) => ip,
                    ClientIpResolution::RejectCloudflareProxy
                    | ClientIpResolution::RejectTrustedProxy => {
                        ctx.observation.set_handler("proxy", "forbidden");
                        let body = "Forbidden";
                        let mut header = ResponseHeader::build(403, None)?;
                        insert_body_headers(&mut header, "text/plain", body)?;
                        session
                            .write_response_header(Box::new(header), false)
                            .await?;
                        session.write_response_body(Some(body.into()), true).await?;
                        return Ok(true);
                    }
                };
                if !self.ip_tracker.try_acquire(ip) {
                    ctx.observation.set_handler("proxy", "rate_limited");
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
        }

        if !ctx.private_request
            && let Some(ref handler) = self.challenge_handler
            && handler.is_challenge_request(path)
        {
            if let Some(response) = handler.handle_challenge(path) {
                ctx.observation.set_handler("acme", "served");
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
                ctx.observation.set_handler("acme", "not_found");
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

        if !ctx.private_request && !path.starts_with("/.well-known/acme-challenge/") {
            let transport_https = session
                .digest()
                .map(|d| d.ssl_digest.is_some())
                .unwrap_or(false);
            let is_effective_https = if transport_https {
                true
            } else {
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
                let peer_ip = client_ip_from_session(session);
                is_effective_request_https(
                    false,
                    hostname,
                    x_forwarded_for,
                    x_forwarded_proto,
                    forwarded,
                    ForwardedHeaderTrust {
                        peer_ip,
                        cloudflare_ips: &self.cloudflare_ips,
                        trusted_proxy: &self.config.trusted_proxy,
                    },
                )
            };
            ctx.is_https = is_effective_https;

            if should_redirect_http_request(is_effective_https, self.config.redirect_http_to_https)
            {
                ctx.observation.set_handler("redirect", "https");
                let path_and_query = session
                    .req_header()
                    .uri
                    .path_and_query()
                    .map(|pq| pq.as_str())
                    .unwrap_or(path);
                let redirect_host = https_redirect_host(host, self.config.https_port);
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

        let route_match = match route_match {
            Some(route_match) => route_match,
            None => {
                ctx.observation.set_handler("proxy", "not_found");
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
        ctx.observation
            .set_app_route(&app_name, ctx.matched_route_path.as_deref());

        if path == tako_core::network::NETWORK_HTTP_PATH {
            return self.handle_network_request(session, &app_name).await;
        }

        if path.starts_with(tako_storage::local_http::PREFIX) {
            let Some(app) = self.lb.app_manager().get_app(&app_name) else {
                return native::reject(session, 404).await;
            };
            // Restore swaps the data tree under this gate. Finish each storage
            // operation before that swap, even when no app worker is running.
            let _storage = app.lifecycle_gate.read().await;
            let bindings = app.config.read().storages.clone();
            let paths =
                crate::release::app_runtime_data_paths(self.lb.app_manager().data_dir(), &app_name);
            ctx.observation.set_handler("storage", "local");
            ctx.start_request_metrics(&app_name, self.metrics_enabled());
            return tako_storage::local_http::serve(
                session,
                &bindings,
                &paths.tako.join("storages/objects"),
                &paths.app,
            )
            .await;
        }

        if path_uses_tako_handler(path) {
            let path = path.to_string();
            let host = host.to_string();
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

            if path_looks_like_static_asset(&path) {
                let matched_route_path = ctx.matched_route_path.clone();
                if self
                    .try_serve_static_asset(
                        session,
                        ctx,
                        &app_name,
                        &path,
                        matched_route_path.as_deref(),
                    )
                    .await?
                {
                    return Ok(true);
                }
            }
        }

        let backend = match self.resolve_backend(&app_name).await {
            BackendResolution::Ready {
                backend,
                cold_start_wait,
            } => {
                if let Some(cold_start_wait) = cold_start_wait {
                    ctx.observation.set_cold_start_wait(cold_start_wait);
                }
                ctx.record_backend(&backend);
                ctx.observation.set_handler("proxy", "upstream");
                backend
            }
            BackendResolution::StartupTimeout => {
                if let Some(elapsed) = self.cold_start.elapsed(&app_name) {
                    ctx.observation.set_cold_start_wait(elapsed);
                }
                ctx.observation.set_handler("proxy", "startup_timeout");
                tracing::warn!(app = %app_name, "App startup timed out");
                return create_production_error_response(session, 504).await;
            }
            BackendResolution::StartupFailed => {
                if let Some(elapsed) = self.cold_start.elapsed(&app_name) {
                    ctx.observation.set_cold_start_wait(elapsed);
                }
                ctx.observation.set_handler("proxy", "startup_failed");
                tracing::warn!(app = %app_name, "App failed to start");
                return create_production_error_response(session, 502).await;
            }
            BackendResolution::QueueFull => {
                if let Some(elapsed) = self.cold_start.elapsed(&app_name) {
                    ctx.observation.set_cold_start_wait(elapsed);
                }
                ctx.observation.set_handler("proxy", "queue_full");
                tracing::warn!(app = %app_name, "App startup queue is full");
                return create_production_error_response(session, 503).await;
            }
            BackendResolution::Unavailable => {
                ctx.observation.set_handler("proxy", "unavailable");
                tracing::warn!(app = %app_name, "No healthy backend");
                return create_production_error_response(session, 503).await;
            }
            BackendResolution::AppMissing => {
                ctx.observation.set_handler("proxy", "app_missing");
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

        ctx.start_request_metrics(&app_name, self.metrics_enabled());
        ctx.backend = Some(backend);

        Ok(false)
    }
}

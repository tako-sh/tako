mod policy;
mod registry;
mod sse;
use policy::*;

use super::{BackendResolution, RequestCtx};
use crate::channels::{
    ChannelAuthResponse, ChannelDefinitionMeta, ChannelError, ChannelOperation, ChannelStore,
    ChannelTransport, authorize_channel_request, fetch_channel_registry, parse_channel_route,
    parse_message_id_cursor, parse_replay_from, parse_ws_last_message_id,
};
use crate::native_channel_client::{ProxyChannelStore, app_scope};
use crate::proxy::TakoProxy;
use crate::proxy::request::insert_body_headers;
use bytes::Bytes;
use pingora_core::prelude::*;
use pingora_http::ResponseHeader;
use pingora_proxy::Session;
use std::sync::Arc;
use tako_channels::authorize::{
    AppAuthorizer, RequestCredentials, request_credentials, require_websocket_grant,
};
use tako_channels::live_store::{
    STORE_CHANNEL_PREFIX, load_store, parse_store_route, serve_store_events_with_writer,
    store_scope, valid_store_name, validate_store_lifecycle, validate_store_query,
};
use tako_channels::params::{ParamsError, validate_query};
use tako_channels::{ChannelStream, ReplayCursor, ReplayFrom};
use ws::ChannelWebSocketAuth;

mod drain;
mod ws;

struct SharedAppResolver {
    app_name: String,
    lb: Arc<crate::lb::LoadBalancer>,
    cold_start: Arc<crate::scaling::ColdStartManager>,
    draining: Arc<std::sync::atomic::AtomicBool>,
    spawn_gate: Arc<tokio::sync::RwLock<()>>,
}

#[async_trait::async_trait]
impl tako_channels::shared::AppResolver for SharedAppResolver {
    async fn resolve(&self) -> std::result::Result<AppAuthorizer, ChannelError> {
        let BackendResolution::Ready { backend, .. } = super::backend::resolve_app_backend(
            &self.lb,
            &self.cold_start,
            &self.draining,
            &self.spawn_gate,
            &self.app_name,
        )
        .await
        else {
            return Err(ChannelError::AuthUnavailable);
        };
        let instance = backend.instance();
        Ok(AppAuthorizer {
            endpoint: instance
                .endpoint()
                .ok_or(ChannelError::AuthUnavailable)?
                .to_string(),
            internal_host: crate::instances::internal_app_host_for_app_id(&self.app_name),
            internal_token_header: "x-tako-internal-token",
            internal_token: instance.internal_token().to_owned(),
        })
    }
}

impl TakoProxy {
    async fn write_json_response(
        &self,
        session: &mut Session,
        status: u16,
        value: &serde_json::Value,
    ) -> Result<bool> {
        let body = serde_json::to_string(value).map_err(|e| {
            Error::explain(
                ErrorType::InternalError,
                format!("Failed to serialize channel response: {e}"),
            )
        })?;
        let mut header = ResponseHeader::build(status, None)?;
        insert_body_headers(&mut header, "application/json", &body)?;
        header.insert_header("Cache-Control", "no-store")?;
        session
            .write_response_header(Box::new(header), false)
            .await?;
        session
            .write_response_body(Some(Bytes::from(body)), true)
            .await?;
        Ok(true)
    }

    async fn write_channel_error(
        &self,
        session: &mut Session,
        error: ChannelError,
    ) -> Result<bool> {
        if let ChannelError::Storage(message) = &error {
            tracing::error!("Channel storage error: {message}");
        }
        let (status, body) = tako_channels::authorize::error_response(&error);
        self.write_json_response(session, status, &body).await
    }

    pub(crate) async fn try_handle_channel_request(
        &self,
        session: &mut Session,
        ctx: &mut RequestCtx,
        app_name: &str,
        path: &str,
        _host: &str,
    ) -> Result<bool> {
        let result = drain::until_draining(
            &self.draining,
            self.try_handle_channel_request_inner(session, ctx, app_name, path, _host),
        )
        .await;
        match result {
            Some(result) => result,
            None => {
                // Dropping the handler cancels WS/SSE and releases its presence session.
                // Close the transport itself; draining is not an application channel event.
                drain::close(session).await;
                Ok(true)
            }
        }
    }

    async fn try_handle_channel_request_inner(
        &self,
        session: &mut Session,
        ctx: &mut RequestCtx,
        app_name: &str,
        path: &str,
        _host: &str,
    ) -> Result<bool> {
        let store_name = match parse_store_route(path) {
            Ok(name) => name,
            Err(error) => return self.write_channel_error(session, error).await,
        };
        let route = match store_name.as_ref() {
            Some(name) => Ok(Some(tako_channels::ChannelRoute {
                channel: format!("{STORE_CHANNEL_PREFIX}{name}"),
            })),
            None => parse_channel_route(path),
        };
        let route = match route {
            Ok(Some(route)) => route,
            Ok(None) if path == tako_channels::shared::PATH => tako_channels::ChannelRoute {
                channel: String::new(),
            },
            Ok(None) => {
                return Ok(false);
            }
            Err(error) => {
                ctx.observation.set_handler("channel", "error");
                return self.write_channel_error(session, error).await;
            }
        };
        let is_store = route.channel.starts_with(STORE_CHANNEL_PREFIX);
        if is_store && !valid_store_name(&route.channel[STORE_CHANNEL_PREFIX.len()..]) {
            return self
                .write_channel_error(session, ChannelError::InvalidPath)
                .await;
        }
        ctx.observation
            .set_handler(if is_store { "store" } else { "channel" }, "pending");
        if is_store
            && (session.req_header().method.as_str() != "GET"
                || session.as_downstream().is_upgrade_req())
        {
            return self
                .write_json_response(
                    session,
                    405,
                    &serde_json::json!({"error":"Method not allowed"}),
                )
                .await;
        }

        let backend = match self.resolve_backend(app_name).await {
            BackendResolution::Ready {
                backend,
                cold_start_wait,
            } => {
                if let Some(cold_start_wait) = cold_start_wait {
                    ctx.observation.set_cold_start_wait(cold_start_wait);
                }
                ctx.record_backend(&backend);
                backend
            }
            BackendResolution::StartupTimeout => {
                if let Some(elapsed) = self.cold_start.elapsed(app_name) {
                    ctx.observation.set_cold_start_wait(elapsed);
                }
                ctx.observation.set_handler("channel", "startup_timeout");
                tracing::warn!(app = %app_name, "Channel auth unavailable: app startup timed out");
                return self
                    .write_channel_error(session, ChannelError::AuthUnavailable)
                    .await;
            }
            BackendResolution::StartupFailed
            | BackendResolution::QueueFull
            | BackendResolution::Unavailable
            | BackendResolution::AppMissing => {
                if let Some(elapsed) = self.cold_start.elapsed(app_name) {
                    ctx.observation.set_cold_start_wait(elapsed);
                }
                ctx.observation.set_handler("channel", "unavailable");
                tracing::warn!(app = %app_name, "Channel auth unavailable: backend unavailable");
                return self
                    .write_channel_error(session, ChannelError::AuthUnavailable)
                    .await;
            }
        };

        let instance = backend.instance();

        if path == tako_channels::shared::PATH {
            let app = Arc::new(SharedAppResolver {
                app_name: app_name.to_owned(),
                lb: self.lb.clone(),
                cold_start: self.cold_start.clone(),
                draining: self.draining.clone(),
                spawn_gate: self.spawn_gate.clone(),
            });
            let store = match self.channel_store_for_app(app_name).await {
                Ok(store) => store,
                Err(error) => {
                    return self
                        .write_channel_error(session, ChannelError::Storage(error.to_string()))
                        .await;
                }
            };
            drop(backend);
            ctx.observation.set_handler_result("shared-websocket");
            return tako_channels::shared::serve(
                session,
                app,
                store.store().clone(),
                store,
                ctx.is_https,
            )
            .await;
        }

        let operation = if session.as_downstream().is_upgrade_req() {
            ChannelOperation::Connect
        } else {
            ChannelOperation::Subscribe
        };

        let meta = match self
            .channel_meta_for_app(app_name, instance, &route.channel)
            .await
        {
            Ok(meta) => meta,
            Err(error) => {
                ctx.observation.set_handler_result("error");
                return self.write_channel_error(session, error).await;
            }
        };

        let params_query = request_params_query(session.req_header().uri.query(), is_store);
        let parsed_params = if is_store {
            validate_store_query(&meta.params_schema, &params_query)
        } else {
            validate_query(&meta.params_schema, &params_query).map_err(|error| match error {
                ParamsError::Invalid(message) => {
                    ChannelError::BadRequest(format!("invalid channel params: {message}"))
                }
                ParamsError::InvalidSchema(message) => {
                    ChannelError::Storage(format!("invalid channel schema: {message}"))
                }
            })
        };
        let params = match parsed_params {
            Ok(params) => params,
            Err(error) => return self.write_channel_error(session, error).await,
        };
        // Validated params select the stream; the app authorizes the public name.
        let stream = ChannelStream::new(&route.channel, &params);
        // `from=start` replays every retained message; a cursor still wins.
        let from_start = if is_store {
            false
        } else {
            match parse_replay_from(session.req_header().uri.query()) {
                Ok(from_start) => from_start,
                Err(error) => return self.write_channel_error(session, error).await,
            }
        };

        let is_websocket = session.as_downstream().is_upgrade_req();
        let RequestCredentials {
            header,
            cookie,
            first_frame: use_first_frame_auth,
        } = match request_credentials(session.req_header(), &meta.auth, is_websocket, ctx.is_https)
        {
            Ok(credentials) => credentials,
            Err(error) => {
                if matches!(error, ChannelError::Unauthorized) {
                    ctx.observation.set_handler_result("unauthorized");
                }
                return self.write_channel_error(session, error).await;
            }
        };

        if is_websocket {
            let presence_params = params.clone();
            if session.req_header().method.as_str() != "GET" {
                ctx.observation.set_handler_result("error");
                return self
                    .write_json_response(
                        session,
                        405,
                        &serde_json::json!({ "error": "Method not allowed" }),
                    )
                    .await;
            }
            if meta.transport != Some(ChannelTransport::Ws) {
                ctx.observation.set_handler_result("unsupported");
                return self
                    .write_channel_error(session, ChannelError::Unsupported)
                    .await;
            }
            let requested = match parse_ws_last_message_id(session.req_header().uri.query()) {
                Ok(cursor) => ReplayFrom::new(cursor, from_start),
                Err(error) => {
                    ctx.observation.set_handler_result("error");
                    return self.write_channel_error(session, error).await;
                }
            };
            let store = match self.channel_store_for_app(app_name).await {
                Ok(store) => store,
                Err(error) => {
                    tracing::error!("channel store unavailable for {app_name}: {error}");
                    ctx.observation.set_handler_result("error");
                    return self
                        .write_channel_error(
                            session,
                            ChannelError::Storage(format!("open store: {error}")),
                        )
                        .await;
                }
            };

            if meta.lifecycle.ephemeral
                && let Err(error) = store.ensure_ephemeral_supported()
            {
                return self.write_channel_error(session, error).await;
            }

            if use_first_frame_auth {
                let Some(endpoint) = instance.endpoint() else {
                    ctx.observation.set_handler_result("unavailable");
                    return self
                        .write_channel_error(session, ChannelError::AuthUnavailable)
                        .await;
                };
                let auth_mode = ChannelWebSocketAuth::FirstFrame {
                    app: AppAuthorizer {
                        endpoint: endpoint.to_string(),
                        internal_host: crate::instances::internal_app_host_for_app_id(app_name),
                        internal_token_header: crate::instances::INTERNAL_TOKEN_HEADER,
                        internal_token: instance.internal_token().to_string(),
                    },
                    params,
                    cookie,
                };
                drop(backend);
                ctx.observation.set_handler_result("websocket");
                return self
                    .write_channel_websocket(
                        session,
                        &store,
                        &stream,
                        requested,
                        presence_params,
                        auth_mode,
                    )
                    .await;
            }

            let auth_result = authorize_channel_request(
                app_name,
                instance,
                operation,
                &route.channel,
                params,
                header,
                cookie,
            )
            .await;

            let auth_result = match auth_result {
                Ok(result) => result,
                Err(error) => {
                    ctx.observation.set_handler_result("error");
                    return self.write_channel_error(session, error).await;
                }
            };
            if let Err(error) = require_websocket_grant(&auth_result) {
                ctx.observation.set_handler_result("unsupported");
                return self.write_channel_error(session, error).await;
            }
            drop(backend);
            ctx.observation.set_handler_result("websocket");
            return self
                .write_channel_websocket(
                    session,
                    &store,
                    &stream,
                    requested,
                    presence_params,
                    ChannelWebSocketAuth::Authorized(auth_result),
                )
                .await;
        }

        let store_params = params.clone();
        let auth_result = authorize_channel_request(
            app_name,
            instance,
            operation,
            &route.channel,
            params,
            header,
            cookie,
        )
        .await;

        let auth_result = match auth_result {
            Ok(result) => result,
            Err(error) => {
                ctx.observation.set_handler_result("error");
                return self.write_channel_error(session, error).await;
            }
        };
        if is_store && let Err(error) = validate_store_lifecycle(&auth_result) {
            return self.write_channel_error(session, error).await;
        }
        if let Some(name) = store_name {
            let Some(endpoint) = instance.endpoint() else {
                return self
                    .write_channel_error(session, ChannelError::AuthUnavailable)
                    .await;
            };
            let result = load_store(
                &endpoint.to_string(),
                &crate::instances::internal_app_host_for_app_id(app_name),
                instance.internal_token(),
                &name,
                store_params,
                auth_result.subject.as_deref(),
            )
            .await;
            ctx.observation
                .set_handler_result(if result.is_ok() { "load" } else { "error" });
            return match result {
                Ok(data) => self.write_json_response(session, 200, &data).await,
                Err(error) => self.write_channel_error(session, error).await,
            };
        }
        drop(backend);

        let store = match self.channel_store_for_app(app_name).await {
            Ok(store) => store,
            Err(error) => {
                tracing::error!("channel store unavailable for {app_name}: {error}");
                ctx.observation.set_handler_result("error");
                return self
                    .write_channel_error(
                        session,
                        ChannelError::Storage(format!("open store: {error}")),
                    )
                    .await;
            }
        };
        if is_store {
            let stream = store_scope(&route.channel, &store_params);
            ctx.observation.set_handler_result("sse");
            return serve_store_events_with_writer(
                session,
                store.store(),
                &stream,
                &auth_result,
                store.as_ref(),
            )
            .await;
        }
        if let Err(error) = store.sync_channel_async(stream.key(), &auth_result).await {
            ctx.observation.set_handler_result("error");
            return self.write_channel_error(session, error).await;
        }

        if session.req_header().method.as_str() != "GET" {
            ctx.observation.set_handler_result("error");
            return self
                .write_json_response(
                    session,
                    405,
                    &serde_json::json!({ "error": "Method not allowed" }),
                )
                .await;
        }
        if auth_result.ephemeral {
            let events = match store.subscribe_ephemeral(stream.key()) {
                Ok(events) => events,
                Err(error) => return self.write_channel_error(session, error).await,
            };
            let mut header = ResponseHeader::build(200, None)?;
            header.insert_header("Content-Type", "text/event-stream")?;
            header.insert_header("Cache-Control", "no-store")?;
            header.insert_header("Connection", "keep-alive")?;
            session
                .write_response_header(Box::new(header), false)
                .await?;
            ctx.observation.set_handler_result("sse");
            return tako_channels::ephemeral::serve_events(session, events, &auth_result, &stream)
                .await;
        }
        let cursor = match parse_message_id_cursor(
            session
                .req_header()
                .headers
                .get("last-event-id")
                .and_then(|value| value.to_str().ok()),
            "Last-Event-ID",
        ) {
            Ok(cursor) => cursor,
            Err(error) => {
                ctx.observation.set_handler_result("error");
                return self.write_channel_error(session, error).await;
            }
        };
        let resets = store.resets(stream.key());
        let cursor = match store
            .replay_from_async(stream.key(), ReplayFrom::new(cursor, from_start))
            .await
        {
            Ok(cursor) => cursor,
            Err(error) => return self.write_channel_error(session, error).await,
        };
        ctx.observation.set_handler_result("sse");
        self.write_channel_events(
            session,
            store.store(),
            &stream,
            cursor,
            &auth_result,
            resets,
        )
        .await
    }
}

#[cfg(test)]
mod tests;

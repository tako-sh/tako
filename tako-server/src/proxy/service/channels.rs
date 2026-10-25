use super::{BackendResolution, RequestCtx};
use crate::channels::{
    ChannelAuthResponse, ChannelAuthScheme, ChannelDefinitionMeta, ChannelError,
    ChannelHeaderValue, ChannelOperation, ChannelStore, ChannelTransport,
    authorize_channel_request, fetch_channel_registry, parse_channel_route,
    parse_message_id_cursor, parse_ws_last_message_id,
};
use crate::proxy::TakoProxy;
use crate::proxy::request::insert_body_headers;
use bytes::Bytes;
use pingora_core::prelude::*;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::Session;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tako_channels::live_store::{
    STORE_CHANNEL_PREFIX, load_store, parse_store_route, serve_store_events, store_scope,
    valid_store_name, validate_store_lifecycle, validate_store_query,
};
use tako_channels::params::{ParamsError, validate_query};
use ws::ChannelWebSocketAuth;

mod ws;

impl TakoProxy {
    /// Cache one store and change poller per app; opening a database may block.
    async fn channel_store_for_app(&self, app_name: &str) -> Result<Arc<ChannelStore>> {
        let cell = self
            .channel_stores
            .write()
            .entry(app_name.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
            .clone();
        cell.get_or_try_init(|| async {
            let resolver = self.channel_postgres_url.read().clone();
            let data_dir = self.lb.app_manager().data_dir().clone();
            let app_id = app_name.to_string();
            let store = Arc::new(
                tokio::task::spawn_blocking(move || {
                    let postgres_url = resolver.and_then(|resolver| resolver(&app_id));
                    let config = crate::channels::app_channel_store_config_with_postgres(
                        &data_dir,
                        &app_id,
                        postgres_url.as_deref(),
                    );
                    ChannelStore::open_config(config)
                })
                .await
                .map_err(|error| Error::explain(ErrorType::InternalError, error.to_string()))?
                .map_err(|error| {
                    Error::explain(
                        ErrorType::InternalError,
                        format!("Failed to open channel store for {app_name}: {error}"),
                    )
                })?,
            );
            Ok(store)
        })
        .await
        .cloned()
    }

    async fn channel_meta_for_app(
        &self,
        app_name: &str,
        instance: &crate::instances::Instance,
        channel: &str,
    ) -> std::result::Result<ChannelDefinitionMeta, ChannelError> {
        self.channel_registry
            .get_or_load(app_name, instance.build_version(), channel, || async {
                let defs = fetch_channel_registry(app_name, instance).await?;
                let store = self
                    .channel_store_for_app(app_name)
                    .await
                    .map_err(|error| ChannelError::Storage(error.to_string()))?;
                let lifecycle_defs = defs.clone();
                store
                    .run(move |store| {
                        for definition in lifecycle_defs {
                            store
                                .sync_channel(&definition.channel, &definition.lifecycle_auth())?;
                        }
                        Ok(())
                    })
                    .await?;
                Ok(defs)
            })
            .await
    }

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
        let (status, body) = match error {
            ChannelError::Unauthorized => (401, serde_json::json!({ "error": "Unauthorized" })),
            ChannelError::Forbidden => (403, serde_json::json!({ "error": "Forbidden" })),
            ChannelError::NotDefined => {
                (404, serde_json::json!({ "error": "Channel not defined" }))
            }
            ChannelError::StaleCursor => (
                410,
                serde_json::json!({ "error": "Channel cursor is outside the replay window" }),
            ),
            ChannelError::BadRequest(message) => (400, serde_json::json!({ "error": message })),
            ChannelError::Unsupported => (
                400,
                serde_json::json!({ "error": "Channel transport is not enabled" }),
            ),
            ChannelError::InvalidPath => (404, serde_json::json!({ "error": "Not found" })),
            ChannelError::AuthUnavailable => {
                (503, serde_json::json!({ "error": "Service Unavailable" }))
            }
            ChannelError::Storage(message) => {
                tracing::error!("Channel storage error: {message}");
                (500, serde_json::json!({ "error": "Internal Server Error" }))
            }
        };
        self.write_json_response(session, status, &body).await
    }

    async fn write_channel_events(
        &self,
        session: &mut Session,
        store: &Arc<ChannelStore>,
        channel: &str,
        mut after: Option<i64>,
        auth: &ChannelAuthResponse,
    ) -> Result<bool> {
        let mut header = ResponseHeader::build(200, None)?;
        header.insert_header("Content-Type", "text/event-stream")?;
        header.insert_header("Cache-Control", "no-store")?;
        header.insert_header("Connection", "keep-alive")?;
        session
            .write_response_header(Box::new(header), false)
            .await?;

        let keepalive_interval = Duration::from_millis(auth.keepalive_interval_ms.max(1));
        let max_connection_lifetime = Duration::from_millis(auth.max_connection_lifetime_ms.max(1));
        let started_at = tokio::time::Instant::now();
        let mut next_keepalive = started_at + keepalive_interval;
        let mut changes = store.changes(channel);
        let mut read_replay = true;

        loop {
            let messages = if read_replay {
                changes.borrow_and_update();
                store
                    .read_after_async(channel, after, 100)
                    .await
                    .map_err(|error| {
                        Error::explain(
                            ErrorType::InternalError,
                            format!("Failed to read channel replay: {error}"),
                        )
                    })?
            } else {
                Vec::new()
            };
            read_replay = messages.len() == 100;

            if !messages.is_empty() {
                for message in messages {
                    let encoded = serde_json::to_string(&message).map_err(|error| {
                        Error::explain(
                            ErrorType::InternalError,
                            format!("Failed to encode SSE payload: {error}"),
                        )
                    })?;
                    let frame = format!("id: {}\ndata: {}\n\n", message.id, encoded);
                    session
                        .write_response_body(Some(Bytes::from(frame)), false)
                        .await?;
                    after = Some(
                        message
                            .id
                            .parse::<i64>()
                            .expect("channel ids are always numeric"),
                    );
                }
                next_keepalive = tokio::time::Instant::now() + keepalive_interval;
            }

            if started_at.elapsed() >= max_connection_lifetime {
                session.write_response_body(None, true).await?;
                return Ok(true);
            }

            let now = tokio::time::Instant::now();
            if now >= next_keepalive {
                session
                    .write_response_body(Some(Bytes::from_static(b": keepalive\n\n")), false)
                    .await?;
                next_keepalive = now + keepalive_interval;
            }

            if read_replay {
                continue;
            }
            tokio::select! {
                _ = changes.changed() => read_replay = true,
                _ = tokio::time::sleep_until(next_keepalive.min(started_at + max_connection_lifetime)) => {}
            }
        }
    }

    pub(crate) async fn try_handle_channel_request(
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

        let is_websocket = session.as_downstream().is_upgrade_req();
        let request_headers = request_headers_to_map(session.req_header());
        let (header, cookie) = extract_credentials(&request_headers, &meta.auth);
        if is_websocket
            && cookie.is_some()
            && !websocket_origin_matches(session.req_header(), ctx.is_https)
        {
            return self
                .write_channel_error(session, ChannelError::Forbidden)
                .await;
        }
        let use_first_frame_auth =
            is_websocket && auth_scheme_requires_header(&meta.auth) && header.is_none();
        if !use_first_frame_auth
            && auth_scheme_requires_declared_credentials(&meta.auth)
            && header.is_none()
            && cookie.is_none()
        {
            ctx.observation.set_handler_result("unauthorized");
            return self
                .write_channel_error(session, ChannelError::Unauthorized)
                .await;
        }

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
            let query_cursor = match parse_ws_last_message_id(session.req_header().uri.query()) {
                Ok(cursor) => cursor,
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

            if use_first_frame_auth {
                let Some(endpoint) = instance.endpoint() else {
                    ctx.observation.set_handler_result("unavailable");
                    return self
                        .write_channel_error(session, ChannelError::AuthUnavailable)
                        .await;
                };
                let auth_mode = ChannelWebSocketAuth::FirstFrame {
                    endpoint: endpoint.to_string(),
                    internal_host: crate::instances::internal_app_host_for_app_id(app_name),
                    internal_token: instance.internal_token().to_string(),
                    params,
                    cookie,
                };
                drop(backend);
                ctx.observation.set_handler_result("websocket");
                return self
                    .write_channel_websocket(
                        session,
                        &store,
                        &route.channel,
                        query_cursor,
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
            if auth_result.transport != Some(ChannelTransport::Ws) {
                ctx.observation.set_handler_result("unsupported");
                return self
                    .write_channel_error(session, ChannelError::Unsupported)
                    .await;
            }
            drop(backend);
            ctx.observation.set_handler_result("websocket");
            return self
                .write_channel_websocket(
                    session,
                    &store,
                    &route.channel,
                    query_cursor,
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
            return serve_store_events(session, &store, &stream, &auth_result).await;
        }
        if let Err(error) = store.sync_channel_async(&route.channel, &auth_result).await {
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
        let cursor = match store.replay_cursor_async(&route.channel, cursor).await {
            Ok(cursor) => cursor,
            Err(error) => return self.write_channel_error(session, error).await,
        };
        ctx.observation.set_handler_result("sse");
        self.write_channel_events(session, &store, &route.channel, cursor, &auth_result)
            .await
    }
}

fn request_params_query(query: Option<&str>, is_store: bool) -> String {
    if is_store {
        query.unwrap_or_default().to_owned()
    } else {
        channel_params_query(query)
    }
}

/// Cookie credentials are ambient browser authority. Require an explicit,
/// same-origin handshake before forwarding them to the app's auth callback.
fn websocket_origin_matches(request: &RequestHeader, is_https: bool) -> bool {
    let mut origins = request.headers.get_all("origin").iter();
    let Some(origin) = origins.next().and_then(|value| value.to_str().ok()) else {
        return false;
    };
    if origins.next().is_some() {
        return false;
    }
    let Ok(origin) = reqwest::Url::parse(origin) else {
        return false;
    };
    if !matches!(origin.scheme(), "http" | "https")
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return false;
    }
    let scheme = if is_https { "https" } else { "http" };
    let host = crate::proxy::request::request_host(request);
    let Ok(expected) = reqwest::Url::parse(&format!("{scheme}://{host}")) else {
        return false;
    };
    origin.origin() == expected.origin()
}

fn channel_params_query(query: Option<&str>) -> String {
    let Some(query) = query else {
        return String::new();
    };

    query
        .split('&')
        .filter(|pair| {
            let key = pair.split_once('=').map_or(*pair, |(key, _)| key);
            key != "last_message_id"
        })
        .filter(|pair| !pair.is_empty())
        .collect::<Vec<_>>()
        .join("&")
}

fn auth_scheme_requires_declared_credentials(scheme: &ChannelAuthScheme) -> bool {
    matches!(
        scheme,
        ChannelAuthScheme::Required {
            header_name: Some(_),
            cookie_name: _
        } | ChannelAuthScheme::Required {
            header_name: _,
            cookie_name: Some(_)
        }
    )
}

fn auth_scheme_requires_header(scheme: &ChannelAuthScheme) -> bool {
    matches!(
        scheme,
        ChannelAuthScheme::Required {
            header_name: Some(_),
            ..
        }
    )
}

fn request_headers_to_map(request: &RequestHeader) -> HashMap<String, String> {
    request
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect()
}

pub(crate) fn extract_credentials(
    headers: &HashMap<String, String>,
    scheme: &ChannelAuthScheme,
) -> (Option<ChannelHeaderValue>, Option<String>) {
    let ChannelAuthScheme::Required {
        header_name,
        cookie_name,
    } = scheme
    else {
        return (None, None);
    };

    let header = header_name
        .as_ref()
        .and_then(|name| headers.get(&name.to_ascii_lowercase()))
        .map(|raw| ChannelHeaderValue::parse(raw));

    let cookie = cookie_name.as_ref().and_then(|name| {
        let raw = headers.get("cookie")?;
        raw.split(';').find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then(|| value.to_string())
        })
    });

    (header, cookie)
}

#[cfg(test)]
mod tests;

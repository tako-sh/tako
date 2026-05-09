use super::{BackendResolution, RequestCtx};
use crate::channels::{
    ChannelAuthResponse, ChannelAuthScheme, ChannelDefinitionMeta, ChannelError,
    ChannelHeaderValue, ChannelOperation, ChannelStore, ChannelTransport, app_channels_db_path,
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
use tako_channels::params::{ParamsError, validate_query};
use ws::ChannelWebSocketAuth;

mod ws;

impl TakoProxy {
    /// Return the cached `ChannelStore` for `app_name`, opening (and
    /// registering) it on first use. The store holds a single persistent
    /// SQLite connection — we share it across requests so the 100ms SSE
    /// poll loop doesn't reopen the DB + rerun PRAGMAs on every tick.
    fn channel_store_for_app(&self, app_name: &str) -> Result<Arc<ChannelStore>> {
        if let Some(existing) = self.channel_stores.read().get(app_name) {
            return Ok(existing.clone());
        }

        let mut stores = self.channel_stores.write();
        if let Some(existing) = stores.get(app_name) {
            return Ok(existing.clone());
        }

        let path = app_channels_db_path(self.lb.app_manager().data_dir(), app_name);
        let store = Arc::new(ChannelStore::open(&path).map_err(|error| {
            Error::explain(
                ErrorType::InternalError,
                format!("Failed to open channel store for {app_name}: {error}"),
            )
        })?);
        stores.insert(app_name.to_string(), store.clone());
        Ok(store)
    }

    async fn channel_meta_for_app(
        &self,
        app_name: &str,
        instance: &crate::instances::Instance,
        channel: &str,
    ) -> std::result::Result<ChannelDefinitionMeta, ChannelError> {
        if let Some(meta) = self.channel_registry.get(app_name, channel) {
            return Ok(meta);
        }

        let defs = fetch_channel_registry(instance).await?;
        self.channel_registry.install(app_name, defs);
        self.channel_registry
            .get(app_name, channel)
            .ok_or(ChannelError::NotDefined)
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
        store: &ChannelStore,
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

        loop {
            let messages = store.read_after(channel, after, 100).map_err(|error| {
                Error::explain(
                    ErrorType::InternalError,
                    format!("Failed to read channel replay: {error}"),
                )
            })?;

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

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub(crate) async fn try_handle_channel_request(
        &self,
        session: &mut Session,
        _ctx: &mut RequestCtx,
        app_name: &str,
        path: &str,
        _host: &str,
    ) -> Result<bool> {
        let route = match parse_channel_route(path) {
            Ok(Some(route)) => route,
            Ok(None) => {
                return Ok(false);
            }
            Err(error) => {
                return self.write_channel_error(session, error).await;
            }
        };

        let backend = match self.resolve_backend(app_name).await {
            BackendResolution::Ready(backend) => backend,
            BackendResolution::StartupTimeout => {
                tracing::warn!(app = %app_name, "Channel auth unavailable: app startup timed out");
                return self
                    .write_channel_error(session, ChannelError::AuthUnavailable)
                    .await;
            }
            BackendResolution::StartupFailed
            | BackendResolution::QueueFull
            | BackendResolution::Unavailable
            | BackendResolution::AppMissing => {
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
                return self.write_channel_error(session, error).await;
            }
        };

        let params_query = channel_params_query(session.req_header().uri.query());
        let params = match validate_query(&meta.params_schema, &params_query) {
            Ok(params) => params,
            Err(ParamsError::Invalid(message)) => {
                return self
                    .write_channel_error(
                        session,
                        ChannelError::BadRequest(format!("invalid channel params: {message}")),
                    )
                    .await;
            }
            Err(ParamsError::InvalidSchema(message)) => {
                return self
                    .write_channel_error(
                        session,
                        ChannelError::Storage(format!("invalid channel schema: {message}")),
                    )
                    .await;
            }
        };

        let is_websocket = session.as_downstream().is_upgrade_req();
        let request_headers = request_headers_to_map(session.req_header());
        let (header, cookie) = extract_credentials(&request_headers, &meta.auth);
        let use_first_frame_auth =
            is_websocket && auth_scheme_requires_header(&meta.auth) && header.is_none();
        if !use_first_frame_auth
            && auth_scheme_requires_declared_credentials(&meta.auth)
            && header.is_none()
            && cookie.is_none()
        {
            return self
                .write_channel_error(session, ChannelError::Unauthorized)
                .await;
        }

        if is_websocket {
            if session.req_header().method.as_str() != "GET" {
                return self
                    .write_json_response(
                        session,
                        405,
                        &serde_json::json!({ "error": "Method not allowed" }),
                    )
                    .await;
            }
            if meta.transport != Some(ChannelTransport::Ws) {
                return self
                    .write_channel_error(session, ChannelError::Unsupported)
                    .await;
            }
            let query_cursor = match parse_ws_last_message_id(session.req_header().uri.query()) {
                Ok(cursor) => cursor,
                Err(error) => {
                    return self.write_channel_error(session, error).await;
                }
            };
            let store = match self.channel_store_for_app(app_name) {
                Ok(store) => store,
                Err(error) => {
                    tracing::error!("channel store unavailable for {app_name}: {error}");
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
                    return self
                        .write_channel_error(session, ChannelError::AuthUnavailable)
                        .await;
                };
                let auth_mode = ChannelWebSocketAuth::FirstFrame {
                    endpoint: endpoint.to_string(),
                    internal_token: instance.internal_token().to_string(),
                    params,
                    cookie,
                };
                drop(backend);
                return self
                    .write_channel_websocket(
                        session,
                        &store,
                        &route.channel,
                        query_cursor,
                        auth_mode,
                    )
                    .await;
            }

            let auth_result = authorize_channel_request(
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
                Err(error) => return self.write_channel_error(session, error).await,
            };
            if auth_result.transport != Some(ChannelTransport::Ws) {
                return self
                    .write_channel_error(session, ChannelError::Unsupported)
                    .await;
            }
            drop(backend);
            return self
                .write_channel_websocket(
                    session,
                    &store,
                    &route.channel,
                    query_cursor,
                    ChannelWebSocketAuth::Authorized(auth_result),
                )
                .await;
        }

        let auth_result =
            authorize_channel_request(instance, operation, &route.channel, params, header, cookie)
                .await;

        let auth_result = match auth_result {
            Ok(result) => result,
            Err(error) => return self.write_channel_error(session, error).await,
        };
        drop(backend);

        let store = match self.channel_store_for_app(app_name) {
            Ok(store) => store,
            Err(error) => {
                tracing::error!("channel store unavailable for {app_name}: {error}");
                return self
                    .write_channel_error(
                        session,
                        ChannelError::Storage(format!("open store: {error}")),
                    )
                    .await;
            }
        };
        if let Err(error) = store.sync_channel(&route.channel, &auth_result) {
            return self.write_channel_error(session, error).await;
        }

        if session.req_header().method.as_str() != "GET" {
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
        )
        .and_then(|cursor| store.replay_cursor(&route.channel, cursor))
        {
            Ok(cursor) => cursor,
            Err(error) => return self.write_channel_error(session, error).await,
        };
        self.write_channel_events(session, &store, &route.channel, cursor, &auth_result)
            .await
    }
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

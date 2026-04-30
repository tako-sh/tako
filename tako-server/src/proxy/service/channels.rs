use super::{BackendResolution, RequestCtx};
use crate::channels::{
    ChannelAuthResponse, ChannelAuthScheme, ChannelError, ChannelHeaderValue, ChannelOperation,
    ChannelStore, ChannelTransport, app_channels_db_path, authorize_channel_request,
    parse_channel_route, parse_message_id_cursor, parse_ws_last_message_id,
};
use crate::channels_ws::{
    WebSocketFrameReader, build_websocket_upgrade_response, parse_publish_payload,
    websocket_close_frame, websocket_ping_frame, websocket_pong_frame, websocket_text_frame,
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
            ChannelError::AuthUnavailable => (
                503,
                serde_json::json!({ "error": "Channel auth unavailable" }),
            ),
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

    async fn write_channel_websocket(
        &self,
        session: &mut Session,
        store: &ChannelStore,
        channel: &str,
        mut after: Option<i64>,
        auth: &ChannelAuthResponse,
    ) -> Result<bool> {
        if !session.as_downstream().is_upgrade_req() {
            return self
                .write_json_response(
                    session,
                    400,
                    &serde_json::json!({ "error": "WebSocket upgrade required" }),
                )
                .await;
        }

        let header = match build_websocket_upgrade_response(session.req_header()) {
            Ok(header) => header,
            Err(error) => return self.write_channel_error(session, error).await,
        };
        session
            .write_response_header(Box::new(header), false)
            .await?;

        let keepalive_interval = Duration::from_millis(auth.keepalive_interval_ms.max(1));
        let max_connection_lifetime = Duration::from_millis(auth.max_connection_lifetime_ms.max(1));
        let started_at = tokio::time::Instant::now();
        let mut next_ping = started_at + keepalive_interval;
        let mut reader = WebSocketFrameReader::default();

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
                            format!("Failed to encode websocket payload: {error}"),
                        )
                    })?;
                    session
                        .write_response_body(
                            Some(Bytes::from(websocket_text_frame(&encoded))),
                            false,
                        )
                        .await?;
                    after = Some(
                        message
                            .id
                            .parse::<i64>()
                            .expect("channel ids are always numeric"),
                    );
                }
                next_ping = tokio::time::Instant::now() + keepalive_interval;
            }

            if started_at.elapsed() >= max_connection_lifetime {
                session
                    .write_response_body(
                        Some(Bytes::from(websocket_close_frame(
                            1000,
                            "connection expired",
                        ))),
                        true,
                    )
                    .await?;
                return Ok(true);
            }

            let ping_deadline = next_ping;
            let sleep_until = std::cmp::min(ping_deadline, started_at + max_connection_lifetime);
            enum WebSocketAction {
                Read(Option<Bytes>),
                Tick,
            }

            let action = {
                let mut sleep = std::pin::pin!(tokio::time::sleep_until(sleep_until));
                let mut read = std::pin::pin!(session.as_downstream_mut().read_body_or_idle(true));
                tokio::select! {
                    body = &mut read => WebSocketAction::Read(body?),
                    _ = &mut sleep => WebSocketAction::Tick,
                }
            };

            match action {
                WebSocketAction::Read(Some(chunk)) => {
                    reader.extend(&chunk);
                    while let Some(frame) = reader.next_frame().map_err(|error| {
                        Error::explain(
                            ErrorType::InvalidHTTPHeader,
                            format!("Invalid websocket frame: {error}"),
                        )
                    })? {
                        match frame.opcode {
                            0x1 => {
                                let payload =
                                    parse_publish_payload(&frame.payload).map_err(|error| {
                                        Error::explain(
                                            ErrorType::InvalidHTTPHeader,
                                            format!("Invalid websocket publish payload: {error}"),
                                        )
                                    })?;
                                store.append(channel, &payload).map_err(|error| {
                                    Error::explain(
                                        ErrorType::InternalError,
                                        format!(
                                            "Failed to append websocket channel payload: {error}"
                                        ),
                                    )
                                })?;
                            }
                            0x8 => {
                                session
                                    .write_response_body(
                                        Some(Bytes::from(websocket_close_frame(1000, "closing"))),
                                        true,
                                    )
                                    .await?;
                                return Ok(true);
                            }
                            0x9 => {
                                session
                                    .write_response_body(
                                        Some(Bytes::from(websocket_pong_frame(&frame.payload))),
                                        false,
                                    )
                                    .await?;
                            }
                            0xA => {}
                            _ => {
                                session
                                    .write_response_body(
                                        Some(Bytes::from(websocket_close_frame(
                                            1003,
                                            "unsupported frame",
                                        ))),
                                        true,
                                    )
                                    .await?;
                                return Ok(true);
                            }
                        }
                    }
                    next_ping = tokio::time::Instant::now() + keepalive_interval;
                }
                WebSocketAction::Read(None) => return Ok(true),
                WebSocketAction::Tick => {
                    if tokio::time::Instant::now() >= next_ping {
                        session
                            .write_response_body(
                                Some(Bytes::from(websocket_ping_frame(b""))),
                                false,
                            )
                            .await?;
                        next_ping = tokio::time::Instant::now() + keepalive_interval;
                    }
                }
            }
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
                return self
                    .write_channel_error(session, ChannelError::AuthUnavailable)
                    .await;
            }
            BackendResolution::StartupFailed
            | BackendResolution::QueueFull
            | BackendResolution::Unavailable
            | BackendResolution::AppMissing => {
                return self
                    .write_channel_error(session, ChannelError::AuthUnavailable)
                    .await;
            }
        };

        let app = match self.lb.app_manager().get_app(&backend.app_name) {
            Some(app) => app,
            None => {
                self.lb
                    .request_completed(&backend.app_name, &backend.instance_id);
                return self
                    .write_channel_error(session, ChannelError::AuthUnavailable)
                    .await;
            }
        };
        let instance = match app.get_instance(&backend.instance_id) {
            Some(instance) => instance,
            None => {
                self.lb
                    .request_completed(&backend.app_name, &backend.instance_id);
                return self
                    .write_channel_error(session, ChannelError::AuthUnavailable)
                    .await;
            }
        };

        let operation = if session.as_downstream().is_upgrade_req() {
            ChannelOperation::Connect
        } else {
            ChannelOperation::Subscribe
        };

        let request_headers = request_headers_to_map(session.req_header());
        let header = request_headers
            .get("authorization")
            .map(|raw| ChannelHeaderValue::parse(raw));

        let auth_result = authorize_channel_request(
            &instance,
            operation.clone(),
            &route.channel,
            serde_json::json!({}),
            header,
            None,
        )
        .await;

        self.lb
            .request_completed(&backend.app_name, &backend.instance_id);

        let auth_result = match auth_result {
            Ok(result) => result,
            Err(error) => return self.write_channel_error(session, error).await,
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
        if let Err(error) = store.sync_channel(&route.channel, &auth_result) {
            return self.write_channel_error(session, error).await;
        }

        if session.as_downstream().is_upgrade_req() {
            if session.req_header().method.as_str() != "GET" {
                return self
                    .write_json_response(
                        session,
                        405,
                        &serde_json::json!({ "error": "Method not allowed" }),
                    )
                    .await;
            }
            if auth_result.transport != Some(ChannelTransport::Ws) {
                return self
                    .write_channel_error(session, ChannelError::Unsupported)
                    .await;
            }
            let cursor = match parse_ws_last_message_id(session.req_header().uri.query())
                .and_then(|cursor| store.replay_cursor(&route.channel, cursor))
            {
                Ok(cursor) => cursor,
                Err(error) => return self.write_channel_error(session, error).await,
            };
            self.write_channel_websocket(session, &store, &route.channel, cursor, &auth_result)
                .await
        } else {
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
mod tests {
    use super::*;

    #[test]
    fn extract_credentials_picks_declared_header_and_cookie() {
        let mut headers = HashMap::new();
        headers.insert("authorization".into(), "Bearer abc".into());
        headers.insert("cookie".into(), "session=xyz; other=ignored".into());

        let scheme = ChannelAuthScheme::Required {
            header_name: Some("authorization".into()),
            cookie_name: Some("session".into()),
        };

        let (header, cookie) = extract_credentials(&headers, &scheme);
        assert_eq!(
            header,
            Some(ChannelHeaderValue {
                scheme: Some("Bearer".into()),
                value: "abc".into()
            })
        );
        assert_eq!(cookie, Some("xyz".to_string()));
    }

    #[test]
    fn extract_credentials_normalizes_header_name_lookup() {
        let mut headers = HashMap::new();
        headers.insert("x-session-token".into(), "plain-token".into());

        let scheme = ChannelAuthScheme::Required {
            header_name: Some("X-Session-Token".into()),
            cookie_name: None,
        };

        let (header, cookie) = extract_credentials(&headers, &scheme);
        assert_eq!(
            header,
            Some(ChannelHeaderValue {
                scheme: None,
                value: "plain-token".into()
            })
        );
        assert!(cookie.is_none());
    }

    #[test]
    fn extract_credentials_returns_none_for_public_channels() {
        let mut headers = HashMap::new();
        headers.insert("authorization".into(), "Bearer abc".into());

        let (header, cookie) = extract_credentials(&headers, &ChannelAuthScheme::Public);
        assert!(header.is_none());
        assert!(cookie.is_none());
    }
}

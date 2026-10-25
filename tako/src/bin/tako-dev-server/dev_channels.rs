//! Channel support for `tako dev`, backed by in-memory `tako-channels` stores.
//!
//! No auth in dev mode — all operations are allowed. Production uses
//! `tako-server/src/proxy/service.rs` with per-request authorization.

use pingora_core::Result;
use pingora_http::ResponseHeader;
use pingora_proxy::Session;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tako_channels::live_store::{
    STORE_CHANNEL_PREFIX, authorize_store, load_store, parse_store_route, serve_store_events,
    store_scope, valid_store_name,
};
use tako_channels::{
    ChannelAuthResponse, ChannelPublishPayload, ChannelStore, parse_channel_route,
};

/// Default auth response for dev mode — permissive, no subject, SSE transport.
fn dev_auth_response() -> ChannelAuthResponse {
    ChannelAuthResponse {
        ok: true,
        subject: None,
        transport: None,
        replay_window_ms: 10 * 60 * 1000,
        inactivity_ttl_ms: 0,
        keepalive_interval_ms: 25_000,
        max_connection_lifetime_ms: 2 * 60 * 60 * 1000,
    }
}

#[derive(Clone)]
pub struct DevChannelStore {
    stores: Arc<Mutex<HashMap<String, Arc<ChannelStore>>>>,
}

impl DevChannelStore {
    pub fn new() -> Self {
        Self {
            stores: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn store_for_app(&self, app_id: &str) -> Arc<ChannelStore> {
        let app_id = app_id.trim();
        let app_id = if app_id.is_empty() { "app" } else { app_id };
        let key = tako_core::deployment_app_id_filename(app_id);

        if let Some(store) = self.stores.lock().unwrap().get(&key) {
            return store.clone();
        }

        let mut stores = self.stores.lock().unwrap();
        if let Some(store) = stores.get(&key) {
            return store.clone();
        }

        let store = Arc::new(ChannelStore::open_in_memory().unwrap_or_else(|e| {
            panic!("failed to open in-memory dev channel store for {app_id}: {e}")
        }));
        stores.insert(key, store.clone());
        store
    }

    /// Append a message to `channel` and return the stored record. Used
    /// from the internal socket's `Command::ChannelPublish` path (server-side
    /// channel `.publish()` from app/workflow code).
    pub fn publish(
        &self,
        app_id: &str,
        channel: &str,
        payload: &ChannelPublishPayload,
        lifecycle: Option<tako_channels::ChannelLifecycle>,
    ) -> Result<tako_channels::ChannelMessage, tako_channels::ChannelError> {
        let store = self.store_for_app(app_id);
        if let Some(lifecycle) = lifecycle {
            store.sync_channel(
                channel,
                &ChannelAuthResponse {
                    replay_window_ms: lifecycle.replay_window_ms,
                    inactivity_ttl_ms: lifecycle.inactivity_ttl_ms,
                    keepalive_interval_ms: lifecycle.keepalive_interval_ms,
                    max_connection_lifetime_ms: lifecycle.max_connection_lifetime_ms,
                    ..dev_auth_response()
                },
            )?;
        }
        store.append(channel, payload)
    }
}

/// Try to handle a channel request. Returns `Ok(true)` if handled,
/// `Ok(false)` if the path is not a channel route (pass to upstream).
pub async fn try_handle(
    session: &mut Session,
    dev_store: &DevChannelStore,
    app_id: &str,
    path: &str,
    method: &str,
) -> Result<bool> {
    let route = match parse_channel_route(path) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(false),
        Err(_) => return write_json(session, 400, r#"{"error":"Invalid channel path"}"#).await,
    };

    if method != "GET" {
        return write_json(session, 405, r#"{"error":"Method not allowed"}"#).await;
    }
    serve_sse(session, dev_store, app_id, &route.channel).await
}

/// Stores use app authorization even in local dev, including schema coercion
/// and verified subjects for loaders. No app token is exposed to the browser.
pub async fn try_handle_store(
    session: &mut Session,
    dev_store: &DevChannelStore,
    target: &crate::proxy::RouteTarget,
    path: &str,
) -> Result<bool> {
    if session.req_header().method.as_str() != "GET" || session.as_downstream().is_upgrade_req() {
        return write_json(session, 405, r#"{"error":"Method not allowed"}"#).await;
    }
    let load_name = match parse_store_route(path) {
        Ok(name) => name,
        Err(_) => return write_json(session, 404, r#"{"error":"Not found"}"#).await,
    };
    let channel = match &load_name {
        Some(name) => format!("{STORE_CHANNEL_PREFIX}{name}"),
        None => match parse_channel_route(path) {
            Ok(Some(route)) if route.channel.starts_with(STORE_CHANNEL_PREFIX) => route.channel,
            _ => return write_json(session, 404, r#"{"error":"Not found"}"#).await,
        },
    };
    if !valid_store_name(&channel[STORE_CHANNEL_PREFIX.len()..]) {
        return write_json(session, 404, r#"{"error":"Not found"}"#).await;
    }
    let Some(context) = &target.store_auth else {
        return write_json(session, 503, r#"{"error":"Service Unavailable"}"#).await;
    };
    let headers = session
        .req_header()
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
        })
        .collect();
    let endpoint = format!("127.0.0.1:{}", target.upstream_port);
    let query = session.req_header().uri.query().unwrap_or_default();
    let (params, auth) = match authorize_store(
        &endpoint,
        &context.internal_host,
        &context.token,
        &channel,
        query,
        &headers,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return write_store_error(session, error).await,
    };
    if let Some(name) = load_name {
        return match load_store(
            &endpoint,
            &context.internal_host,
            &context.token,
            &name,
            params,
            auth.subject.as_deref(),
        )
        .await
        {
            Ok(data) => write_json(session, 200, &data.to_string()).await,
            Err(error) => write_store_error(session, error).await,
        };
    }
    let app_id = target.channel_store_key.clone();
    let dev_store = dev_store.clone();
    let store = tokio::task::spawn_blocking(move || dev_store.store_for_app(&app_id))
        .await
        .map_err(|error| {
            pingora_core::Error::explain(pingora_core::ErrorType::InternalError, error.to_string())
        })?;
    serve_store_events(session, &store, &store_scope(&channel, &params), &auth).await
}

async fn write_store_error(
    session: &mut Session,
    error: tako_channels::ChannelError,
) -> Result<bool> {
    use tako_channels::ChannelError;
    let (status, message) = match error {
        ChannelError::Unauthorized => (401, "Unauthorized"),
        ChannelError::Forbidden => (403, "Forbidden"),
        ChannelError::NotDefined | ChannelError::InvalidPath => (404, "Store not defined"),
        ChannelError::BadRequest(_) => (400, "Invalid Store request"),
        _ => (503, "Service Unavailable"),
    };
    write_json(
        session,
        status,
        &serde_json::json!({"error":message}).to_string(),
    )
    .await
}

async fn serve_sse(
    session: &mut Session,
    dev_store: &DevChannelStore,
    app_id: &str,
    channel: &str,
) -> Result<bool> {
    let dev_store = dev_store.clone();
    let app_id = app_id.to_owned();
    let store = tokio::task::spawn_blocking(move || dev_store.store_for_app(&app_id))
        .await
        .map_err(|error| {
            pingora_core::Error::explain(pingora_core::ErrorType::InternalError, error.to_string())
        })?;
    let auth = dev_auth_response();
    if session.as_downstream().is_upgrade_req() {
        let params = match tako_channels::params::validate_query(
            &serde_json::json!({"type":"object"}),
            session.req_header().uri.query().unwrap_or_default(),
        ) {
            Ok(params) => params,
            Err(_) => {
                return write_json(session, 400, r#"{"error":"Invalid presence params"}"#).await;
            }
        };
        let header =
            tako_channels::websocket::build_websocket_upgrade_response(session.req_header())
                .map_err(|e| {
                    pingora_core::Error::explain(
                        pingora_core::ErrorType::InvalidHTTPHeader,
                        e.to_string(),
                    )
                })?;
        session
            .write_response_header(Box::new(header), false)
            .await?;
        return tako_channels::websocket::serve(
            session,
            &store,
            channel,
            None,
            auth,
            params,
            Default::default(),
        )
        .await;
    }

    // Resolve the initial cursor — start from the latest message so new
    // subscribers only see future events (matching production behavior).
    let mut after = match store.replay_cursor_async(channel, None).await {
        Ok(cursor) => cursor,
        Err(e) => {
            let msg = format!(r#"{{"error":"Failed to resolve cursor: {e}"}}"#);
            return write_json(session, 500, &msg).await;
        }
    };

    let keepalive_interval = Duration::from_millis(auth.keepalive_interval_ms.max(1));
    let max_connection_lifetime = Duration::from_millis(auth.max_connection_lifetime_ms.max(1));

    let mut header = ResponseHeader::build(200, None)?;
    header.insert_header("Content-Type", "text/event-stream")?;
    header.insert_header("Cache-Control", "no-store")?;
    header.insert_header("Connection", "keep-alive")?;
    header.insert_header("Access-Control-Allow-Origin", "*")?;
    session
        .write_response_header(Box::new(header), false)
        .await?;

    let started = tokio::time::Instant::now();
    let mut next_keepalive = started + keepalive_interval;
    let mut changes = store.changes();
    let mut read_replay = true;

    loop {
        if read_replay {
            changes.borrow_and_update();
            match store.read_after_async(channel, after, 100).await {
                Ok(messages) => {
                    read_replay = messages.len() == 100;
                    if !messages.is_empty() {
                        for message in messages {
                            let encoded = serde_json::to_string(&message).unwrap_or_default();
                            let frame = format!("id: {}\ndata: {}\n\n", message.id, encoded);
                            if session
                                .write_response_body(Some(frame.into_bytes().into()), false)
                                .await
                                .is_err()
                            {
                                return Ok(true);
                            }
                            after = Some(
                                message
                                    .id
                                    .parse::<i64>()
                                    .expect("channel ids are always numeric"),
                            );
                        }
                        next_keepalive = tokio::time::Instant::now() + keepalive_interval;
                    }
                }
                Err(error) => {
                    return Err(pingora_core::Error::explain(
                        pingora_core::ErrorType::InternalError,
                        error.to_string(),
                    ));
                }
            }
        }

        if started.elapsed() >= max_connection_lifetime {
            session.write_response_body(None, true).await?;
            return Ok(true);
        }

        let now = tokio::time::Instant::now();
        if now >= next_keepalive {
            if session
                .write_response_body(
                    Some(": keepalive\n\n".to_string().into_bytes().into()),
                    false,
                )
                .await
                .is_err()
            {
                return Ok(true);
            }
            next_keepalive = now + keepalive_interval;
        }

        if read_replay {
            continue;
        }
        tokio::select! {
            _ = changes.changed() => read_replay = true,
            _ = tokio::time::sleep_until(next_keepalive.min(started + max_connection_lifetime)) => {}
        }
    }
}

async fn write_json(session: &mut Session, status: u16, body: &str) -> Result<bool> {
    let mut header = ResponseHeader::build(status, None)?;
    header.insert_header("Content-Type", "application/json")?;
    header.insert_header("Cache-Control", "no-store")?;
    header.insert_header("Access-Control-Allow-Origin", "*")?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(Some(body.to_string().into_bytes().into()), true)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod store_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use tako_channels::parse_channel_route;

    #[test]
    fn parse_subscribe_path() {
        let route = parse_channel_route("/_tako/channels/demo-broadcast")
            .unwrap()
            .unwrap();
        assert_eq!(route.channel, "demo-broadcast");
    }

    #[test]
    fn parse_rejects_nested_channel_path() {
        assert!(parse_channel_route("/_tako/channels/demo-broadcast/messages").is_err());
    }

    #[test]
    fn parse_returns_none_for_non_channel_path() {
        assert!(parse_channel_route("/api/hello").unwrap().is_none());
    }

    #[test]
    fn parse_returns_error_for_empty_channel_path() {
        assert!(parse_channel_route("/_tako/channels/").is_err());
    }

    #[test]
    fn publish_and_read_via_store() {
        let dev_store = DevChannelStore::new();
        let store = dev_store.store_for_app("demo");

        // Sync the channel first.
        store.sync_channel("test", &dev_auth_response()).unwrap();

        let msg = store
            .append(
                "test",
                &ChannelPublishPayload {
                    r#type: "message".to_string(),
                    data: serde_json::json!({"text": "hello"}),
                },
            )
            .unwrap();

        assert_eq!(msg.id, "1");
        assert_eq!(msg.channel, "test");

        let messages = store.read_after("test", None, 100).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data, serde_json::json!({"text": "hello"}));
    }

    #[test]
    fn publish_increments_ids() {
        let dev_store = DevChannelStore::new();
        let store = dev_store.store_for_app("demo");

        store.sync_channel("ch", &dev_auth_response()).unwrap();

        let p = ChannelPublishPayload {
            r#type: "message".to_string(),
            data: serde_json::json!(null),
        };
        assert_eq!(store.append("ch", &p).unwrap().id, "1");
        assert_eq!(store.append("ch", &p).unwrap().id, "2");
        assert_eq!(store.append("ch", &p).unwrap().id, "3");
    }

    #[test]
    fn dev_channel_store_keeps_apps_in_separate_dbs() {
        let dev_store = DevChannelStore::new();
        let payload = ChannelPublishPayload {
            r#type: "message".to_string(),
            data: serde_json::json!({"text": "hello"}),
        };

        let first = dev_store
            .publish("app/one", "chat", &payload, None)
            .unwrap();
        let second = dev_store
            .publish("app/two", "chat", &payload, None)
            .unwrap();

        assert_eq!(first.id, "1");
        assert_eq!(second.id, "1");
        assert_eq!(
            dev_store
                .store_for_app("app/one")
                .read_after("chat", None, 100)
                .unwrap()
                .len(),
            1,
        );
        assert_eq!(
            dev_store
                .store_for_app("app/two")
                .read_after("chat", None, 100)
                .unwrap()
                .len(),
            1,
        );
    }
}

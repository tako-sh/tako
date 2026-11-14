//! Channel support for `tako dev`, backed by in-memory `tako-channels` stores.
//!
//! Subscribe and connect are authorized by the running app exactly as in
//! production (`tako-server/src/proxy/service/channels.rs`): the proxy has
//! already woken the app and waited for its port, the app's registry supplies
//! the params schema and auth scheme, and its `/channels/authorize` answer
//! supplies the subject, transport and lifecycle the stream is served with.

use pingora_core::Result;
use pingora_http::ResponseHeader;
use pingora_proxy::Session;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tako_channels::authorize::{
    AppAuthorizer, RequestCredentials, authorize_first_frame, request_credentials,
    require_websocket_grant,
};
use tako_channels::live_store::{
    STORE_CHANNEL_PREFIX, authorize_store, load_store, parse_store_route, serve_store_events,
    store_scope, valid_store_name,
};
use tako_channels::params::ParamsError;
use tako_channels::{
    ChannelAuthResponse, ChannelDefinitionMeta, ChannelError, ChannelOperation, ChannelStore,
    ChannelStream, ChannelTransport, ReplayFrom, TrustedPublication, parse_channel_route,
    parse_message_id_cursor, parse_replay_from, parse_ws_last_message_id,
};

/// The dev proxy listens only on HTTPS, so a cookie-authenticated socket's
/// `Origin` must be the `https://` origin of the requested host.
const DEV_PROXY_IS_HTTPS: bool = true;
const INTERNAL_TOKEN_HEADER: &str = "x-tako-internal-token";

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

    /// Apply a server-side publication from the internal socket's
    /// `Command::ChannelPublish` path (channel `.publish()` from app or
    /// workflow code) to the stream its params select.
    pub fn publish(
        &self,
        app_id: &str,
        publication: &TrustedPublication,
    ) -> Result<tako_channels::ChannelMessage, tako_channels::ChannelError> {
        self.store_for_app(app_id).publish_trusted(publication)
    }
}

/// Bind an internal-socket publish or close to its stream. A close appends
/// the stream's terminal marker; ephemeral channels cannot be closed.
pub fn trusted_publication(
    request: tako_workflows::ChannelPublishRequest<'_>,
) -> Result<TrustedPublication, ChannelError> {
    match request.write {
        tako_workflows::ChannelWrite::Publish(payload) => {
            TrustedPublication::parse(request.channel, request.params, payload)
        }
        tako_workflows::ChannelWrite::Close { lifecycle } => {
            TrustedPublication::close(request.channel, request.params, lifecycle)
        }
    }
}

/// Try to handle a channel request. Returns `Ok(true)` if handled,
/// `Ok(false)` if the path is not a channel route (pass to upstream).
pub async fn try_handle(
    session: &mut Session,
    dev_store: &DevChannelStore,
    target: &crate::proxy::RouteTarget,
    path: &str,
    method: &str,
) -> Result<bool> {
    if path == tako_channels::shared::PATH {
        let Some(context) = &target.store_auth else {
            return write_channel_error(session, target, ChannelError::AuthUnavailable).await;
        };
        let app = AppAuthorizer {
            endpoint: format!("127.0.0.1:{}", target.upstream_port),
            internal_host: context.internal_host.clone(),
            internal_token_header: INTERNAL_TOKEN_HEADER,
            internal_token: context.token.clone(),
        };
        let store = store_for(dev_store, &target.channel_store_key).await?;
        let writer = Arc::new(tako_channels::LocalChannelWriter::new(store.clone()));
        return tako_channels::shared::serve(
            session,
            Arc::new(app),
            store,
            writer,
            DEV_PROXY_IS_HTTPS,
        )
        .await;
    }
    let route = match parse_channel_route(path) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(false),
        Err(error) => return write_channel_error(session, target, error).await,
    };

    if method != "GET" {
        return write_json(session, 405, r#"{"error":"Method not allowed"}"#).await;
    }
    let Some(context) = &target.store_auth else {
        return write_channel_error(session, target, ChannelError::AuthUnavailable).await;
    };
    let app = AppAuthorizer {
        endpoint: format!("127.0.0.1:{}", target.upstream_port),
        internal_host: context.internal_host.clone(),
        internal_token_header: INTERNAL_TOKEN_HEADER,
        internal_token: context.token.clone(),
    };
    let meta = match channel_definition(&app, &route.channel).await {
        Ok(meta) => meta,
        Err(error) => return write_channel_error(session, target, error).await,
    };
    // Params are validated and coerced as in production, so dev subscribers
    // bind the same stream a server-side publish with the same params reaches.
    let query = tako_channels::params::channel_params_query(session.req_header().uri.query());
    let params = match tako_channels::params::validate_query(&meta.params_schema, &query) {
        Ok(params) => params,
        Err(ParamsError::Invalid(message)) => {
            let error = ChannelError::BadRequest(format!("invalid channel params: {message}"));
            return write_channel_error(session, target, error).await;
        }
        Err(ParamsError::InvalidSchema(message)) => {
            let error = ChannelError::Storage(format!("invalid channel schema: {message}"));
            return write_channel_error(session, target, error).await;
        }
    };
    let stream = ChannelStream::new(&route.channel, &params);
    let from_start = match parse_replay_from(session.req_header().uri.query()) {
        Ok(from_start) => from_start,
        Err(error) => return write_channel_error(session, target, error).await,
    };
    let websocket = session.as_downstream().is_upgrade_req();
    let RequestCredentials {
        header,
        cookie,
        first_frame,
    } = match request_credentials(
        session.req_header(),
        &meta.auth,
        websocket,
        DEV_PROXY_IS_HTTPS,
    ) {
        Ok(credentials) => credentials,
        Err(error) => return write_channel_error(session, target, error).await,
    };

    if websocket {
        if meta.transport != Some(ChannelTransport::Ws) {
            return write_channel_error(session, target, ChannelError::Unsupported).await;
        }
        // As in production, a query cursor wins over `from`; the shared
        // session loop resolves it and signals a stale cursor or a close.
        let requested = match parse_ws_last_message_id(session.req_header().uri.query()) {
            Ok(cursor) => ReplayFrom::new(cursor, from_start),
            Err(error) => return write_channel_error(session, target, error).await,
        };
        let auth = if first_frame {
            DevSocketAuth::FirstFrame { app, cookie }
        } else {
            let auth = match app
                .authorize(
                    ChannelOperation::Connect,
                    &route.channel,
                    params.clone(),
                    header,
                    cookie,
                )
                .await
                .and_then(|auth| require_websocket_grant(&auth).map(|()| auth))
            {
                Ok(auth) => auth,
                Err(error) => return write_channel_error(session, target, error).await,
            };
            DevSocketAuth::Authorized(auth)
        };
        return serve_websocket(session, dev_store, target, &stream, params, requested, auth).await;
    }

    let auth = match app
        .authorize(
            ChannelOperation::Subscribe,
            &route.channel,
            params,
            header,
            cookie,
        )
        .await
    {
        Ok(auth) => auth,
        Err(error) => return write_channel_error(session, target, error).await,
    };
    serve_sse(
        session,
        dev_store,
        &target.channel_store_key,
        &stream,
        from_start,
        auth,
    )
    .await
}

/// The app's definition of `channel`, read from its registry on every request
/// so a hot reload's new definitions apply at once.
async fn channel_definition(
    app: &AppAuthorizer,
    channel: &str,
) -> std::result::Result<ChannelDefinitionMeta, ChannelError> {
    let definitions = tako_channels::live_store::internal_store_json(
        &app.endpoint,
        &app.internal_host,
        &app.internal_token,
        tako_channels::INTERNAL_CHANNEL_REGISTRY_PATH,
        None,
    )
    .await?;
    serde_json::from_value::<Vec<ChannelDefinitionMeta>>(definitions)
        .map_err(|_| ChannelError::AuthUnavailable)?
        .into_iter()
        .find(|meta| meta.channel == channel)
        .ok_or(ChannelError::NotDefined)
}

enum DevSocketAuth {
    Authorized(ChannelAuthResponse),
    /// Header auth arrives in the socket's first `tako.auth` frame.
    FirstFrame {
        app: AppAuthorizer,
        cookie: Option<String>,
    },
}

async fn serve_websocket(
    session: &mut Session,
    dev_store: &DevChannelStore,
    target: &crate::proxy::RouteTarget,
    stream: &ChannelStream,
    params: serde_json::Value,
    requested: ReplayFrom,
    auth: DevSocketAuth,
) -> Result<bool> {
    let store = store_for(dev_store, &target.channel_store_key).await?;
    let header =
        match tako_channels::websocket::build_websocket_upgrade_response(session.req_header()) {
            Ok(header) => header,
            Err(error) => return write_channel_error(session, target, error).await,
        };
    session
        .write_response_header(Box::new(header), false)
        .await?;
    let mut reader = tako_channels::websocket::WebSocketFrameReader::default();
    let (auth, requested) = match auth {
        DevSocketAuth::Authorized(auth) => (auth, requested),
        DevSocketAuth::FirstFrame { app, cookie } => match authorize_first_frame(
            session,
            &mut reader,
            &app,
            stream.channel(),
            params.clone(),
            cookie,
            requested,
        )
        .await?
        {
            Some(authorized) => authorized,
            None => return Ok(true),
        },
    };
    tako_channels::websocket::serve(session, &store, stream, requested, auth, params, reader).await
}

async fn store_for(dev_store: &DevChannelStore, app_id: &str) -> Result<Arc<ChannelStore>> {
    let dev_store = dev_store.clone();
    let app_id = app_id.to_owned();
    tokio::task::spawn_blocking(move || dev_store.store_for_app(&app_id))
        .await
        .map_err(|error| {
            pingora_core::Error::explain(pingora_core::ErrorType::InternalError, error.to_string())
        })
}

/// Production's status and body for a failed channel request. When the app
/// cannot answer, say so: the browser only sees the 503.
async fn write_channel_error(
    session: &mut Session,
    target: &crate::proxy::RouteTarget,
    error: ChannelError,
) -> Result<bool> {
    match &error {
        ChannelError::AuthUnavailable => tracing::warn!(
            app = %target.app_id,
            port = target.upstream_port,
            "channel authorization unavailable: the app did not answer its internal channel endpoints"
        ),
        ChannelError::Storage(message) => tracing::error!("dev channel storage error: {message}"),
        _ => {}
    }
    let (status, mut body) = tako_channels::authorize::error_response(&error);
    if matches!(error, ChannelError::AuthUnavailable) {
        body = serde_json::json!({
            "error": "Channel authorization unavailable: the app did not answer its channel registry or authorize endpoint"
        });
    }
    write_json(session, status, &body.to_string()).await
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
    let store = store_for(dev_store, &target.channel_store_key).await?;
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
    stream: &ChannelStream,
    from_start: bool,
    auth: ChannelAuthResponse,
) -> Result<bool> {
    let store = store_for(dev_store, app_id).await?;
    let ephemeral = if auth.ephemeral {
        Some(store.subscribe_ephemeral(stream.key()).map_err(|e| {
            pingora_core::Error::explain(pingora_core::ErrorType::InternalError, e.to_string())
        })?)
    } else {
        None
    };
    let resets = store.resets(stream.key());
    let after = if auth.ephemeral {
        None
    } else {
        // Start the stream's bookkeeping before resolving its head, as
        // production does: a stream first created by a later publish would
        // otherwise reset this subscriber's cursor.
        if let Err(e) = store.sync_channel_async(stream.key(), &auth).await {
            let msg = format!(r#"{{"error":"Failed to sync channel: {e}"}}"#);
            return write_json(session, 500, &msg).await;
        }
        let requested = match parse_message_id_cursor(
            session
                .req_header()
                .headers
                .get("last-event-id")
                .and_then(|value| value.to_str().ok()),
            "Last-Event-ID",
        ) {
            Ok(cursor) => cursor,
            Err(_) => {
                return write_json(session, 400, r#"{"error":"Invalid replay cursor"}"#).await;
            }
        };
        match store
            .replay_from_async(stream.key(), ReplayFrom::new(requested, from_start))
            .await
        {
            Ok(cursor) => cursor,
            Err(ChannelError::StaleCursor) => {
                return write_json(session, 410, r#"{"error":"Replay history expired"}"#).await;
            }
            Err(e) => {
                let msg = format!(r#"{{"error":"Failed to resolve cursor: {e}"}}"#);
                return write_json(session, 500, &msg).await;
            }
        }
    };

    let mut header = ResponseHeader::build(200, None)?;
    header.insert_header("Content-Type", "text/event-stream")?;
    header.insert_header("Cache-Control", "no-store")?;
    header.insert_header("Connection", "keep-alive")?;
    header.insert_header("Access-Control-Allow-Origin", "*")?;
    session
        .write_response_header(Box::new(header), false)
        .await?;

    if let Some(events) = ephemeral {
        return tako_channels::ephemeral::serve_events(session, events, &auth, stream).await;
    }
    tako_channels::sse::serve_events(session, &store, stream, after, &auth, resets).await
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
mod test_app;

#[cfg(test)]
mod auth_tests;

#[cfg(test)]
mod store_tests;

#[cfg(test)]
mod ephemeral_tests;

#[cfg(test)]
mod stream_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use tako_channels::{ChannelPublishPayload, parse_channel_route};

    fn lifecycle() -> ChannelAuthResponse {
        ChannelAuthResponse {
            ok: true,
            subject: None,
            transport: None,
            ephemeral: false,
            replay_window_ms: 10 * 60 * 1000,
            inactivity_ttl_ms: 0,
            keepalive_interval_ms: 25_000,
            max_connection_lifetime_ms: 2 * 60 * 60 * 1000,
        }
    }

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

    /// Replay ids are opaque SQLite cursors scoped to the store epoch and
    /// stream; only the trailing position is comparable across stores.
    fn position(id: &str) -> i64 {
        id.parse::<tako_channels::ReplayCursor>().unwrap();
        id.rsplit('.').next().unwrap().parse().unwrap()
    }

    #[test]
    fn publish_and_read_via_store() {
        let dev_store = DevChannelStore::new();
        let store = dev_store.store_for_app("demo");

        // Sync the channel first.
        store.sync_channel("test", &lifecycle()).unwrap();

        let msg = store
            .append(
                "test",
                &ChannelPublishPayload {
                    r#type: "message".to_string(),
                    data: serde_json::json!({"text": "hello"}),
                },
            )
            .unwrap();

        assert_eq!(position(&msg.id), 1);
        assert_eq!(msg.channel, "test");

        let messages = store.read_after("test", None, 100).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data, serde_json::json!({"text": "hello"}));
    }

    #[test]
    fn publish_increments_ids() {
        let dev_store = DevChannelStore::new();
        let store = dev_store.store_for_app("demo");

        store.sync_channel("ch", &lifecycle()).unwrap();

        let p = ChannelPublishPayload {
            r#type: "message".to_string(),
            data: serde_json::json!(null),
        };
        assert_eq!(position(&store.append("ch", &p).unwrap().id), 1);
        assert_eq!(position(&store.append("ch", &p).unwrap().id), 2);
        assert_eq!(position(&store.append("ch", &p).unwrap().id), 3);
    }

    #[test]
    fn dev_channel_store_keeps_apps_in_separate_dbs() {
        let dev_store = DevChannelStore::new();
        let payload = serde_json::json!({"type": "message", "data": {"text": "hello"}});

        let publication =
            TrustedPublication::parse("chat", &serde_json::Value::Null, payload).unwrap();
        let first = dev_store.publish("app/one", &publication).unwrap();
        let second = dev_store.publish("app/two", &publication).unwrap();

        assert_eq!(position(&first.id), 1);
        assert_eq!(position(&second.id), 1);
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

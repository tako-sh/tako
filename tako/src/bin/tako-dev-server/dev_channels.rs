//! Channel support for `tako dev`. Replay lives in each project's dev
//! database, `.tako/data/tako/channels.sqlite`, which the daemon serves with
//! its other local databases; presence stays in the daemon's memory.
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
    ChannelStoreConfig, ChannelStream, ChannelTransport, ReplayFrom, TrustedPublication,
    parse_channel_route, parse_message_id_cursor, parse_replay_from, parse_ws_last_message_id,
};

/// The dev proxy listens only on HTTPS, so a cookie-authenticated socket's
/// `Origin` must be the `https://` origin of the requested host.
const DEV_PROXY_IS_HTTPS: bool = true;
const INTERNAL_TOKEN_HEADER: &str = "x-tako-internal-token";

/// The project's dev replay database, relative to its directory.
const REPLAY_FILE: &str = ".tako/data/tako/channels.sqlite";

/// The daemon's channel stores, keyed by the app id channel requests and
/// internal-socket publishes name. Each registered app gets its replay
/// binding at registration; its store opens on first use.
#[derive(Clone, Default)]
pub struct DevChannelStore {
    apps: Arc<Mutex<HashMap<String, DevApp>>>,
}

struct DevApp {
    /// The registration whose binding this is.
    scope: String,
    connection: tako_core::DatabaseConnection,
    store: Option<Arc<ChannelStore>>,
}

fn store_key(app_id: &str) -> String {
    let app_id = app_id.trim();
    let app_id = if app_id.is_empty() { "app" } else { app_id };
    tako_core::deployment_app_id_filename(app_id)
}

impl DevChannelStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hand `app_id` its replay binding. A registration renamed since keeps
    /// one entry, under its new name.
    fn provision(&self, scope: &str, app_id: &str, connection: tako_core::DatabaseConnection) {
        let key = store_key(app_id);
        let mut apps = self.apps.lock().unwrap();
        apps.retain(|other, app| {
            let renamed = app.scope == scope && *other != key;
            if renamed && let Some(store) = &app.store {
                store.signal_gap(None);
            }
            !renamed
        });
        if apps
            .get(&key)
            .is_some_and(|app| app.connection == connection && app.scope == scope)
        {
            return;
        }
        if let Some(store) = apps.get(&key).and_then(|app| app.store.as_ref()) {
            store.signal_gap(None);
        }
        apps.insert(
            key,
            DevApp {
                scope: scope.into(),
                connection,
                store: None,
            },
        );
    }

    /// Forget the registration's store. Its live subscribers reload.
    fn release(&self, scope: &str) {
        self.apps.lock().unwrap().retain(|_, app| {
            if app.scope == scope
                && let Some(store) = &app.store
            {
                store.signal_gap(None);
            }
            app.scope != scope
        });
    }

    /// The app's replay store. Blocking: the first call opens the database.
    fn store_for_app(&self, app_id: &str) -> std::result::Result<Arc<ChannelStore>, ChannelError> {
        let key = store_key(app_id);
        let mut apps = self.apps.lock().unwrap();
        let app = apps.get_mut(&key).ok_or(ChannelError::NotProvisioned)?;
        if let Some(store) = &app.store {
            return Ok(store.clone());
        }
        let config = ChannelStoreConfig::hosted(
            &app.connection.url,
            &app.connection.auth_token,
            key.as_str(),
        );
        let store = Arc::new(ChannelStore::open_config(config)?);
        app.store = Some(store.clone());
        Ok(store)
    }

    /// Apply a server-side publication from the internal socket's
    /// `Command::ChannelPublish` path (channel `.publish()` from app or
    /// workflow code) to the stream its params select. Blocking.
    pub fn publish(
        &self,
        app_id: &str,
        publication: &TrustedPublication,
    ) -> std::result::Result<tako_channels::ChannelMessage, ChannelError> {
        self.store_for_app(app_id)?.publish_trusted(publication)
    }
}

/// The host scope of a registration's channel replay binding.
fn replay_scope(config_path: &str) -> String {
    format!("{config_path}#channels")
}

/// The project's replay database, as the binding the dev host serves.
fn replay_bindings(project_dir: &str) -> HashMap<String, String> {
    let file = std::path::Path::new(project_dir).join(REPLAY_FILE);
    HashMap::from([(
        tako_core::CHANNEL_REPLAY_BINDING.to_string(),
        file.display().to_string(),
    )])
}

/// Serve the project's dev replay database to the app registered at
/// `config_path`, creating the file on first use.
pub(crate) async fn provision(
    channels: &DevChannelStore,
    config_path: &str,
    project_dir: &str,
    app_id: &str,
) -> std::result::Result<(), String> {
    let scope = replay_scope(config_path);
    let connection = crate::databases::connect(&scope, &replay_bindings(project_dir))
        .await?
        .remove(tako_core::CHANNEL_REPLAY_BINDING)
        .ok_or("the channel replay binding was not served")?;
    channels.provision(&scope, app_id, connection);
    Ok(())
}

/// Stop serving the replay database of the app registered at `config_path`.
pub(crate) async fn release(channels: Option<&DevChannelStore>, config_path: &str) {
    let scope = replay_scope(config_path);
    if let Some(channels) = channels {
        channels.release(&scope);
    }
    crate::databases::release(&scope).await;
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
        let store = match store_for(dev_store, &target.channel_store_key).await {
            Ok(store) => store,
            Err(error) => return write_channel_error(session, target, error).await,
        };
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
    let websocket = route.operation == ChannelOperation::Connect;
    if websocket != session.as_downstream().is_upgrade_req() {
        return if websocket {
            write_json(session, 426, r#"{"error":"WebSocket upgrade required"}"#).await
        } else {
            write_json(
                session,
                400,
                r#"{"error":"SSE endpoint does not accept WebSocket upgrades"}"#,
            )
            .await
        };
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
    let store = match store_for(dev_store, &target.channel_store_key).await {
        Ok(store) => store,
        Err(error) => return write_channel_error(session, target, error).await,
    };
    let header =
        match tako_channels::websocket::build_websocket_upgrade_response(session.req_header()) {
            Ok(header) => header,
            Err(error) => return write_channel_error(session, target, error).await,
        };
    session
        .write_response_header(Box::new(header), false)
        .await?;
    let mut reader = tako_channels::websocket::WebSocketFrameReader::from_client();
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

/// The app's store, opened off the async executor: the hosted client blocks.
async fn store_for(
    dev_store: &DevChannelStore,
    app_id: &str,
) -> std::result::Result<Arc<ChannelStore>, ChannelError> {
    let dev_store = dev_store.clone();
    let app_id = app_id.to_owned();
    tokio::task::spawn_blocking(move || dev_store.store_for_app(&app_id))
        .await
        .map_err(|error| ChannelError::Storage(error.to_string()))?
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
    let (status, body) = error_response(&error);
    write_json(session, status, &body.to_string()).await
}

/// Production's status and body, with dev's own explanations.
fn error_response(error: &ChannelError) -> (u16, serde_json::Value) {
    let (status, body) = tako_channels::authorize::error_response(error);
    let explained = match error {
        ChannelError::AuthUnavailable => {
            "Channel authorization unavailable: the app did not answer its channel registry or authorize endpoint"
        }
        ChannelError::NotProvisioned => {
            "Channels unavailable: the app is not registered with the dev server"
        }
        _ => return (status, body),
    };
    (status, serde_json::json!({ "error": explained }))
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
            Ok(Some(route))
                if route.channel.starts_with(STORE_CHANNEL_PREFIX)
                    && route.operation == ChannelOperation::Subscribe =>
            {
                route.channel
            }
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
    let store = match store_for(dev_store, &target.channel_store_key).await {
        Ok(store) => store,
        Err(error) => return write_channel_error(session, target, error).await,
    };
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
    let store = match store_for(dev_store, app_id).await {
        Ok(store) => store,
        Err(error) => {
            let (status, body) = error_response(&error);
            return write_json(session, status, &body.to_string()).await;
        }
    };
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
        let route = parse_channel_route("/_tako/channels/sse/demo-broadcast")
            .unwrap()
            .unwrap();
        assert_eq!(route.channel, "demo-broadcast");
    }

    #[test]
    fn parse_rejects_nested_channel_path() {
        assert!(parse_channel_route("/_tako/channels/sse/demo-broadcast/messages").is_err());
    }

    #[test]
    fn parse_returns_none_for_non_channel_path() {
        assert!(parse_channel_route("/api/hello").unwrap().is_none());
    }

    #[test]
    fn parse_returns_error_for_empty_channel_path() {
        assert!(parse_channel_route("/_tako/channels/").is_err());
    }

    /// Replay ids are opaque cursors scoped to the database epoch and
    /// stream; only the trailing position is comparable across stores.
    fn position(id: &str) -> i64 {
        id.parse::<tako_channels::ReplayCursor>().unwrap();
        id.rsplit('.').next().unwrap().parse().unwrap()
    }

    #[test]
    fn registrations_serve_the_projects_dev_replay_database() {
        assert_eq!(replay_scope("/p/tako.toml"), "/p/tako.toml#channels");
        assert_eq!(
            replay_bindings("/p"),
            HashMap::from([(
                "@channels".to_string(),
                "/p/.tako/data/tako/channels.sqlite".to_string()
            )])
        );
    }

    #[tokio::test]
    async fn publish_and_read_via_store() {
        let (dev_store, _server) = test_app::registered_as("demo").await;
        let store = store_for(&dev_store, "demo").await.unwrap();
        store
            .sync_channel_async("test", &lifecycle())
            .await
            .unwrap();
        let payload = ChannelPublishPayload {
            r#type: "message".to_string(),
            data: serde_json::json!({"text": "hello"}),
        };
        let msg = store.append_async("test", &payload).await.unwrap();
        assert_eq!(position(&msg.id), 1);
        assert_eq!(msg.channel, "test");

        let messages = store.read_after_async("test", None, 100).await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data, serde_json::json!({"text": "hello"}));
    }

    #[tokio::test]
    async fn publish_increments_ids() {
        let (dev_store, _server) = test_app::registered_as("demo").await;
        let store = store_for(&dev_store, "demo").await.unwrap();
        store.sync_channel_async("ch", &lifecycle()).await.unwrap();
        let p = ChannelPublishPayload {
            r#type: "message".to_string(),
            data: serde_json::json!(null),
        };
        for id in 1..=3 {
            assert_eq!(
                position(&store.append_async("ch", &p).await.unwrap().id),
                id
            );
        }
    }

    #[tokio::test]
    async fn dev_channel_store_keeps_apps_in_separate_histories() {
        let server = tako_db::testing::TestServer::start();
        let database = server.database("/p/one.toml#channels");
        let dev_store = DevChannelStore::new();
        // Two registrations of one project share its database file.
        for (scope, app) in [
            ("/p/one.toml#channels", "app/one"),
            ("/p/two.toml#channels", "app/two"),
        ] {
            let token = server.bind(scope, &database.id);
            dev_store.provision(scope, app, test_app::connection(&server, token));
        }
        let payload = serde_json::json!({"type": "message", "data": {"text": "hello"}});
        let publication =
            TrustedPublication::parse("chat", &serde_json::Value::Null, payload).unwrap();
        let first = test_app::publish(&dev_store, "app/one", publication.clone())
            .await
            .unwrap();
        let second = test_app::publish(&dev_store, "app/two", publication)
            .await
            .unwrap();

        assert_eq!(position(&first.id), 1);
        assert_eq!(position(&second.id), 1);
        for app in ["app/one", "app/two"] {
            assert_eq!(test_app::read(&dev_store, app, "chat").await.len(), 1);
        }
    }

    #[tokio::test]
    async fn released_and_renamed_registrations_keep_one_store() {
        let server = tako_db::testing::TestServer::start();
        let scope = "/p/tako.toml#channels";
        let database = server.database(scope);
        let connection = test_app::connection(&server, database.token);
        let dev_store = DevChannelStore::new();
        let publication = TrustedPublication::parse(
            "chat",
            &serde_json::Value::Null,
            serde_json::json!({"type": "message", "data": 1}),
        )
        .unwrap();
        assert!(matches!(
            test_app::publish(&dev_store, "demo", publication.clone()).await,
            Err(ChannelError::NotProvisioned)
        ));
        dev_store.provision(scope, "demo", connection.clone());
        let store = store_for(&dev_store, "demo").await.unwrap();
        let mut gap = store.resets("chat");
        test_app::publish(&dev_store, "demo", publication.clone())
            .await
            .unwrap();
        // Registering again keeps the open store.
        dev_store.provision(scope, "demo", connection.clone());
        assert!(Arc::ptr_eq(
            &store,
            &store_for(&dev_store, "demo").await.unwrap()
        ));
        assert!(!gap.has_changed().unwrap());
        // A renamed registration keeps one store, under its new name.
        dev_store.provision(scope, "renamed", connection);
        assert!(gap.has_changed().unwrap());
        gap.borrow_and_update();
        assert!(matches!(
            store_for(&dev_store, "demo").await,
            Err(ChannelError::NotProvisioned)
        ));
        assert!(
            test_app::read(&dev_store, "renamed", "chat")
                .await
                .is_empty()
        );
        dev_store.release(scope);
        assert!(matches!(
            test_app::publish(&dev_store, "renamed", publication).await,
            Err(ChannelError::NotProvisioned)
        ));
        let (status, body) = error_response(&ChannelError::NotProvisioned);
        assert_eq!(status, 503);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("not registered with the dev server")
        );
    }
}

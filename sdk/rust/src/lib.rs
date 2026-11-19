use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::OnceLock;

use serde::Deserialize;

#[cfg(all(test, unix, feature = "axum"))]
mod axum_shutdown_tests;
pub mod error;
pub mod network;
pub mod presence;
#[cfg(feature = "axum")]
mod presence_http;
#[cfg(feature = "schema")]
pub mod schema;
pub mod stores;
#[cfg(feature = "axum")]
mod stores_http;
#[cfg(all(test, unix))]
mod wire_fixtures;
pub use error::{ErrorCode, Rejected};
pub use presence::{
    PresenceAuth, PresenceDefinition, PresenceGrant, PresenceMember, PresenceRegistry,
    PresenceSnapshot, PresenceVerifyInput,
};
pub use stores::{
    StoreAuth, StoreAuthorization, StoreDefinition, StoreError, StoreGrant, StoreHandle,
    StoreLoadInput, StoreOfflineConfig, StoreRegistry, StoreSnapshot, StoreVerifyInput,
};

pub const BOOTSTRAP_DATA_ENV: &str = "TAKO_BOOTSTRAP_DATA";
pub const INTERNAL_TOKEN_HEADER: &str = "X-Tako-Internal-Token";

static BOOTSTRAP: OnceLock<Result<Bootstrap, BootstrapError>> = OnceLock::new();

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Bootstrap {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    secrets: HashMap<String, String>,
    #[serde(default)]
    storages: HashMap<String, serde_json::Value>,
    #[serde(default)]
    databases: HashMap<String, Database>,
}

/// A connection to a Tako-hosted database bound to the app in `tako.toml`
/// (`[envs.<env>].databases`). Tako serves it over the libSQL HTTP protocol
/// next to the app, so libSQL clients connect with [`Database::url`] and
/// [`Database::auth_token`]:
///
/// ```no_run
/// let main = tako::database("main").expect("a database bound as main");
/// // libsql::Builder::new_remote(main.url().into(), main.auth_token().into())
/// # let _ = main;
/// ```
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct Database {
    url: String,
    #[serde(rename = "authToken")]
    auth_token: String,
}

impl Database {
    /// The database endpoint.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The token that authorizes this app's binding. Treat it as a secret.
    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }
}

impl fmt::Debug for Database {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Database")
            .field("url", &self.url)
            .field("auth_token", &"[REDACTED]")
            .finish()
    }
}

impl Bootstrap {
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub fn secret(&self, name: &str) -> Option<&str> {
        self.secrets.get(name).map(String::as_str)
    }

    pub fn secrets(&self) -> &HashMap<String, String> {
        &self.secrets
    }

    pub fn storages(&self) -> &HashMap<String, serde_json::Value> {
        &self.storages
    }

    /// The database bound as `name`.
    pub fn database(&self, name: &str) -> Option<&Database> {
        self.databases.get(name)
    }

    /// Every database binding, by name.
    pub fn databases(&self) -> &HashMap<String, Database> {
        &self.databases
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapError {
    InvalidJson(String),
    InvalidEnvelope,
    ReadFd(String),
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson(err) => write!(f, "invalid Tako bootstrap JSON: {err}"),
            Self::InvalidEnvelope => write!(
                f,
                "Tako bootstrap must be {{token: string, secrets: object, storages?: object, databases?: object}}",
            ),
            Self::ReadFd(err) => write!(f, "failed to read Tako bootstrap fd: {err}"),
        }
    }
}

impl std::error::Error for BootstrapError {}

pub fn bootstrap() -> Result<&'static Bootstrap, BootstrapError> {
    match BOOTSTRAP.get_or_init(load_bootstrap) {
        Ok(bootstrap) => Ok(bootstrap),
        Err(err) => Err(err.clone()),
    }
}

/// The secret `name`, or `None` when it is not configured.
///
/// # Panics
///
/// In a Compose service that isn't marked `sdk: true` in `x-tako`, with a
/// message that names the missing setting.
pub fn secret(name: &str) -> Option<String> {
    if let Some(hint) = compose_sdk_hint() {
        panic!("{hint}");
    }
    bootstrap()
        .ok()
        .and_then(|bootstrap| bootstrap.secret(name).map(str::to_string))
}

/// Set by Tako on every Compose service with `x-tako`.
pub const COMPOSE_SERVICE_ENV: &str = "TAKO_COMPOSE_SERVICE";

/// Why the Tako runtime is unavailable in a Compose service that isn't
/// marked `sdk: true`: Tako marks every `x-tako` service but gives the
/// runtime only to SDK ones. `None` outside Compose and in SDK services.
pub fn compose_sdk_hint() -> Option<String> {
    let service = std::env::var(COMPOSE_SERVICE_ENV)
        .ok()
        .filter(|service| !service.is_empty())?;
    if internal_token().is_some() {
        return None;
    }
    Some(format!(
        "this Compose service ('{service}') isn't marked `sdk: true` in x-tako, so it has no Tako runtime for secrets, channels or workflows; add `sdk: true` to services.{service}.x-tako in the Compose file"
    ))
}

/// The database bound to this app as `name`.
pub fn database(name: &str) -> Option<Database> {
    bootstrap()
        .ok()
        .and_then(|bootstrap| bootstrap.database(name).cloned())
}

pub fn internal_token() -> Option<&'static str> {
    bootstrap().ok().and_then(Bootstrap::token)
}

pub fn std_listener() -> io::Result<TcpListener> {
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("PORT").unwrap_or_else(|_| "0".to_string());
    let listener = TcpListener::bind(format!("{host}:{port}"))?;
    report_ready_port(listener.local_addr()?.port())?;
    Ok(listener)
}

#[cfg(feature = "tokio")]
pub async fn listener() -> io::Result<tokio::net::TcpListener> {
    let listener = std_listener()?;
    listener.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(listener)
}

pub fn is_internal_status_request(host: Option<&str>, path: &str) -> bool {
    path == "/status" && host.is_some_and(is_internal_host)
}

/// Tako's server addresses SDK endpoints as `<app>.tako`: exactly one label.
/// Multi-label `.tako` names such as `api.prod.tako` are private network
/// traffic and belong to the application.
pub(crate) fn is_internal_host(host: &str) -> bool {
    host_without_port(host)
        .strip_suffix(".tako")
        .is_some_and(|app| !app.is_empty() && !app.contains('.'))
}

pub fn status_body() -> serde_json::Value {
    serde_json::json!({
        "status": "healthy",
        "pid": std::process::id(),
    })
}

fn load_bootstrap() -> Result<Bootstrap, BootstrapError> {
    if let Some(data) = read_bootstrap_fd()? {
        clear_bootstrap_env();
        return parse_bootstrap_data(&data);
    }
    if let Some(data) = read_bootstrap_env() {
        return parse_bootstrap_data(&data);
    }
    Ok(Bootstrap::default())
}

fn parse_bootstrap_data(data: &str) -> Result<Bootstrap, BootstrapError> {
    let value: serde_json::Value =
        serde_json::from_str(data).map_err(|err| BootstrapError::InvalidJson(err.to_string()))?;
    if !value.is_object()
        || !value.get("token").is_some_and(serde_json::Value::is_string)
        || !value
            .get("secrets")
            .is_some_and(serde_json::Value::is_object)
        || ["storages", "databases"]
            .iter()
            .any(|key| value.get(key).is_some_and(|field| !field.is_object()))
    {
        return Err(BootstrapError::InvalidEnvelope);
    }
    serde_json::from_value(value).map_err(|_| BootstrapError::InvalidEnvelope)
}

fn read_bootstrap_env() -> Option<String> {
    let data = std::env::var(BOOTSTRAP_DATA_ENV).ok()?;
    if data.is_empty() {
        return None;
    }
    clear_bootstrap_env();
    Some(data)
}

fn clear_bootstrap_env() {
    unsafe {
        std::env::remove_var(BOOTSTRAP_DATA_ENV);
    }
}

/// Marks the bootstrap pipe close-on-exec as the program loads, before
/// `main`: the SDK reads it only on first use, and a subprocess started
/// before then must not inherit it and read the app's secrets and database
/// tokens. The process itself can still read it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[used]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
static CLOSE_BOOTSTRAP_ON_EXEC: extern "C" fn() = close_bootstrap_on_exec;

#[cfg(any(target_os = "linux", target_os = "macos"))]
extern "C" fn close_bootstrap_on_exec() {
    if fd_is_fifo(3) {
        unsafe {
            let flags = libc::fcntl(3, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(3, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

#[cfg(unix)]
fn read_bootstrap_fd() -> Result<Option<String>, BootstrapError> {
    if !fd_is_fifo(3) {
        return Ok(None);
    }
    use std::fs::File;
    use std::os::fd::FromRawFd;

    let mut data = String::new();
    let mut file = unsafe { File::from_raw_fd(3) };
    file.read_to_string(&mut data)
        .map_err(|err| BootstrapError::ReadFd(err.to_string()))?;
    Ok(Some(data))
}

#[cfg(not(unix))]
fn read_bootstrap_fd() -> Result<Option<String>, BootstrapError> {
    Ok(None)
}

#[cfg(unix)]
fn report_ready_port(port: u16) -> io::Result<()> {
    if !fd_is_fifo(4) {
        return Ok(());
    }
    use std::fs::File;
    use std::os::fd::FromRawFd;

    let mut file = unsafe { File::from_raw_fd(4) };
    writeln!(file, "{port}")?;
    Ok(())
}

#[cfg(not(unix))]
fn report_ready_port(_port: u16) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn fd_is_fifo(fd: libc::c_int) -> bool {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe { libc::fstat(fd, stat.as_mut_ptr()) };
    if rc != 0 {
        return false;
    }
    let stat = unsafe { stat.assume_init() };
    stat.st_mode & libc::S_IFMT == libc::S_IFIFO
}

fn host_without_port(host: &str) -> &str {
    host.split(':').next().unwrap_or(host)
}

#[cfg(feature = "axum")]
pub mod axum {
    use std::io;
    use std::sync::Arc;

    use ::axum::Router;
    use ::axum::body::Body;
    use ::axum::http::header::{CONTENT_TYPE, HOST, HeaderName, HeaderValue};
    use ::axum::http::{Request, StatusCode};
    use ::axum::middleware::{self, Next};
    use ::axum::response::{IntoResponse, Response};

    pub fn router<S>(router: Router<S>) -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        router.layer(middleware::from_fn(status_middleware))
    }

    pub async fn serve(router: Router) -> io::Result<()> {
        serve_router(self::router(router)).await
    }

    async fn serve_router(router: Router) -> io::Result<()> {
        // Register before reporting readiness. Router/listener-only adapters
        // leave signal ownership to the application's own server loop.
        let shutdown = shutdown_signal()?;
        let listener = crate::listener().await?;
        ::axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(io::Error::other)
    }

    fn shutdown_signal() -> io::Result<impl Future<Output = ()> + Send> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut terminate = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
            Ok(async move {
                tokio::select! {
                    _ = terminate.recv() => {},
                    _ = interrupt.recv() => {},
                }
            })
        }
        #[cfg(not(unix))]
        Ok(async {
            let _ = tokio::signal::ctrl_c().await;
        })
    }

    /// Install authenticated presence discovery and authorization alongside status probes.
    pub fn router_with_presence<S>(
        router: Router<S>,
        registry: Arc<crate::PresenceRegistry>,
    ) -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        self::router(router.layer(middleware::from_fn_with_state(
            registry,
            crate::presence_http::presence_middleware,
        )))
    }

    pub async fn serve_with_presence(
        router: Router,
        registry: Arc<crate::PresenceRegistry>,
    ) -> io::Result<()> {
        serve_router(router_with_presence(router, registry)).await
    }

    /// Install authenticated Store discovery, authorization, and loader endpoints.
    pub fn router_with_stores<S>(router: Router<S>, stores: Arc<crate::StoreRegistry>) -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        realtime_router(
            router,
            crate::stores_http::RealtimeState {
                presence: None,
                stores: Some(stores),
            },
        )
    }

    /// Install presence and Stores together so their channel registries are merged.
    pub fn router_with_realtime<S>(
        router: Router<S>,
        presence: Arc<crate::PresenceRegistry>,
        stores: Arc<crate::StoreRegistry>,
    ) -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        realtime_router(
            router,
            crate::stores_http::RealtimeState {
                presence: Some(presence),
                stores: Some(stores),
            },
        )
    }

    fn realtime_router<S>(router: Router<S>, state: crate::stores_http::RealtimeState) -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        self::router(router.layer(middleware::from_fn_with_state(
            state,
            crate::stores_http::realtime_middleware,
        )))
    }

    pub async fn serve_with_stores(
        router: Router,
        stores: Arc<crate::StoreRegistry>,
    ) -> io::Result<()> {
        serve_router(router_with_stores(router, stores)).await
    }

    pub async fn serve_with_realtime(
        router: Router,
        presence: Arc<crate::PresenceRegistry>,
        stores: Arc<crate::StoreRegistry>,
    ) -> io::Result<()> {
        serve_router(router_with_realtime(router, presence, stores)).await
    }

    async fn status_middleware(request: Request<Body>, next: Next) -> Response {
        let host = request
            .headers()
            .get(HOST)
            .and_then(|value| value.to_str().ok());
        if crate::is_internal_status_request(host, request.uri().path()) {
            let request_token = request
                .headers()
                .get(crate::INTERNAL_TOKEN_HEADER)
                .and_then(|value| value.to_str().ok());
            return if crate::internal_token().is_some_and(|token| Some(token) == request_token) {
                status_response()
            } else {
                (StatusCode::FORBIDDEN, r#"{"error":"Forbidden"}"#).into_response()
            };
        }
        next.run(request).await
    }

    fn status_response() -> Response {
        let body = crate::status_body().to_string();
        let mut response = (StatusCode::OK, body).into_response();
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(token) =
            crate::internal_token().and_then(|token| HeaderValue::from_str(token).ok())
        {
            response
                .headers_mut()
                .insert(HeaderName::from_static("x-tako-internal-token"), token);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    #[test]
    fn compose_services_without_sdk_explain_the_missing_marker_on_first_use() {
        #[cfg(unix)]
        if !in_subprocess(
            "tests::compose_services_without_sdk_explain_the_missing_marker_on_first_use",
        ) {
            return;
        }
        let _guard = env_lock().lock().unwrap();
        unsafe {
            std::env::remove_var(BOOTSTRAP_DATA_ENV);
            std::env::remove_var("TAKO_INTERNAL_SOCKET");
            std::env::remove_var("TAKO_APP_NAME");
            std::env::set_var(COMPOSE_SERVICE_ENV, "web");
        }
        let hint = compose_sdk_hint().unwrap();
        assert!(
            hint.contains("isn't marked `sdk: true` in x-tako"),
            "{hint}"
        );
        assert!(hint.contains("services.web.x-tako"), "{hint}");
        let error = crate::network::Client::from_env().unwrap_err();
        assert_eq!(error.to_string(), hint);
        assert_eq!(crate::StoreError::RuntimeUnavailable.to_string(), hint);
        let panic = std::panic::catch_unwind(|| secret("API_KEY")).unwrap_err();
        assert_eq!(panic.downcast_ref::<String>(), Some(&hint));
        unsafe {
            std::env::remove_var(COMPOSE_SERVICE_ENV);
        }
        assert_eq!(compose_sdk_hint(), None);
        assert_eq!(secret("API_KEY"), None);
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[cfg(unix)]
    fn in_subprocess(test: &str) -> bool {
        const CHILD_ENV: &str = "TAKO_TEST_FD_CHILD";
        if std::env::var(CHILD_ENV).as_deref() == Ok(test) {
            return true;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD_ENV, test)
            .status()
            .unwrap();
        assert!(status.success(), "{test} subprocess failed: {status}");
        false
    }

    #[test]
    fn parses_bootstrap_envelope() {
        let bootstrap = parse_bootstrap_data(
            r#"{"token":"tok","secrets":{"DATABASE_URL":"postgres://db"},"storages":{}}"#,
        )
        .unwrap();

        assert_eq!(bootstrap.token(), Some("tok"));
        assert_eq!(bootstrap.secret("DATABASE_URL"), Some("postgres://db"));
    }

    #[test]
    fn parses_database_bindings() {
        let bootstrap = parse_bootstrap_data(
            r#"{"token":"tok","secrets":{},"databases":{"main":{"url":"http://127.0.0.1:9845","authToken":"secret"}}}"#,
        )
        .unwrap();
        let main = bootstrap.database("main").unwrap();
        assert_eq!(main.url(), "http://127.0.0.1:9845");
        assert_eq!(main.auth_token(), "secret");
        assert!(!format!("{main:?}").contains("secret"));
        assert!(bootstrap.database("other").is_none());

        let err =
            parse_bootstrap_data(r#"{"token":"tok","secrets":{},"databases":[]}"#).unwrap_err();
        assert_eq!(err, BootstrapError::InvalidEnvelope);
    }

    #[test]
    fn rejects_missing_secret_object() {
        let err = parse_bootstrap_data(r#"{"token":"tok"}"#).unwrap_err();

        assert_eq!(err, BootstrapError::InvalidEnvelope);
    }

    #[test]
    fn reads_bootstrap_env_once() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        unsafe {
            std::env::set_var(BOOTSTRAP_DATA_ENV, "data");
        }

        assert_eq!(read_bootstrap_env().as_deref(), Some("data"));
        assert_eq!(std::env::var(BOOTSTRAP_DATA_ENV).ok(), None);
        assert_eq!(read_bootstrap_env(), None);
    }

    #[cfg(unix)]
    #[test]
    fn load_bootstrap_prefers_fd_over_env() {
        use std::os::fd::FromRawFd;

        // FD 3 is process-wide: replacing it can close a parallel test's socket.
        if !in_subprocess("tests::load_bootstrap_prefers_fd_over_env") {
            return;
        }

        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        unsafe {
            std::env::set_var(
                BOOTSTRAP_DATA_ENV,
                r#"{"token":"env-token","secrets":{"KEY":"env"},"storages":{}}"#,
            );
        }

        let original_fd3 = unsafe { libc::dup(3) };
        let (read_fd, write_fd) = {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            (fds[0], fds[1])
        };
        let write_file = unsafe { std::fs::File::from_raw_fd(write_fd) };
        write!(
            &write_file,
            r#"{{"token":"fd-token","secrets":{{"KEY":"fd"}},"storages":{{}}}}"#
        )
        .unwrap();
        drop(write_file);
        assert_eq!(unsafe { libc::dup2(read_fd, 3) }, 3);
        if read_fd != 3 {
            unsafe {
                libc::close(read_fd);
            }
        }

        let bootstrap = load_bootstrap().unwrap();

        if original_fd3 >= 0 {
            assert_eq!(unsafe { libc::dup2(original_fd3, 3) }, 3);
            unsafe {
                libc::close(original_fd3);
            }
        }

        assert_eq!(bootstrap.token(), Some("fd-token"));
        assert_eq!(bootstrap.secret("KEY"), Some("fd"));
        assert_eq!(std::env::var(BOOTSTRAP_DATA_ENV).ok(), None);
    }

    #[test]
    fn recognizes_internal_status_host() {
        assert!(is_internal_status_request(Some("app.tako"), "/status"));
        assert!(is_internal_status_request(Some("app.tako:3000"), "/status"));
        assert!(!is_internal_status_request(Some("example.com"), "/status"));
        assert!(!is_internal_status_request(Some("app.tako"), "/"));
        assert!(!is_internal_status_request(
            Some("api.prod.tako"),
            "/status"
        ));
        assert!(!is_internal_status_request(Some(".tako"), "/status"));
    }

    #[test]
    fn binds_std_listener_from_host_and_port() {
        // The readiness probe consumes FD 4 when it is a pipe. In the parallel
        // harness that pipe could belong to another test or subprocess spawn.
        #[cfg(unix)]
        if !in_subprocess("tests::binds_std_listener_from_host_and_port") {
            return;
        }

        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        unsafe {
            std::env::set_var("HOST", "127.0.0.1");
            std::env::set_var("PORT", "0");
        }

        let listener = std_listener().unwrap();
        assert!(listener.local_addr().unwrap().port() > 0);

        unsafe {
            std::env::remove_var("HOST");
            std::env::remove_var("PORT");
        }
    }
}

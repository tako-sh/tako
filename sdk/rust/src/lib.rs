use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::OnceLock;

use serde::Deserialize;

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
                "Tako bootstrap must be {{token: string, secrets: object, storages?: object}}",
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

pub fn secret(name: &str) -> Option<String> {
    bootstrap()
        .ok()
        .and_then(|bootstrap| bootstrap.secret(name).map(str::to_string))
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
    path == "/status"
        && host
            .map(host_without_port)
            .is_some_and(|host| host.ends_with(".tako"))
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
        || value
            .get("storages")
            .is_some_and(|storages| !storages.is_object())
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
        let listener = crate::listener().await?;
        ::axum::serve(listener, self::router(router))
            .await
            .map_err(io::Error::other)
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

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
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
    }

    #[test]
    fn binds_std_listener_from_host_and_port() {
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

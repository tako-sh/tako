use serde::Deserialize;
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, ToSocketAddrs},
    os::fd::FromRawFd,
};

const BOOTSTRAP_FD: i32 = 3;
const READINESS_FD: i32 = 4;
pub const INTERNAL_STATUS_PATH: &str = "/status";
pub const INTERNAL_TOKEN_HEADER: &str = "x-tako-internal-token";

#[derive(Debug, Clone)]
pub struct BindOptions {
    pub host_env: &'static str,
    pub port_env: &'static str,
}

impl Default for BindOptions {
    fn default() -> Self {
        Self {
            host_env: "HOST",
            port_env: "PORT",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Bootstrap {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub secrets: HashMap<String, String>,
    #[serde(default)]
    pub storages: serde_json::Value,
}

pub fn bind_listener() -> Result<TcpListener, Error> {
    bind_listener_with(BindOptions::default())
}

pub fn bind_listener_with(options: BindOptions) -> Result<TcpListener, Error> {
    let host = std::env::var(options.host_env).unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var(options.port_env).unwrap_or_else(|_| "0".to_string());
    let addr = resolve_addr(&host, &port)?;
    let listener = TcpListener::bind(addr)?;
    report_ready(listener.local_addr()?.port())?;
    Ok(listener)
}

pub fn read_bootstrap() -> Result<Bootstrap, Error> {
    read_bootstrap_from_fd(BOOTSTRAP_FD)
}

pub fn report_ready(port: u16) -> Result<(), Error> {
    write_ready_to_fd(READINESS_FD, port)
}

pub fn is_internal_status_request(
    host: &str,
    path: &str,
    token_header: Option<&str>,
    app_name: &str,
    bootstrap: &Bootstrap,
) -> bool {
    let base_app = app_name.split('/').next().unwrap_or(app_name);
    host == format!("{base_app}.tako")
        && path == INTERNAL_STATUS_PATH
        && token_header == Some(bootstrap.token.as_str())
}

pub fn internal_status_response(app_name: &str, bootstrap: &Bootstrap) -> InternalStatusResponse {
    InternalStatusResponse {
        status: 200,
        token_header_name: INTERNAL_TOKEN_HEADER,
        token_header_value: bootstrap.token.clone(),
        body: format!(
            r#"{{"status":"healthy","app":"{}"}}"#,
            escape_json_string(app_name)
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalStatusResponse {
    pub status: u16,
    pub token_header_name: &'static str,
    pub token_header_value: String,
    pub body: String,
}

fn resolve_addr(host: &str, port: &str) -> Result<SocketAddr, Error> {
    let port: u16 = port
        .parse()
        .map_err(|_| Error::InvalidPort(port.to_string()))?;
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::InvalidHost(host.to_string()))
}

fn read_bootstrap_from_fd(fd: i32) -> Result<Bootstrap, Error> {
    ensure_fd_open(fd)?;
    // SAFETY: Tako passes fd 3 as a child-owned read pipe. This function takes
    // ownership and closes it after reading the JSON bootstrap envelope.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut raw = String::new();
    file.read_to_string(&mut raw)?;
    Ok(serde_json::from_str(&raw)?)
}

fn write_ready_to_fd(fd: i32, port: u16) -> Result<(), Error> {
    ensure_fd_open(fd)?;
    // SAFETY: Tako passes fd 4 as a child-owned write pipe. This function takes
    // ownership and closes it after writing the selected listen port.
    let mut file = unsafe { File::from_raw_fd(fd) };
    writeln!(file, "{port}")?;
    Ok(())
}

fn ensure_fd_open(fd: i32) -> Result<(), Error> {
    // SAFETY: `fcntl` with `F_GETFD` only inspects the descriptor table and
    // does not take ownership of the fd.
    let result = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if result == -1 {
        return Err(Error::MissingFd(fd));
    }
    Ok(())
}

fn escape_json_string(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("required Tako fd {0} is not open")]
    MissingFd(i32),
    #[error("invalid host: {0}")]
    InvalidHost(String),
    #[error("invalid port: {0}")]
    InvalidPort(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_defaults_missing_optional_fields() {
        let bootstrap: Bootstrap = serde_json::from_str(r#"{"token":"abc"}"#).unwrap();
        assert_eq!(bootstrap.token, "abc");
        assert!(bootstrap.secrets.is_empty());
    }

    #[test]
    fn invalid_port_is_rejected() {
        let err = resolve_addr("127.0.0.1", "nope").unwrap_err();
        assert!(matches!(err, Error::InvalidPort(_)));
    }

    #[test]
    fn internal_status_request_requires_host_path_and_token() {
        let bootstrap = Bootstrap {
            token: "secret".to_string(),
            secrets: HashMap::new(),
            storages: serde_json::Value::Null,
        };

        assert!(is_internal_status_request(
            "cloud.tako",
            "/status",
            Some("secret"),
            "cloud/production",
            &bootstrap
        ));
        assert!(!is_internal_status_request(
            "cloud.example.com",
            "/status",
            Some("secret"),
            "cloud/production",
            &bootstrap
        ));
        assert!(!is_internal_status_request(
            "cloud.tako",
            "/status",
            Some("wrong"),
            "cloud/production",
            &bootstrap
        ));
    }

    #[test]
    fn internal_status_response_echoes_token_header() {
        let bootstrap = Bootstrap {
            token: "secret".to_string(),
            secrets: HashMap::new(),
            storages: serde_json::Value::Null,
        };

        let response = internal_status_response("cloud/production", &bootstrap);
        assert_eq!(response.status, 200);
        assert_eq!(response.token_header_name, INTERNAL_TOKEN_HEADER);
        assert_eq!(response.token_header_value, "secret");
        assert!(response.body.contains(r#""status":"healthy""#));
    }
}

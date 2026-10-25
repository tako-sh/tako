//! Reactive backend data with authorized reads and scoped invalidations.
//!
//! The Tako proxy validates parameters and authorizes every read and live
//! subscription. Call [`StoreHandle::invalidate_blocking`] (or `invalidate` with
//! the `tokio` feature) after committing a write to refresh subscribed clients.

use crate::presence::{PresenceAuth, PresenceGrant, PresenceHeaderValue, PresenceVerifyInput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io::{Read, Write};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

pub const STORE_CHANNEL_PREFIX: &str = "tako.store:";
pub(crate) const MAX_STORE_BYTES: usize = 1 << 20;
const MAX_SCOPE_BYTES: usize = 16 << 10;
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Header and cookie extraction policy, shared with presence.
pub type StoreAuth = PresenceAuth;
/// An authorization grant whose optional subject is supplied to the loader.
pub type StoreGrant = PresenceGrant;

#[derive(Debug, Clone)]
pub struct StoreVerifyInput {
    pub store: String,
    pub params: Value,
    pub header: Option<PresenceHeaderValue>,
    pub cookie: Option<String>,
}

/// Input sent to a loader after the proxy has validated and authorized a read.
#[derive(Debug, Clone, Deserialize)]
pub struct StoreLoadInput {
    pub store: String,
    pub params: Value,
    #[serde(default)]
    pub subject: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreOfflineConfig {
    pub max_age_ms: u64,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreSnapshot {
    pub data: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offline: Option<StoreOfflineConfig>,
}

/// Store failures are returned to application code; internal HTTP handlers
/// return generic errors so loader or socket details never reach a browser.
#[derive(Debug)]
pub enum StoreError {
    InvalidDefinition(&'static str),
    InvalidParams,
    ScopeTooLarge,
    TooLarge,
    Loader(String),
    RuntimeUnavailable,
    Timeout,
    Io(std::io::Error),
    Protocol,
    Rejected,
}

impl StoreError {
    pub fn loader(error: impl fmt::Display) -> Self {
        Self::Loader(error.to_string())
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDefinition(message) => write!(f, "invalid Store definition: {message}"),
            Self::InvalidParams => f.write_str("Store parameters must be a JSON object"),
            Self::ScopeTooLarge => f.write_str("Store parameters exceed 16 KiB"),
            Self::TooLarge => f.write_str("Store payload exceeds 1 MiB"),
            Self::Loader(message) => write!(f, "Store loader failed: {message}"),
            Self::RuntimeUnavailable => f.write_str("Store invalidation requires the Tako runtime"),
            Self::Timeout => f.write_str("Store invalidation timed out"),
            Self::Io(error) => write!(f, "Store invalidation transport failed: {error}"),
            Self::Protocol => f.write_str("invalid Store invalidation response"),
            Self::Rejected => f.write_str("Store invalidation was rejected"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        if matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ) {
            Self::Timeout
        } else {
            Self::Io(error)
        }
    }
}

type LoadFuture = Pin<Box<dyn Future<Output = Result<Value, StoreError>> + Send>>;
type Loader = Arc<dyn Fn(StoreLoadInput) -> LoadFuture + Send + Sync>;
type VerifyFuture = Pin<Box<dyn Future<Output = Option<StoreGrant>> + Send>>;
type Verifier = Arc<dyn Fn(StoreVerifyInput) -> VerifyFuture + Send + Sync>;

#[derive(Clone)]
pub struct StoreDefinition {
    params_schema: Value,
    auth: Option<StoreAuth>,
    verify: Option<Verifier>,
    load: Loader,
    offline: Option<StoreOfflineConfig>,
    offline_version: String,
    keepalive_interval_ms: u64,
    max_connection_lifetime_ms: u64,
}

impl StoreDefinition {
    /// Create a public Store. Add [`Self::auth`] to restrict reads and subscriptions.
    pub fn new<F, Fut>(load: F) -> Self
    where
        F: Fn(StoreLoadInput) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, StoreError>> + Send + 'static,
    {
        Self {
            params_schema: json!({"type":"object", "additionalProperties":false}),
            auth: None,
            verify: None,
            load: Arc::new(move |input| Box::pin(load(input))),
            offline: None,
            offline_version: "1".into(),
            keepalive_interval_ms: 25_000,
            max_connection_lifetime_ms: 7_200_000,
        }
    }

    pub fn params_schema(mut self, schema: Value) -> Self {
        self.params_schema = schema;
        self
    }

    /// Return None to deny the read; return a grant to authorize it.
    pub fn auth<F, Fut>(mut self, auth: StoreAuth, verify: F) -> Self
    where
        F: Fn(StoreVerifyInput) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<StoreGrant>> + Send + 'static,
    {
        self.auth = Some(auth);
        self.verify = Some(Arc::new(move |input| Box::pin(verify(input))));
        self
    }

    /// Permit opt-in offline cached reads for this many milliseconds.
    /// Clients must also supply a non-secret cache scope, such as a user ID.
    pub fn offline(mut self, max_age_ms: u64) -> Self {
        self.offline = Some(StoreOfflineConfig {
            max_age_ms,
            version: self.offline_version.clone(),
        });
        self
    }

    /// Change the cache format version after an incompatible result schema change.
    pub fn offline_version(mut self, version: impl Into<String>) -> Self {
        self.offline_version = version.into();
        if let Some(offline) = &mut self.offline {
            offline.version.clone_from(&self.offline_version);
        }
        self
    }

    pub fn keepalive_interval_ms(mut self, milliseconds: u64) -> Self {
        self.keepalive_interval_ms = milliseconds;
        self
    }

    pub fn max_connection_lifetime_ms(mut self, milliseconds: u64) -> Self {
        self.max_connection_lifetime_ms = milliseconds;
        self
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreAuthorization {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub keepalive_interval_ms: u64,
    pub max_connection_lifetime_ms: u64,
}

#[derive(Default)]
pub struct StoreRegistry {
    definitions: RwLock<BTreeMap<String, StoreDefinition>>,
}

impl StoreRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or replace a Store and return its server-side invalidation handle.
    pub fn register(
        &self,
        name: impl Into<String>,
        definition: StoreDefinition,
    ) -> Result<StoreHandle, StoreError> {
        let name = name.into();
        validate_definition(&name, &definition)?;
        self.definitions
            .write()
            .unwrap()
            .insert(name.clone(), definition);
        Ok(StoreHandle { name })
    }

    pub fn metadata(&self) -> Vec<Value> {
        self.definitions.read().unwrap().iter().map(|(name, definition)| json!({
            "channel": format!("{STORE_CHANNEL_PREFIX}{name}"),
            "paramsSchema": definition.params_schema,
            "auth": definition.auth.as_ref().map_or(Value::Bool(false), |auth| serde_json::to_value(auth).unwrap()),
            "keepaliveIntervalMs": definition.keepalive_interval_ms,
            "maxConnectionLifetimeMs": definition.max_connection_lifetime_ms,
        })).collect()
    }

    /// Authorize an internal channel request. Store callbacks receive public names.
    pub async fn authorize(&self, input: PresenceVerifyInput) -> Option<StoreAuthorization> {
        let name = input.channel.strip_prefix(STORE_CHANNEL_PREFIX)?;
        let definition = self.definitions.read().unwrap().get(name).cloned()?;
        let grant = if input.operation != "subscribe" || !input.params.is_object() {
            None
        } else if let Some(verify) = definition.verify {
            verify(StoreVerifyInput {
                store: name.to_owned(),
                params: input.params,
                header: input.header,
                cookie: input.cookie,
            })
            .await
        } else {
            Some(StoreGrant::default())
        };
        Some(StoreAuthorization {
            ok: grant.is_some(),
            subject: grant.and_then(|grant| grant.subject),
            keepalive_interval_ms: definition.keepalive_interval_ms,
            max_connection_lifetime_ms: definition.max_connection_lifetime_ms,
        })
    }

    /// Execute a read that has already been validated and authorized by the proxy.
    /// Only expose this through an authenticated internal endpoint.
    pub async fn load(&self, input: StoreLoadInput) -> Option<Result<StoreSnapshot, StoreError>> {
        let definition = self
            .definitions
            .read()
            .unwrap()
            .get(&input.store)
            .cloned()?;
        if !input.params.is_object() {
            return Some(Err(StoreError::InvalidParams));
        }
        Some((definition.load)(input).await.and_then(|data| {
            let snapshot = StoreSnapshot {
                data,
                offline: definition.offline,
            };
            let bytes = serde_json::to_vec(&snapshot).map_err(|_| StoreError::Protocol)?;
            if bytes.len() > MAX_STORE_BYTES {
                Err(StoreError::TooLarge)
            } else {
                Ok(snapshot)
            }
        }))
    }
}

fn validate_definition(name: &str, definition: &StoreDefinition) -> Result<(), StoreError> {
    if name.is_empty()
        || name.len() > 256
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(StoreError::InvalidDefinition(
            "name must match [A-Za-z0-9][A-Za-z0-9._-]{0,255}",
        ));
    }
    if !definition.params_schema.is_object() {
        return Err(StoreError::InvalidDefinition(
            "params schema must be an object",
        ));
    }
    if definition.keepalive_interval_ms == 0
        || definition.max_connection_lifetime_ms == 0
        || definition.keepalive_interval_ms > 9_007_199_254_740_991
        || definition.max_connection_lifetime_ms > 9_007_199_254_740_991
    {
        return Err(StoreError::InvalidDefinition(
            "connection intervals must be positive safe integers",
        ));
    }
    if let Some(offline) = &definition.offline
        && (offline.max_age_ms == 0
            || offline.max_age_ms > 9_007_199_254_740_991
            || offline.version.is_empty()
            || offline.version.len() > 128)
    {
        return Err(StoreError::InvalidDefinition(
            "offline cache requires a positive safe max age and 1–128 byte version",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct StoreHandle {
    name: String,
}

impl StoreHandle {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Publish after a committed write. A transport failure does not roll back
    /// that write; use an application outbox when delivery must be atomic.
    pub fn invalidate_blocking(&self, params: Value) -> Result<(), StoreError> {
        let (socket_path, app) = runtime_config()?;
        self.invalidate_at(&socket_path, &app, params, RPC_TIMEOUT)
    }

    #[cfg(feature = "tokio")]
    pub async fn invalidate(&self, params: Value) -> Result<(), StoreError> {
        let (socket_path, app) = runtime_config()?;
        self.invalidate_async_at(&socket_path, &app, params, RPC_TIMEOUT)
            .await
    }

    fn command(&self, app: &str, params: Value) -> Result<Vec<u8>, StoreError> {
        if !params.is_object() {
            return Err(StoreError::InvalidParams);
        }
        if serde_json::to_vec(&params)
            .map_err(|_| StoreError::Protocol)?
            .len()
            > MAX_SCOPE_BYTES
        {
            return Err(StoreError::ScopeTooLarge);
        }
        let mut bytes = serde_json::to_vec(&json!({
            "command":"channel_publish", "app":app,
            "channel":format!("{STORE_CHANNEL_PREFIX}{}", self.name),
            "payload":{"type":"tako.store.invalidate", "data":{}, "scopeParams":params},
        }))
        .map_err(|_| StoreError::Protocol)?;
        if bytes.len() >= MAX_STORE_BYTES {
            return Err(StoreError::TooLarge);
        }
        bytes.push(b'\n');
        Ok(bytes)
    }

    #[cfg(unix)]
    fn invalidate_at(
        &self,
        path: &str,
        app: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<(), StoreError> {
        let bytes = self.command(app, params)?;
        let deadline = Instant::now() + timeout;
        let mut stream = connect_with_deadline(path, deadline)?;
        let mut remaining = bytes.as_slice();
        while !remaining.is_empty() {
            stream.set_write_timeout(Some(time_remaining(deadline)?))?;
            let count = stream.write(remaining)?;
            if count == 0 {
                return Err(StoreError::Protocol);
            }
            remaining = &remaining[count..];
        }
        let mut response = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            stream.set_read_timeout(Some(time_remaining(deadline)?))?;
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                return Err(StoreError::Protocol);
            }
            let newline = buffer[..count].iter().position(|byte| *byte == b'\n');
            response.extend_from_slice(&buffer[..newline.unwrap_or(count)]);
            if response.len() > MAX_STORE_BYTES {
                return Err(StoreError::TooLarge);
            }
            if newline.is_some() {
                return parse_rpc_response(&response);
            }
        }
    }

    #[cfg(not(unix))]
    fn invalidate_at(
        &self,
        _path: &str,
        _app: &str,
        _params: Value,
        _timeout: Duration,
    ) -> Result<(), StoreError> {
        Err(StoreError::RuntimeUnavailable)
    }

    #[cfg(all(feature = "tokio", unix))]
    async fn invalidate_async_at(
        &self,
        path: &str,
        app: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<(), StoreError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let command = self.command(app, params)?;
        tokio::time::timeout(timeout, async {
            let mut stream = tokio::net::UnixStream::connect(path).await?;
            stream.write_all(&command).await?;
            let mut reader = BufReader::new(stream);
            let mut response = Vec::new();
            loop {
                let buffer = reader.fill_buf().await?;
                if buffer.is_empty() {
                    return Err(StoreError::Protocol);
                }
                let newline = buffer.iter().position(|byte| *byte == b'\n');
                let consumed = newline.map_or(buffer.len(), |index| index + 1);
                response.extend_from_slice(&buffer[..newline.unwrap_or(buffer.len())]);
                if response.len() > MAX_STORE_BYTES {
                    return Err(StoreError::TooLarge);
                }
                reader.consume(consumed);
                if newline.is_some() {
                    return parse_rpc_response(&response);
                }
            }
        })
        .await
        .map_err(|_| StoreError::Timeout)?
    }

    #[cfg(all(feature = "tokio", not(unix)))]
    async fn invalidate_async_at(
        &self,
        _path: &str,
        _app: &str,
        _params: Value,
        _timeout: Duration,
    ) -> Result<(), StoreError> {
        Err(StoreError::RuntimeUnavailable)
    }
}

fn runtime_config() -> Result<(String, String), StoreError> {
    let path = std::env::var("TAKO_INTERNAL_SOCKET")
        .ok()
        .filter(|value| !value.is_empty());
    let app = std::env::var("TAKO_APP_NAME")
        .ok()
        .filter(|value| !value.is_empty());
    path.zip(app).ok_or(StoreError::RuntimeUnavailable)
}

fn parse_rpc_response(bytes: &[u8]) -> Result<(), StoreError> {
    let response: Value = serde_json::from_slice(bytes).map_err(|_| StoreError::Protocol)?;
    match response.get("status").and_then(Value::as_str) {
        Some("ok") => Ok(()),
        Some("error") => Err(StoreError::Rejected),
        _ => Err(StoreError::Protocol),
    }
}

#[cfg(unix)]
fn time_remaining(deadline: Instant) -> Result<Duration, StoreError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(StoreError::Timeout)
}

/// Unix socket connection with a total deadline, including a full listen backlog.
#[cfg(unix)]
fn connect_with_deadline(
    path: &str,
    deadline: Instant,
) -> Result<std::os::unix::net::UnixStream, StoreError> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let bytes = std::ffi::OsStr::new(path).as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(StoreError::RuntimeUnavailable);
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        address.sun_len = length as u8;
    }
    let connected = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    };
    if connected != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error.into());
        }
        loop {
            let milliseconds = time_remaining(deadline)?
                .as_millis()
                .clamp(1, i32::MAX as u128) as i32;
            let mut descriptor = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if result == 0 {
                return Err(StoreError::Timeout);
            }
            let mut error = 0_i32;
            let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut error as *mut i32).cast(),
                    &mut length,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error).into());
            }
            break;
        }
    }
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(std::os::unix::net::UnixStream::from(fd))
}

#[cfg(test)]
mod tests;

//! Tako-hosted KV stores bound to the app in `tako.toml` (`[envs.<env>].kv`).
//!
//! A store holds JSON values, each with an optional TTL; keys without one
//! stay until deleted. Requests go through the app's internal socket with
//! the binding's token, in development and in production, and the store may
//! live on another server of the network.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::KvRejected;

/// The longest key, in bytes.
const MAX_KEY_BYTES: usize = 1024;
/// The largest value, as JSON text.
const MAX_VALUE_BYTES: usize = 256 * 1024;
/// The longest request or reply line.
const LIMIT: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

/// A Tako-hosted KV store bound to this app.
///
/// ```no_run
/// use std::time::Duration;
///
/// let sessions = tako::kv("sessions").expect("a KV store bound as sessions");
/// sessions.put_blocking("session:42", &"u1", Some(Duration::from_secs(86_400)))?;
/// let user: Option<String> = sessions.get_blocking("session:42")?;
/// sessions.delete_blocking("session:42")?;
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// With the `tokio` feature, [`Kv::get`], [`Kv::put`] and [`Kv::delete`] are
/// the async forms. A rejection's [`ErrorCode::of`](crate::ErrorCode::of)
/// gives its reason, such as
/// [`ErrorCode::Unavailable`](crate::ErrorCode::Unavailable).
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct Kv {
    token: String,
}

impl fmt::Debug for Kv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Kv").field("token", &"[REDACTED]").finish()
    }
}

/// Where requests go: the internal socket and the calling app.
#[derive(Clone, Debug)]
pub(crate) struct Endpoint {
    pub(crate) socket: PathBuf,
    pub(crate) app: String,
}

impl Endpoint {
    fn from_env() -> io::Result<Self> {
        let unavailable = || {
            io::Error::other(
                crate::compose_sdk_hint()
                    .unwrap_or_else(|| "KV stores require the Tako runtime".into()),
            )
        };
        let socket = std::env::var_os("TAKO_INTERNAL_SOCKET")
            .filter(|value| !value.is_empty())
            .ok_or_else(unavailable)?;
        let app = std::env::var("TAKO_APP_NAME")
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(unavailable)?;
        Ok(Self {
            socket: socket.into(),
            app,
        })
    }
}

impl Kv {
    #[cfg(test)]
    pub(crate) fn new(token: &str) -> Self {
        Self {
            token: token.into(),
        }
    }

    /// The key's value, or `None` when it is not set or has expired.
    pub fn get_blocking<T: DeserializeOwned>(&self, key: &str) -> io::Result<Option<T>> {
        self.get_blocking_at(&Endpoint::from_env()?, key)
    }

    /// Sets the key to `value`, encoded as JSON. With a `ttl` the key expires
    /// after it; without one it stays until deleted. Putting the key again
    /// replaces its TTL.
    pub fn put_blocking<T: Serialize + ?Sized>(
        &self,
        key: &str,
        value: &T,
        ttl: Option<Duration>,
    ) -> io::Result<()> {
        self.put_blocking_at(&Endpoint::from_env()?, key, value, ttl)
    }

    /// Removes the key. Removing a key that is not set succeeds.
    pub fn delete_blocking(&self, key: &str) -> io::Result<()> {
        self.delete_blocking_at(&Endpoint::from_env()?, key)
    }

    pub(crate) fn get_blocking_at<T: DeserializeOwned>(
        &self,
        endpoint: &Endpoint,
        key: &str,
    ) -> io::Result<Option<T>> {
        let line = self.encode(endpoint, key, json!({"operation": "get"}))?;
        value(key, send_blocking(endpoint, &line)?)
    }

    pub(crate) fn put_blocking_at<T: Serialize + ?Sized>(
        &self,
        endpoint: &Endpoint,
        key: &str,
        value: &T,
        ttl: Option<Duration>,
    ) -> io::Result<()> {
        let line = self.encode(endpoint, key, put(value, ttl)?)?;
        send_blocking(endpoint, &line).map(drop)
    }

    pub(crate) fn delete_blocking_at(&self, endpoint: &Endpoint, key: &str) -> io::Result<()> {
        let line = self.encode(endpoint, key, json!({"operation": "delete"}))?;
        send_blocking(endpoint, &line).map(drop)
    }

    /// Async form of [`Kv::get_blocking`].
    #[cfg(all(unix, feature = "tokio"))]
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> io::Result<Option<T>> {
        let endpoint = Endpoint::from_env()?;
        let line = self.encode(&endpoint, key, json!({"operation": "get"}))?;
        value(key, send(&endpoint, &line).await?)
    }

    /// Async form of [`Kv::put_blocking`].
    #[cfg(all(unix, feature = "tokio"))]
    pub async fn put<T: Serialize + ?Sized>(
        &self,
        key: &str,
        value: &T,
        ttl: Option<Duration>,
    ) -> io::Result<()> {
        let endpoint = Endpoint::from_env()?;
        let line = self.encode(&endpoint, key, put(value, ttl)?)?;
        send(&endpoint, &line).await.map(drop)
    }

    /// Async form of [`Kv::delete_blocking`].
    #[cfg(all(unix, feature = "tokio"))]
    pub async fn delete(&self, key: &str) -> io::Result<()> {
        let endpoint = Endpoint::from_env()?;
        let line = self.encode(&endpoint, key, json!({"operation": "delete"}))?;
        send(&endpoint, &line).await.map(drop)
    }

    fn encode(&self, endpoint: &Endpoint, key: &str, mut request: Value) -> io::Result<Vec<u8>> {
        if key.is_empty() {
            return Err(invalid("KV key cannot be empty"));
        }
        if key.len() > MAX_KEY_BYTES {
            return Err(invalid("KV key exceeds 1 KiB"));
        }
        let fields = request.as_object_mut().expect("requests are objects");
        fields.insert("command".into(), "kv".into());
        fields.insert("app".into(), endpoint.app.clone().into());
        fields.insert("token".into(), self.token.clone().into());
        fields.insert("key".into(), key.into());
        let mut line = serde_json::to_vec(&request)?;
        line.push(b'\n');
        Ok(line)
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn put<T: Serialize + ?Sized>(value: &T, ttl: Option<Duration>) -> io::Result<Value> {
    let json = serde_json::to_string(value)?;
    if json.len() > MAX_VALUE_BYTES {
        return Err(invalid("KV value exceeds 256 KiB"));
    }
    let mut request = json!({"operation": "put", "json": json});
    if let Some(ttl) = ttl {
        if ttl.is_zero() {
            return Err(invalid("KV ttl must be longer than zero"));
        }
        // Round up so a sub-millisecond TTL still expires rather than failing.
        let ms = u64::try_from(ttl.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX);
        request["ttl_ms"] = ms.into();
    }
    Ok(request)
}

/// A get reply's value: the JSON text, or null for a missing key.
fn value<T: DeserializeOwned>(key: &str, data: Value) -> io::Result<Option<T>> {
    match data {
        Value::Null => Ok(None),
        Value::String(json) => serde_json::from_str(&json).map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("decode KV value {key:?}: {error}"),
            )
        }),
        _ => Err(io::Error::other("invalid KV response")),
    }
}

fn decode(bytes: &[u8]) -> io::Result<Value> {
    if bytes.len() > LIMIT || bytes.last() != Some(&b'\n') {
        return Err(io::Error::other("invalid KV response frame"));
    }
    let response: Value = serde_json::from_slice(bytes)?;
    if response.get("status").and_then(Value::as_str) != Some("ok") {
        let message = response
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        return Err(io::Error::other(KvRejected {
            code: crate::error::reply_code(&response),
            message,
        }));
    }
    Ok(response.get("data").cloned().unwrap_or(Value::Null))
}

#[cfg(unix)]
fn send_blocking(endpoint: &Endpoint, line: &[u8]) -> io::Result<Value> {
    use std::io::{BufRead, Read, Write};
    let deadline = std::time::Instant::now() + TIMEOUT;
    let path = endpoint
        .socket
        .to_str()
        .ok_or_else(|| io::Error::other("invalid socket path"))?;
    let mut socket =
        crate::stores::connect_with_deadline(path, deadline).map_err(io::Error::other)?;
    socket.set_read_timeout(Some(TIMEOUT))?;
    socket.set_write_timeout(Some(TIMEOUT))?;
    socket.write_all(line)?;
    let mut response = Vec::new();
    io::BufReader::new(socket.take((LIMIT + 1) as u64)).read_until(b'\n', &mut response)?;
    decode(&response)
}

#[cfg(not(unix))]
fn send_blocking(_endpoint: &Endpoint, _line: &[u8]) -> io::Result<Value> {
    Err(io::Error::other("Tako KV stores require Unix sockets"))
}

#[cfg(all(unix, feature = "tokio"))]
async fn send(endpoint: &Endpoint, line: &[u8]) -> io::Result<Value> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    tokio::time::timeout(TIMEOUT, async {
        let mut socket = tokio::net::UnixStream::connect(&endpoint.socket).await?;
        socket.write_all(line).await?;
        let mut response = Vec::new();
        tokio::io::BufReader::new(socket.take((LIMIT + 1) as u64))
            .read_until(b'\n', &mut response)
            .await?;
        decode(&response)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "KV request timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> Endpoint {
        Endpoint {
            socket: "/nonexistent/tako.sock".into(),
            app: "demo".into(),
        }
    }

    #[test]
    fn invalid_requests_fail_before_connecting() {
        let kv = Kv::new("t");
        let endpoint = endpoint();
        let errors = [
            kv.get_blocking_at::<Value>(&endpoint, "").unwrap_err(),
            kv.delete_blocking_at(&endpoint, &"k".repeat(1025))
                .unwrap_err(),
            kv.put_blocking_at(&endpoint, "k", &"x".repeat(MAX_VALUE_BYTES), None)
                .unwrap_err(),
            kv.put_blocking_at(&endpoint, "k", &1, Some(Duration::ZERO))
                .unwrap_err(),
        ];
        for error in errors {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        }
    }

    #[test]
    fn ttls_round_up_to_whole_milliseconds() {
        let request = put(&1, Some(Duration::from_micros(1_500))).unwrap();
        assert_eq!(request["ttl_ms"], 2);
        assert!(put(&1, None).unwrap().get("ttl_ms").is_none());
    }

    #[test]
    fn the_token_is_not_shown() {
        assert_eq!(
            format!("{:?}", Kv::new("secret")),
            r#"Kv { token: "[REDACTED]" }"#
        );
    }
}

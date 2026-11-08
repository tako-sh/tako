//! Private app-to-app channel publication and workflow enqueue.
//! Addresses are `app.primitive`; bare names select the current app.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io, path::PathBuf, time::Duration};

const LIMIT: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default, Clone, Serialize)]
pub struct EnqueueOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_at_ms: Option<i64>,
    /// Total attempts including the first. Omit to use the recipient's policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnqueueResult {
    pub id: String,
    pub deduplicated: bool,
}

#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    app: String,
}

impl Client {
    pub fn from_env() -> io::Result<Self> {
        let socket = std::env::var_os("TAKO_INTERNAL_SOCKET")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| io::Error::other("network primitives require the Tako runtime"))?;
        let app = std::env::var("TAKO_APP_NAME")
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| io::Error::other("network primitives require the Tako runtime"))?;
        Ok(Self {
            socket: socket.into(),
            app,
        })
    }

    fn encode(&self, address: &str, request: Value) -> io::Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec(
            &json!({"command":"network_call", "app":self.app, "address":address, "request":request}),
        )?;
        if bytes.len() >= LIMIT {
            return Err(io::Error::other("network request too large"));
        }
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn enqueue_blocking(
        &self,
        address: &str,
        payload: impl Serialize,
        options: EnqueueOptions,
    ) -> io::Result<EnqueueResult> {
        let bytes = self.encode(address, json!({"operation":"enqueue", "payload":serde_json::to_value(payload)?, "opts":options}))?;
        serde_json::from_value(self.call_blocking(&bytes)?).map_err(io::Error::other)
    }

    pub fn publish_blocking(
        &self,
        address: &str,
        params: impl Serialize,
        message_type: &str,
        data: impl Serialize,
    ) -> io::Result<Value> {
        let bytes = self.encode(address, json!({"operation":"publish", "params":serde_json::to_value(params)?, "message":{"type":message_type,"data":serde_json::to_value(data)?}}))?;
        self.call_blocking(&bytes)
    }

    #[cfg(unix)]
    fn call_blocking(&self, bytes: &[u8]) -> io::Result<Value> {
        use std::io::{BufRead, Read, Write};
        let deadline = std::time::Instant::now() + TIMEOUT;
        let path = self
            .socket
            .to_str()
            .ok_or_else(|| io::Error::other("invalid socket path"))?;
        let mut socket =
            crate::stores::connect_with_deadline(path, deadline).map_err(io::Error::other)?;
        socket.set_read_timeout(Some(TIMEOUT))?;
        socket.set_write_timeout(Some(TIMEOUT))?;
        socket.write_all(bytes)?;
        let mut response = Vec::new();
        io::BufReader::new(socket.take((LIMIT + 1) as u64)).read_until(b'\n', &mut response)?;
        decode(&response)
    }

    #[cfg(not(unix))]
    fn call_blocking(&self, _bytes: &[u8]) -> io::Result<Value> {
        Err(io::Error::other(
            "Tako network primitives require Unix sockets",
        ))
    }

    #[cfg(all(unix, feature = "tokio"))]
    pub async fn enqueue(
        &self,
        address: &str,
        payload: impl Serialize,
        options: EnqueueOptions,
    ) -> io::Result<EnqueueResult> {
        let bytes = self.encode(address, json!({"operation":"enqueue", "payload":serde_json::to_value(payload)?, "opts":options}))?;
        serde_json::from_value(self.call(&bytes).await?).map_err(io::Error::other)
    }

    #[cfg(all(unix, feature = "tokio"))]
    pub async fn publish(
        &self,
        address: &str,
        params: impl Serialize,
        message_type: &str,
        data: impl Serialize,
    ) -> io::Result<Value> {
        let bytes = self.encode(address, json!({"operation":"publish", "params":serde_json::to_value(params)?, "message":{"type":message_type,"data":serde_json::to_value(data)?}}))?;
        self.call(&bytes).await
    }

    #[cfg(all(unix, feature = "tokio"))]
    async fn call(&self, bytes: &[u8]) -> io::Result<Value> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(TIMEOUT, async {
            let mut socket = tokio::net::UnixStream::connect(&self.socket).await?;
            socket.write_all(bytes).await?;
            let mut response = Vec::new();
            tokio::io::BufReader::new(socket.take((LIMIT + 1) as u64))
                .read_until(b'\n', &mut response)
                .await?;
            decode(&response)
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "network primitive call timed out"))?
    }
}

fn decode(bytes: &[u8]) -> io::Result<Value> {
    if bytes.len() > LIMIT || bytes.last() != Some(&b'\n') {
        return Err(io::Error::other("invalid network response frame"));
    }
    let response: Value = serde_json::from_slice(bytes)?;
    if response.get("status").and_then(Value::as_str) != Some("ok") {
        return Err(io::Error::other("network primitive call failed"));
    }
    Ok(response.get("data").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_preserves_caller_and_destination_separately() {
        let client = Client {
            socket: "/tmp/internal.sock".into(),
            app: "rust-app/staging".into(),
        };
        let value: Value = serde_json::from_slice(
            &client
                .encode(
                    "mail.send-email",
                    json!({"operation":"enqueue","payload":{"to":"a@example.com"},"opts":{}}),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value["app"], "rust-app/staging");
        assert_eq!(value["address"], "mail.send-email");
        assert_eq!(value["request"]["opts"], json!({}));
        assert!(decode(b"{\"status\":\"ok\",\"data\":{}}\n").is_ok());
        assert!(decode(b"{\"status\":\"ok\"}").is_err());
        assert!(
            decode(b"{\"status\":\"error\",\"message\":\"private backend detail\"}\n")
                .unwrap_err()
                .to_string()
                .find("private")
                .is_none()
        );
    }
}

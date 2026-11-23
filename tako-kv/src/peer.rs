//! Forwarding between servers: an app's server sends a request for a store
//! on another server to that server's peer port over the private network,
//! one JSON line each way.
//!
//! A request names the app environment, the grant key the store's server
//! issued to it, the store and the operation. The listener accepts only
//! addresses of the network's servers, and the store's server checks the
//! grant, so a process on another server cannot use a store its app was
//! not granted. Connections are kept for reuse and closed after a minute
//! idle.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tako_core::{KvRequest, Response};
use tokio::io::{AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::host::{KvError, KvHost, NOT_SERVED};

/// The largest request or reply line: a value, escaped, and its envelope.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Open connections the listener serves at once.
const MAX_CONNECTIONS: usize = 4096;
/// A connection with no request for this long is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Idle connections kept per server for reuse.
const POOLED_PER_UPSTREAM: usize = 16;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PeerRequest {
    /// The app environment the request is for.
    pub scope: String,
    /// The key the store's server issued with its grant to `scope`. The
    /// request's own `key` is the KV key.
    pub grant: String,
    pub store: String,
    #[serde(flatten)]
    pub request: KvRequest,
}

/// Accepts forwarded requests from the network's other servers. `allow`
/// decides which source addresses may connect.
pub async fn serve_peers(
    host: KvHost,
    listener: TcpListener,
    allow: Arc<dyn Fn(IpAddr) -> bool + Send + Sync>,
) {
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, address) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(%error, "KV peer accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if !allow(address.ip()) {
            continue;
        }
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            continue;
        };
        let host = host.clone();
        tokio::spawn(async move {
            let _permit = permit;
            serve_connection(host, stream).await;
        });
    }
}

async fn serve_connection(host: KvHost, stream: tokio::net::TcpStream) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = AsyncBufReader::new(reader);
    loop {
        let line = tokio::time::timeout(
            IDLE_TIMEOUT,
            tako_socket::read_json_line_with_limit::<_, serde_json::Value>(
                &mut reader,
                MAX_LINE_BYTES,
            ),
        )
        .await;
        let value = match line {
            Ok(Ok(Some(value))) => value,
            // Idle, closed, too long, or not JSON.
            _ => return,
        };
        let response = match serde_json::from_value::<PeerRequest>(value) {
            Ok(request) => {
                let host = host.clone();
                tokio::task::spawn_blocking(move || host.serve_peer(request))
                    .await
                    .unwrap_or_else(|_| Err(KvError::unavailable(NOT_SERVED)))
                    .map_or_else(KvError::into_response, Response::ok)
            }
            Err(error) => {
                KvError::invalid(format!("malformed KV request: {error}")).into_response()
            }
        };
        let mut line = serde_json::to_vec(&response).expect("responses serialize");
        line.push(b'\n');
        if writer.write_all(&line).await.is_err() {
            return;
        }
    }
}

type Connection = BufReader<TcpStream>;

/// Connections to other servers' peer ports, kept for reuse.
pub(crate) struct PeerPool {
    idle: Mutex<HashMap<SocketAddr, Vec<Connection>>>,
    connect_timeout: Duration,
    request_timeout: Duration,
}

fn unreachable(upstream: SocketAddr, error: impl std::fmt::Display) -> KvError {
    KvError::unavailable(format!(
        "The KV store's server ({upstream}) is not reachable over the private network: {error}"
    ))
}

impl PeerPool {
    pub(crate) fn new(connect_timeout: Duration, request_timeout: Duration) -> Self {
        Self {
            idle: Mutex::default(),
            connect_timeout,
            request_timeout,
        }
    }

    fn connect(&self, upstream: SocketAddr) -> std::io::Result<Connection> {
        let stream = TcpStream::connect_timeout(&upstream, self.connect_timeout)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(self.request_timeout))?;
        stream.set_write_timeout(Some(self.request_timeout))?;
        Ok(BufReader::new(stream))
    }

    /// Sends `request` to the store's server and returns its reply. Blocks.
    pub(crate) fn request(
        &self,
        upstream: SocketAddr,
        request: &PeerRequest,
    ) -> Result<serde_json::Value, KvError> {
        let mut line = serde_json::to_vec(request).expect("requests serialize");
        line.push(b'\n');
        let pooled = self.idle.lock().get_mut(&upstream).and_then(Vec::pop);
        // A kept connection may have been closed by the other side while
        // idle; the request is sent again on a new one. Every operation can
        // be repeated safely.
        let reply = match pooled
            .map(|mut connection| exchange(&mut connection, &line).map(|r| (r, connection)))
        {
            Some(Ok(done)) => done,
            _ => {
                let mut connection = self
                    .connect(upstream)
                    .map_err(|error| unreachable(upstream, error))?;
                let reply = exchange(&mut connection, &line)
                    .map_err(|error| unreachable(upstream, error))?;
                (reply, connection)
            }
        };
        let (response, connection) = reply;
        {
            let mut idle = self.idle.lock();
            let kept = idle.entry(upstream).or_default();
            if kept.len() < POOLED_PER_UPSTREAM {
                kept.push(connection);
            }
        }
        match response {
            Response::Ok { data } => Ok(data),
            Response::Error { message, code } => Err(KvError::new(code, message)),
        }
    }
}

fn exchange(connection: &mut Connection, line: &[u8]) -> std::io::Result<Response> {
    connection.get_mut().write_all(line)?;
    let mut reply = Vec::new();
    let read = connection
        .by_ref()
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut reply)?;
    if read == 0 || reply.last() != Some(&b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the connection closed",
        ));
    }
    serde_json::from_slice(&reply)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

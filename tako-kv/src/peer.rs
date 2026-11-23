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
use std::time::{Duration, Instant};

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
/// How long an idle connection is kept, well within the other side's
/// [`IDLE_TIMEOUT`], so a reused one is rarely closed under a request.
const POOLED_FOR: Duration = Duration::from_secs(30);

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
    /// Idle connections, with when each was last used.
    idle: Mutex<HashMap<SocketAddr, Vec<(Instant, Connection)>>>,
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
        let pooled = {
            let mut idle = self.idle.lock();
            let kept = idle.entry(upstream).or_default();
            kept.retain(|(used, _)| used.elapsed() < POOLED_FOR);
            // One the other side closed meanwhile, as a restarting server
            // does, is dropped before anything is sent on it.
            std::iter::from_fn(|| kept.pop())
                .map(|(_, connection)| connection)
                .find(open)
        };
        let reply = match pooled
            .map(|mut connection| exchange(&mut connection, &line).map(|reply| (reply, connection)))
        {
            Some(Ok(done)) => done,
            // A kept connection the other side closed before the request was
            // written: send it again on a new one. Once it was written, any
            // failure may come after the request was applied, even the
            // connection closing with no reply, so it is not repeated: a
            // repeated put would move its expiry, or undo a later write.
            Some(Err(Failure::NotDelivered(_))) | None => {
                let mut connection = self
                    .connect(upstream)
                    .map_err(|error| unreachable(upstream, error))?;
                let reply = exchange(&mut connection, &line)
                    .map_err(|error| unreachable(upstream, error.into_inner()))?;
                (reply, connection)
            }
            Some(Err(Failure::Unknown(error))) => return Err(unreachable(upstream, error)),
        };
        let (response, connection) = reply;
        {
            let mut idle = self.idle.lock();
            let kept = idle.entry(upstream).or_default();
            if kept.len() < POOLED_PER_UPSTREAM {
                kept.push((Instant::now(), connection));
            }
        }
        match response {
            Response::Ok { data } => Ok(data),
            Response::Error { message, code } => Err(KvError::new(code, message)),
        }
    }
}

/// Why an exchange failed.
enum Failure {
    /// Writing the request failed because the other side had closed the
    /// connection, so it never received the whole line.
    NotDelivered(std::io::Error),
    /// The request may have been applied: it was written, and then the
    /// reply timed out, broke off, or never came.
    Unknown(std::io::Error),
}

/// Whether a kept connection is still open: nothing is waiting to be read,
/// and the other side has not closed it.
fn open(connection: &Connection) -> bool {
    let stream = connection.get_ref();
    if !connection.buffer().is_empty() || stream.set_nonblocking(true).is_err() {
        return false;
    }
    let mut byte = [0u8; 1];
    let idle = matches!(
        stream.peek(&mut byte),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    );
    stream.set_nonblocking(false).is_ok() && idle
}

impl Failure {
    fn into_inner(self) -> std::io::Error {
        match self {
            Self::NotDelivered(error) | Self::Unknown(error) => error,
        }
    }
}

fn closed(kind: std::io::ErrorKind) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, NotConnected};
    matches!(
        kind,
        BrokenPipe | ConnectionReset | ConnectionAborted | NotConnected
    )
}

fn exchange(connection: &mut Connection, line: &[u8]) -> Result<Response, Failure> {
    if let Err(error) = connection.get_mut().write_all(line) {
        return Err(if closed(error.kind()) {
            Failure::NotDelivered(error)
        } else {
            Failure::Unknown(error)
        });
    }
    let mut reply = Vec::new();
    let read = connection
        .by_ref()
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut reply);
    match read {
        Ok(0) => {
            return Err(Failure::Unknown(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the connection closed before replying",
            )));
        }
        Err(error) => return Err(Failure::Unknown(error)),
        Ok(_) => {}
    }
    if reply.last() != Some(&b'\n') {
        return Err(Failure::Unknown(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the connection closed before the reply ended",
        )));
    }
    serde_json::from_slice(&reply).map_err(|error| {
        Failure::Unknown(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn request() -> PeerRequest {
        PeerRequest {
            scope: "shop/production".into(),
            grant: "key".into(),
            store: "sessions".into(),
            request: KvRequest::Put {
                key: "k".into(),
                json: "1".into(),
                ttl_ms: Some(60_000),
            },
        }
    }

    /// A peer that answers the first request on each connection and never
    /// answers the ones after it.
    fn stalling_peer() -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let counted = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let counted = counted.clone();
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().unwrap();
                    let mut lines = BufReader::new(stream).lines();
                    let mut answered = false;
                    while let Some(Ok(_)) = lines.next() {
                        counted.fetch_add(1, Ordering::SeqCst);
                        if !answered {
                            answered = true;
                            writer
                                .write_all(b"{\"status\":\"ok\",\"data\":null}\n")
                                .unwrap();
                        }
                    }
                });
            }
        });
        (address, seen)
    }

    #[test]
    fn requests_that_may_have_been_applied_are_not_sent_again() {
        let (upstream, seen) = stalling_peer();
        let pool = PeerPool::new(Duration::from_secs(5), Duration::from_millis(300));
        pool.request(upstream, &request()).unwrap();
        // The second goes out on the kept connection and gets no reply: it
        // may have been applied, so it is not repeated on a new one.
        let error = pool.request(upstream, &request()).unwrap_err();
        assert!(error.message.contains("not reachable"), "{}", error.message);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn requests_the_peer_read_before_closing_are_not_sent_again() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let counted = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut writer = stream.try_clone().unwrap();
                let mut lines = BufReader::new(stream).lines();
                // Answer the first request, then read the next one and close
                // without replying, as a server stopping mid-request would.
                if lines.next().is_some() {
                    counted.fetch_add(1, Ordering::SeqCst);
                    writer
                        .write_all(b"{\"status\":\"ok\",\"data\":null}\n")
                        .unwrap();
                }
                if lines.next().is_some() {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        let pool = PeerPool::new(Duration::from_secs(5), Duration::from_secs(5));
        pool.request(upstream, &request()).unwrap();
        assert!(pool.request(upstream, &request()).is_err());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn requests_on_a_connection_the_peer_closed_are_sent_again() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let counted = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut writer = stream.try_clone().unwrap();
                let mut line = String::new();
                if BufReader::new(stream).read_line(&mut line).unwrap_or(0) > 0 {
                    counted.fetch_add(1, Ordering::SeqCst);
                    writer
                        .write_all(b"{\"status\":\"ok\",\"data\":null}\n")
                        .unwrap();
                }
                // Each connection closes after one reply, as after an idle
                // timeout.
            }
        });
        let pool = PeerPool::new(Duration::from_secs(5), Duration::from_secs(5));
        pool.request(upstream, &request()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        pool.request(upstream, &request()).unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }
}

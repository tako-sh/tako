//! HTTP endpoints of the SQL over HTTP protocol.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::Frame;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use serde::Serialize;
use tokio::net::TcpListener;

use crate::exec::{self, Limits};
use crate::host::{Database, DatabaseHost, DatabaseId, Principal, Resolved, Stream, StreamEntry};
use crate::proto::{CursorRequest, CursorResponse, PipelineRequest, Version};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = http_body_util::combinators::BoxBody<Chunk, BoxError>;

/// Response data. A chunk can keep part of the host's buffer budget
/// reserved: the connection drops it only once it has written it, so the
/// reservation lasts until the bytes have left this process.
pub struct Chunk {
    bytes: Bytes,
    _held: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl Chunk {
    fn held(bytes: Bytes, held: Arc<dyn std::any::Any + Send + Sync>) -> Self {
        Self {
            bytes,
            _held: Some(held),
        }
    }
}

impl From<Bytes> for Chunk {
    fn from(bytes: Bytes) -> Self {
        Self { bytes, _held: None }
    }
}

impl bytes::Buf for Chunk {
    fn remaining(&self) -> usize {
        self.bytes.remaining()
    }

    fn chunk(&self) -> &[u8] {
        self.bytes.chunk()
    }

    fn advance(&mut self, count: usize) {
        self.bytes.advance(count);
    }

    // Without a copy: the default would copy into a new buffer.
    fn copy_to_bytes(&mut self, len: usize) -> Bytes {
        self.bytes.copy_to_bytes(len)
    }
}

/// Header naming the app environment a forwarding server acts for.
pub const SCOPE_HEADER: &str = "x-tako-database-scope";
/// Header naming the database a forwarding server addresses.
pub const DATABASE_HEADER: &str = "x-tako-database";
/// Header carrying the key the database's server issued with its grant.
pub const KEY_HEADER: &str = "x-tako-database-key";

/// Who a listener accepts.
#[derive(Clone)]
pub enum Access {
    /// Apps on this server. Requests carry their binding token.
    Apps,
    /// Other servers forwarding for their apps. `allow` decides which peer
    /// addresses are servers of this network.
    Peers {
        allow: Arc<dyn Fn(IpAddr) -> bool + Send + Sync>,
    },
}

/// Serves `listener` until the task is dropped.
pub async fn serve(host: DatabaseHost, listener: TcpListener, access: Access) {
    let config = host.config();
    // Connections that never send a request reach none of the request
    // limits, so their number and idle time are bounded here.
    let connections = Arc::new(tokio::sync::Semaphore::new(config.max_connections));
    let idle_timeout = config.idle_connection_timeout;
    let stall_timeout = config.write_stall_timeout;
    loop {
        let Ok(slot) = connections.clone().acquire_owned().await else {
            return;
        };
        let (socket, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(%error, "database listener accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = socket.set_nodelay(true);
        let host_closings = host.inner.closings.clone();
        let host = host.clone();
        let access = access.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let service = hyper::service::service_fn(move |request| {
                let host = host.clone();
                let access = access.clone();
                async move { Ok::<_, Infallible>(handle(&host, &access, peer, request).await) }
            });
            // When this host stops serving its databases (a reload hands
            // them over), the connection closes after its current request,
            // so the client reconnects to the process that serves them now.
            let mut closings = host_closings.subscribe();
            let opened = *closings.borrow_and_update();
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .keep_alive(true)
                .timer(TokioTimer::new())
                .header_read_timeout(idle_timeout);
            let connection = builder.serve_connection(
                TokioIo::new(WriteStall::new(socket, stall_timeout)),
                service,
            );
            tokio::pin!(connection);
            let result = tokio::select! {
                result = connection.as_mut() => result,
                closed = async { closings.wait_for(|count| *count != opened).await.is_ok() } => {
                    if closed {
                        connection.as_mut().graceful_shutdown();
                    }
                    connection.await
                }
            };
            if let Err(error) = result {
                tracing::debug!(%error, "database connection ended");
            }
        });
    }
}

/// Fails writes that make no progress for `timeout`, which closes the
/// connection. Progress of any size starts the wait again.
struct WriteStall<S> {
    inner: S,
    timeout: Duration,
    stalled: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl<S> WriteStall<S> {
    fn new(inner: S, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            stalled: None,
        }
    }

    fn settle<T>(
        &mut self,
        cx: &mut std::task::Context<'_>,
        poll: std::task::Poll<std::io::Result<T>>,
    ) -> std::task::Poll<std::io::Result<T>> {
        use std::future::Future;
        use std::task::Poll;
        if poll.is_ready() {
            self.stalled = None;
            return poll;
        }
        let timeout = self.timeout;
        let stalled = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
        match stalled.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the client stopped reading the response",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for WriteStall<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for WriteStall<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let poll = std::pin::Pin::new(&mut this.inner).poll_write(cx, buf);
        this.settle(cx, poll)
    }

    fn poll_write_vectored(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let poll = std::pin::Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        this.settle(cx, poll)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let poll = std::pin::Pin::new(&mut this.inner).poll_flush(cx);
        this.settle(cx, poll)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Handles one request.
pub async fn handle<B>(
    host: &DatabaseHost,
    access: &Access,
    peer: SocketAddr,
    request: Request<B>,
) -> Response<Body>
where
    B: http_body::Body<Data = Bytes> + Send + Sync + 'static,
    B::Error: Into<BoxError>,
{
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    let endpoint = match (&method, path.as_str()) {
        (&Method::GET, "/" | "/health" | "/v2" | "/v3") => {
            return text(StatusCode::OK, "");
        }
        (&Method::POST, "/v2/pipeline") => Endpoint::Pipeline(Version::V2),
        (&Method::POST, "/v3/pipeline") => Endpoint::Pipeline(Version::V3),
        (&Method::POST, "/v3/cursor") => Endpoint::Cursor,
        _ => return error(StatusCode::NOT_FOUND, "not found"),
    };

    let resolved = match access {
        Access::Apps => {
            let Some(token) = bearer(&request) else {
                return error(StatusCode::UNAUTHORIZED, "missing bearer token");
            };
            match host.resolve_token(token) {
                Some(resolved) => resolved,
                None => return error(StatusCode::UNAUTHORIZED, "invalid database token"),
            }
        }
        Access::Peers { allow } => {
            if !allow(peer.ip()) {
                return error(
                    StatusCode::FORBIDDEN,
                    "peer is not a server of this network",
                );
            }
            let Some((scope, database, key)) = forwarded(&request) else {
                return error(
                    StatusCode::BAD_REQUEST,
                    "missing database forwarding headers",
                );
            };
            match host.resolve_remote(peer.ip(), &scope, &database, &key) {
                Some(resolved) => resolved,
                None => {
                    return error(
                        StatusCode::FORBIDDEN,
                        "this app environment has no grant for the database",
                    );
                }
            }
        }
    };

    match resolved {
        Resolved::Forward {
            database,
            upstream,
            scope,
            key,
        } => {
            forward(
                host,
                Upstream {
                    address: upstream,
                    scope: &scope,
                    database: &database,
                    key: &key,
                },
                &path,
                request.into_body(),
            )
            .await
        }
        Resolved::Unavailable(reason) => error(StatusCode::SERVICE_UNAVAILABLE, &reason),
        Resolved::Serve {
            database,
            principal,
        } => {
            // The request counts against the database's concurrency limit,
            // and its body against the host's buffer budget, before the body
            // is read.
            let limit = host.config().max_request_bytes;
            let declared = request
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok());
            if declared.is_some_and(|length| length > limit) {
                return error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    &format!("request body exceeds {limit} bytes"),
                );
            }
            let reserve = declared.unwrap_or(limit).clamp(1, limit.max(1));
            let admitted = match admit(host, &database, reserve).await {
                Ok(admitted) => admitted,
                Err(response) => return response,
            };
            // A client that stalls while sending its body must not hold
            // the slot it was admitted to.
            let config = host.config();
            let body = match tokio::time::timeout(
                config.request_body_timeout,
                read_body(request.into_body(), reserve),
            )
            .await
            {
                Ok(Ok(body)) => body,
                Ok(Err(response)) => return response,
                Err(_) => {
                    drop(admitted);
                    return error(
                        StatusCode::REQUEST_TIMEOUT,
                        "the request body did not arrive in time",
                    );
                }
            };
            match endpoint {
                Endpoint::Pipeline(version) => {
                    pipeline(host, database, principal, version, body, admitted).await
                }
                Endpoint::Cursor => cursor(host, database, principal, body, admitted).await,
            }
        }
    }
}

enum Endpoint {
    Pipeline(Version),
    Cursor,
}

/// A request admitted to a database: it is active (so pausing waits for it),
/// holds one of the database's request slots, and holds its body's share of
/// the host's buffer budget.
struct Admitted {
    active: crate::host::ActiveGuard,
    permit: tokio::sync::OwnedSemaphorePermit,
    buffered: tokio::sync::OwnedSemaphorePermit,
}

/// Waits, for at most the admission timeout, for one of the database's
/// request slots and for `body_bytes` of the host's buffer budget, and then
/// marks the request active. A request still waiting is not active, so it
/// neither holds up a pause nor waits without a deadline.
#[allow(clippy::result_large_err)]
async fn admit(
    host: &DatabaseHost,
    database: &Arc<Database>,
    body_bytes: usize,
) -> Result<Admitted, Response<Body>> {
    let busy = || {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the database is busy with other requests; retry",
        )
    };
    let deadline = tokio::time::Instant::now() + host.config().admission_timeout;
    let permit =
        match tokio::time::timeout_at(deadline, database.permits.clone().acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                return Err(error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "database is closing",
                ));
            }
            Err(_) => return Err(busy()),
        };
    let need = u32::try_from(body_bytes.min(host.config().max_buffered_bytes)).unwrap_or(u32::MAX);
    let buffered = match tokio::time::timeout_at(
        deadline,
        host.inner.buffered.clone().acquire_many_owned(need),
    )
    .await
    {
        Ok(Ok(buffered)) => buffered,
        Ok(Err(_)) => return Err(error(StatusCode::SERVICE_UNAVAILABLE, "server is closing")),
        Err(_) => return Err(busy()),
    };
    let active = DatabaseHost::enter(database)
        .map_err(|unavailable| error(StatusCode::SERVICE_UNAVAILABLE, &unavailable.0))?;
    Ok(Admitted {
        active,
        permit,
        buffered,
    })
}

async fn pipeline(
    host: &DatabaseHost,
    database: Arc<Database>,
    principal: Principal,
    version: Version,
    body: Bytes,
    admitted: Admitted,
) -> Response<Body> {
    let Admitted {
        active,
        permit,
        buffered,
    } = admitted;
    let request: PipelineRequest = match crate::proto::parse_request(&body) {
        Ok(request) => request,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("invalid pipeline: {e}")),
    };
    // The parsed request owns its text now; the admitted buffer bytes count
    // it in place of the body, so the body must not stay alive beside it.
    drop(body);
    let config = host.config();
    if let Err(exec::ProtocolError(message)) =
        exec::validate_pipeline(&request.requests, version, config.max_condition_depth)
    {
        return error(StatusCode::BAD_REQUEST, &message);
    }
    let (key, stream, existing) = match take_or_open(host, &database, &principal, request.baton) {
        Ok(taken) => taken,
        Err(response) => return response,
    };
    let limits = Limits {
        max_response_bytes: config.max_response_bytes,
        max_stored_sql: config.max_stored_sql_per_stream,
        max_stored_sql_bytes: config.max_stored_sql_bytes_per_stream,
    };
    let task_host = host.clone();
    let host_bytes = host.inner.buffered.clone();
    let task = tokio::task::spawn_blocking(move || {
        let _active = active;
        let _permit = permit;
        // The parsed requests live as long as the body did.
        let _buffered = buffered;
        let mut stream = stream;
        let outcome =
            exec::run_pipeline(&mut stream, &database, request.requests, limits, host_bytes);
        let baton = settle(
            &task_host,
            &database,
            principal,
            key,
            existing,
            stream,
            outcome.closed,
        );
        (outcome, baton)
    });
    match task.await {
        // The stream was closed; its transaction rolled back.
        Ok((
            exec::PipelineOutcome {
                stopped: Some(stopped),
                ..
            },
            _,
        )) => match stopped {
            exec::Stopped::TooLarge => error(
                StatusCode::BAD_REQUEST,
                "the pipeline's results exceed the response size limit; send fewer requests at a time",
            ),
            exec::Stopped::HostBusy => error(
                StatusCode::SERVICE_UNAVAILABLE,
                "the server is holding too many database responses; retry",
            ),
        },
        Ok((outcome, Ok(baton))) => {
            let capacity = outcome
                .reservation
                .as_ref()
                .map_or(0, tokio::sync::OwnedSemaphorePermit::num_permits);
            let mut response = Response::new(full(pipeline_body(baton, outcome.results, capacity)));
            response
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            // The host's bytes stay reserved until the connection has
            // written the last of the body, or dropped it.
            if let Some(reservation) = outcome.reservation {
                response = response.map(|body| Holding::new(body, Arc::new(reservation)).boxed());
            }
            response
        }
        Ok((_, Err(message))) => error(StatusCode::SERVICE_UNAVAILABLE, &message),
        Err(join) => {
            tracing::error!(error = %join, "database pipeline panicked");
            if existing {
                remove_entry(host, key);
            }
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

async fn cursor(
    host: &DatabaseHost,
    database: Arc<Database>,
    principal: Principal,
    body: Bytes,
    admitted: Admitted,
) -> Response<Body> {
    let Admitted {
        active,
        permit,
        buffered,
    } = admitted;
    let request: CursorRequest = match crate::proto::parse_request(&body) {
        Ok(request) => request,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("invalid cursor request: {e}"),
            );
        }
    };
    // As for pipelines: the parsed batch replaces the body.
    drop(body);
    if let Err(exec::ProtocolError(message)) =
        exec::validate_batch(&request.batch, host.config().max_condition_depth)
    {
        return error(StatusCode::BAD_REQUEST, &message);
    }
    let (key, stream, existing) = match take_or_open(host, &database, &principal, request.baton) {
        Ok(taken) => taken,
        Err(response) => return response,
    };
    // Cursor streams are retained, and the baton is issued before execution.
    let sequence = match retain_busy(host, &database, &principal, key, existing) {
        Ok(sequence) => sequence,
        Err(message) => return error(StatusCode::SERVICE_UNAVAILABLE, &message),
    };
    let first = serde_json::to_vec(&CursorResponse {
        baton: Some(baton(key, sequence)),
        base_url: None,
    })
    .expect("cursor response serializes");

    // Lines waiting for the client are bounded by count and by bytes; a
    // line larger than the byte budget waits until the queue is empty.
    // Each line also counts, at its full size, against the host's buffer
    // budget, so cursors across databases cannot together hold more.
    type Queued = (
        Bytes,
        tokio::sync::OwnedSemaphorePermit,
        tokio::sync::OwnedSemaphorePermit,
    );
    let (sender, receiver) = tokio::sync::mpsc::channel::<Queued>(64);
    let queued_bytes = Arc::new(tokio::sync::Semaphore::new(CURSOR_QUEUE_BYTES));
    let host_bytes = host.inner.buffered.clone();
    let max_buffered = host.config().max_buffered_bytes;
    let max_row_bytes = host.config().max_response_bytes;
    // Set when the cursor stopped before its end; the body then fails rather
    // than end as if complete.
    let aborted = Arc::new(AtomicBool::new(false));
    let task_aborted = aborted.clone();
    let task_host = host.clone();
    let runtime = tokio::runtime::Handle::current();
    let send_timeout = host.config().cursor_send_timeout;
    tokio::task::spawn_blocking(move || {
        // Dropped last; after a panic it fails the body and frees the stream.
        let _panicked = CursorPanic {
            host: task_host.clone(),
            key,
            aborted: task_aborted.clone(),
        };
        let _active = active;
        let _permit = permit;
        // The parsed batch lives as long as the cursor.
        let _buffered = buffered;
        let mut stream = stream;
        // A client that stops reading must not hold the stream, its
        // transaction, and a request slot indefinitely.
        let acquire = |budget: &Arc<tokio::sync::Semaphore>, need: u32| match budget
            .clone()
            .try_acquire_many_owned(need)
        {
            Ok(permit) => Some(permit),
            Err(tokio::sync::TryAcquireError::Closed) => None,
            Err(tokio::sync::TryAcquireError::NoPermits) => runtime
                .block_on(tokio::time::timeout(
                    send_timeout,
                    budget.clone().acquire_many_owned(need),
                ))
                .ok()
                .and_then(Result::ok),
        };
        let host_share =
            |bytes: usize| u32::try_from(bytes.clamp(1, max_buffered)).unwrap_or(u32::MAX);
        // A row's share of the host's budget is reserved before the row is
        // copied out of SQLite.
        let mut reserve = |bytes: usize| acquire(&host_bytes, host_share(bytes));
        let send = |line: Bytes, reserved: Option<tokio::sync::OwnedSemaphorePermit>| -> bool {
            let queued = line.len().clamp(1, CURSOR_QUEUE_BYTES) as u32;
            let Some(permit) = acquire(&queued_bytes, queued) else {
                return false;
            };
            let held = host_share(line.len()) as usize;
            let host_permit = match reserved {
                Some(mut reserved) => {
                    let have = reserved.num_permits();
                    if have > held {
                        drop(reserved.split(have - held));
                    } else if have < held {
                        let Some(more) = acquire(&host_bytes, (held - have) as u32) else {
                            return false;
                        };
                        reserved.merge(more);
                    }
                    reserved
                }
                None => match acquire(&host_bytes, held as u32) {
                    Some(permit) => permit,
                    None => return false,
                },
            };
            match sender.try_send((line, permit, host_permit)) {
                Ok(()) => true,
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
                Err(tokio::sync::mpsc::error::TrySendError::Full(item)) => runtime
                    .block_on(tokio::time::timeout(send_timeout, sender.send(item)))
                    .is_ok_and(|sent| sent.is_ok()),
            }
        };
        let mut emit = |entry: crate::proto::CursorEntry,
                        reserved: Option<tokio::sync::OwnedSemaphorePermit>|
         -> bool {
            let mut line = serde_json::to_vec(&entry).expect("cursor entry serializes");
            line.push(b'\n');
            send(Bytes::from(line), reserved)
        };
        let mut lead = first;
        lead.push(b'\n');
        let connected = send(Bytes::from(lead), None)
            && exec::run_cursor(
                &mut stream,
                &database,
                &request.batch,
                max_row_bytes,
                &mut reserve,
                &mut emit,
            )
            .is_ok();
        // A stream closed meanwhile (the host was closed) rolled back what the
        // cursor did; the client must not take the end as success.
        if connected {
            if !restore_entry(&task_host, key, stream) {
                task_aborted.store(true, Ordering::Release);
            }
        } else {
            task_aborted.store(true, Ordering::Release);
            remove_entry(&task_host, key);
            drop(stream);
        }
    });

    let body = StreamBody::new(futures_util::stream::unfold(
        (receiver, aborted, false),
        |(mut receiver, aborted, failed)| async move {
            if failed {
                return None;
            }
            match receiver.recv().await {
                // The line keeps its share of the queue's and the host's
                // budgets until the connection has written it.
                Some((line, permit, host_permit)) => Some((
                    Ok(Frame::data(Chunk::held(
                        line,
                        Arc::new((permit, host_permit)),
                    ))),
                    (receiver, aborted, false),
                )),
                None if aborted.load(Ordering::Acquire) => Some((
                    Err::<Frame<Chunk>, BoxError>("the cursor was stopped before its end".into()),
                    (receiver, aborted, true),
                )),
                None => None,
            }
        },
    ));
    let mut response = Response::new(body.boxed());
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    response
}

type StreamKey = [u8; 16];

/// Bytes of cursor output queued for a client that has not read them yet.
const CURSOR_QUEUE_BYTES: usize = 4 * 1024 * 1024;

/// Takes the stream named by `baton`, or starts a new one.
#[allow(clippy::result_large_err)]
fn take_or_open(
    host: &DatabaseHost,
    database: &Database,
    principal: &Principal,
    baton: Option<String>,
) -> Result<(StreamKey, Stream, bool), Response<Body>> {
    let Some(baton) = baton else {
        let mut key = [0u8; 16];
        getrandom::fill(&mut key).expect("operating system random source");
        let mut stream = Stream::new(host.inner.stored_sql.clone());
        stream.read_only = matches!(principal, Principal::Replica(_));
        return Ok((key, stream, false));
    };
    let Some((key, sequence)) = parse_baton(&baton) else {
        return Err(error(StatusCode::BAD_REQUEST, "malformed baton"));
    };
    let mut streams = host.inner.streams.lock();
    let Some(entry) = streams.get_mut(&key) else {
        return Err(error(StatusCode::NOT_FOUND, "stream not found or expired"));
    };
    if entry.database != database.id || &entry.principal != principal || entry.sequence != sequence
    {
        return Err(error(StatusCode::NOT_FOUND, "stream not found or expired"));
    }
    let Some(stream) = entry.stream.take() else {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "stream is busy with another request",
        ));
    };
    Ok((key, stream, true))
}

/// Decides what happens to a stream after a pipeline and returns its next
/// baton. Runs on the blocking thread.
fn settle(
    host: &DatabaseHost,
    database: &Database,
    principal: Principal,
    key: StreamKey,
    existing: bool,
    mut stream: Stream,
    closed: bool,
) -> Result<Option<String>, String> {
    if closed || !stream.has_state() {
        if existing {
            remove_entry(host, key);
        }
        if let Some(conn) = stream.conn.take() {
            database.release(conn);
        }
        return Ok(None);
    }
    if existing {
        let mut streams = host.inner.streams.lock();
        return Ok(match streams.get_mut(&key) {
            Some(entry) => {
                entry.sequence += 1;
                entry.last_used = Instant::now();
                entry.stream = Some(stream);
                Some(baton(key, entry.sequence))
            }
            // The stream was closed while it ran (pause or shutdown), which
            // rolled back its transaction: report that rather than results
            // the client would take as kept.
            None => {
                return Err(
                    "the stream was closed while the request ran (the database was paused or handed over); its transaction was rolled back"
                        .into(),
                );
            }
        });
    }
    // On failure the stream is dropped, which rolls back its transaction.
    host.insert_stream(
        database,
        key,
        StreamEntry {
            database: database.id.clone(),
            principal,
            sequence: 1,
            last_used: Instant::now(),
            stream: Some(stream),
        },
    )
    .map_err(|refused| refused.message)?;
    Ok(Some(baton(key, 1)))
}

/// Registers or advances a stream that stays busy while a cursor runs.
fn retain_busy(
    host: &DatabaseHost,
    database: &Database,
    principal: &Principal,
    key: StreamKey,
    existing: bool,
) -> Result<u64, String> {
    if existing {
        let mut streams = host.inner.streams.lock();
        let entry = streams
            .get_mut(&key)
            .ok_or_else(|| "stream was closed".to_string())?;
        entry.sequence += 1;
        entry.last_used = Instant::now();
        return Ok(entry.sequence);
    }
    host.insert_stream(
        database,
        key,
        StreamEntry {
            database: database.id.clone(),
            principal: principal.clone(),
            sequence: 1,
            last_used: Instant::now(),
            stream: None,
        },
    )
    .map_err(|refused| refused.message)?;
    Ok(1)
}

/// Returns a cursor's stream to its entry. Returns false, dropping the
/// stream and rolling back its transaction, when the entry was closed.
fn restore_entry(host: &DatabaseHost, key: StreamKey, stream: Stream) -> bool {
    let mut streams = host.inner.streams.lock();
    match streams.get_mut(&key) {
        Some(entry) => {
            entry.last_used = Instant::now();
            entry.stream = Some(stream);
            true
        }
        None => {
            drop(streams);
            drop(stream);
            false
        }
    }
}

/// Cleans up after a cursor task that panicked: its body fails rather than
/// end as if complete, and its stream entry, which holds no stream while
/// the cursor runs and so never expires, is removed.
struct CursorPanic {
    host: DatabaseHost,
    key: StreamKey,
    aborted: Arc<AtomicBool>,
}

impl Drop for CursorPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.aborted.store(true, Ordering::Release);
            remove_entry(&self.host, self.key);
        }
    }
}

fn remove_entry(host: &DatabaseHost, key: StreamKey) {
    let removed = host.inner.streams.lock().remove(&key);
    if let Some(entry) = removed
        && let Ok(database) = host.database(&entry.database)
    {
        host.unreserve_stream(&database);
    }
}

fn baton(key: StreamKey, sequence: u64) -> String {
    format!("{}.{sequence}", hex::encode(key))
}

fn parse_baton(baton: &str) -> Option<(StreamKey, u64)> {
    let (key, sequence) = baton.split_once('.')?;
    let mut bytes = [0u8; 16];
    hex::decode_to_slice(key, &mut bytes).ok()?;
    Some((bytes, sequence.parse().ok()?))
}

fn bearer<B>(request: &Request<B>) -> Option<&str> {
    let value = request.headers().get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|token| !token.is_empty())
}

fn forwarded<B>(request: &Request<B>) -> Option<(String, DatabaseId, String)> {
    let header = |name: &str| request.headers().get(name)?.to_str().ok();
    let scope = header(SCOPE_HEADER)?.to_owned();
    let database = DatabaseId::parse(header(DATABASE_HEADER)?).ok()?;
    let key = header(KEY_HEADER)?.to_owned();
    Some((scope, database, key))
}

// The error is the finished response; it is returned once per request.
#[allow(clippy::result_large_err)]
async fn read_body<B>(body: B, limit: usize) -> Result<Bytes, Response<Body>>
where
    B: http_body::Body<Data = Bytes> + Send + Sync + 'static,
    B::Error: Into<BoxError>,
{
    match Limited::new(body, limit).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e)
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some() =>
        {
            Err(error(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("request body exceeds {limit} bytes"),
            ))
        }
        Err(_) => Err(error(
            StatusCode::BAD_REQUEST,
            "failed to read request body",
        )),
    }
}

fn client() -> &'static Client<HttpConnector, Body> {
    static CLIENT: OnceLock<Client<HttpConnector, Body>> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(Duration::from_secs(5)));
        connector.set_nodelay(true);
        // A home server that becomes unreachable mid-request is noticed
        // within about a minute rather than when TCP gives up.
        connector.set_keepalive(Some(Duration::from_secs(15)));
        connector.set_keepalive_interval(Some(Duration::from_secs(5)));
        connector.set_keepalive_retries(Some(6));
        // Below the home server's idle timeout, so this side lets a pooled
        // connection go before the other side closes it.
        Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(20))
            .build(connector)
    })
}

/// Sends a request on to the server that hosts the database, streaming both
/// bodies. The binding token never leaves this server.
/// Where and as whom a request is forwarded.
struct Upstream<'a> {
    address: SocketAddr,
    scope: &'a str,
    database: &'a DatabaseId,
    key: &'a str,
}

async fn forward<B>(host: &DatabaseHost, to: Upstream<'_>, path: &str, body: B) -> Response<Body>
where
    B: http_body::Body<Data = Bytes> + Send + Sync + 'static,
    B::Error: Into<BoxError>,
{
    let upstream = to.address;
    let body = Limited::new(body, host.config().max_request_bytes)
        .map_err(|e| -> BoxError { e })
        .map_frame(|frame| frame.map_data(Chunk::from))
        .boxed();
    let Ok(request) = Request::post(format!("http://{upstream}{path}"))
        .header(CONTENT_TYPE, "application/json")
        .header(SCOPE_HEADER, to.scope)
        .header(DATABASE_HEADER, to.database.as_str())
        .header(KEY_HEADER, to.key)
        .body(body)
    else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid forwarding request",
        );
    };
    // No limit on the wait for the answer: a pipeline there may run for
    // long, one statement within its time limit at a time, and giving up
    // would not stop it, so a retry could apply its writes twice. A home
    // server that becomes unreachable is noticed by TCP keepalive instead.
    match client().request(request).await {
        Ok(response) => response.map(|body| {
            IdleBody::new(
                body.map_err(|e| -> BoxError { Box::new(e) })
                    .map_frame(|frame| frame.map_data(Chunk::from))
                    .boxed(),
                FORWARD_BODY_IDLE,
            )
            .boxed()
        }),
        Err(e) => {
            tracing::warn!(%upstream, error = %e, "database forwarding failed");
            error(
                StatusCode::BAD_GATEWAY,
                "the server hosting this database is unreachable",
            )
        }
    }
}

/// How long a forwarded response may go without sending anything once it
/// has started: a pipeline's body follows its head at once, and a cursor
/// sends each row as it is read, within a statement's time limit.
const FORWARD_BODY_IDLE: Duration = Duration::from_secs(2 * 60);

/// A body that fails once its inner body produces nothing for `idle`.
struct IdleBody {
    body: Body,
    idle: Duration,
    timer: std::pin::Pin<Box<tokio::time::Sleep>>,
}

impl IdleBody {
    fn new(body: Body, idle: Duration) -> Self {
        Self {
            body,
            idle,
            timer: Box::pin(tokio::time::sleep(idle)),
        }
    }
}

impl http_body::Body for IdleBody {
    type Data = Chunk;
    type Error = BoxError;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Chunk>, BoxError>>> {
        let this = &mut *self;
        match std::pin::Pin::new(&mut this.body).poll_frame(cx) {
            std::task::Poll::Ready(frame) => {
                let deadline = tokio::time::Instant::now() + this.idle;
                this.timer.as_mut().reset(deadline);
                std::task::Poll::Ready(frame)
            }
            std::task::Poll::Pending => match this.timer.as_mut().poll(cx) {
                std::task::Poll::Ready(()) => std::task::Poll::Ready(Some(Err(
                    "the server hosting this database stopped sending its response".into(),
                ))),
                std::task::Poll::Pending => std::task::Poll::Pending,
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

/// Encodes a pipeline response as [`PipelineResponse`] does, dropping each
/// result once it is written: the results and their encoding are not both
/// held in full, so the host's reservation for the results covers the body.
/// `capacity` is the encoded size reserved for them, so the buffer does not
/// grow by copying.
fn pipeline_body(
    baton: Option<String>,
    results: Vec<crate::proto::StreamResult>,
    capacity: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(capacity.saturating_add(128));
    out.extend_from_slice(b"{\"baton\":");
    serde_json::to_writer(&mut out, &baton).expect("baton serializes");
    out.extend_from_slice(b",\"base_url\":null,\"results\":[");
    for (index, result) in results.into_iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut out, &result).expect("result serializes");
    }
    out.extend_from_slice(b"]}");
    out
}

fn json(status: StatusCode, value: &impl Serialize) -> Response<Body> {
    let body = serde_json::to_vec(value).expect("response serializes");
    let mut response = Response::new(full(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

fn error(status: StatusCode, message: &str) -> Response<Body> {
    #[derive(Serialize)]
    struct ErrorBody<'a> {
        error: &'a str,
    }
    json(status, &ErrorBody { error: message })
}

fn text(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut response = Response::new(full(message.as_bytes().to_vec()));
    *response.status_mut() = status;
    response
}

/// A body whose chunks keep part of the host's buffer budget reserved: the
/// reservation ends when the connection has written the last of them. It
/// hands its data over in pieces, so the connection takes the next only
/// once it has written most of the last.
struct Holding {
    body: Body,
    pending: Bytes,
    held: Arc<dyn std::any::Any + Send + Sync>,
}

const HOLDING_CHUNK: usize = 64 * 1024;

impl Holding {
    fn new(body: Body, held: Arc<dyn std::any::Any + Send + Sync>) -> Self {
        Self {
            body,
            pending: Bytes::new(),
            held,
        }
    }
}

impl http_body::Body for Holding {
    type Data = Chunk;
    type Error = BoxError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Chunk>, BoxError>>> {
        use bytes::Buf;
        use std::task::Poll;
        let this = self.get_mut();
        loop {
            if !this.pending.is_empty() {
                let take = this.pending.len().min(HOLDING_CHUNK);
                let piece = this.pending.split_to(take);
                return Poll::Ready(Some(Ok(Frame::data(Chunk::held(piece, this.held.clone())))));
            }
            match std::pin::Pin::new(&mut this.body).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(mut data) => this.pending = data.copy_to_bytes(data.remaining()),
                    Err(frame) => return Poll::Ready(Some(Ok(frame))),
                },
                other => return other,
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.pending.is_empty() && self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        let inner = self.body.size_hint();
        let pending = self.pending.len() as u64;
        let mut hint = http_body::SizeHint::new();
        if let Some(upper) = inner.upper() {
            hint.set_upper(upper + pending);
        }
        hint.set_lower(inner.lower() + pending);
        hint
    }
}

fn full(bytes: Vec<u8>) -> Body {
    Full::new(Chunk::from(Bytes::from(bytes)))
        .map_err(|never| -> BoxError { match never {} })
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Error, PipelineResponse, StreamResponse, StreamResult};

    #[test]
    fn pipeline_bodies_encode_as_the_response_type_does() {
        let results = || {
            vec![
                StreamResult::Ok {
                    response: StreamResponse::Close,
                },
                StreamResult::Error {
                    error: Error::new("SQLITE_ERROR", "no such table: \"t\"\n"),
                },
            ]
        };
        let expected = |baton: &Option<String>, results| {
            serde_json::to_vec(&PipelineResponse {
                baton: baton.clone(),
                base_url: None,
                results,
            })
            .unwrap()
        };
        for baton in [None, Some("abc.1".to_owned())] {
            assert_eq!(
                pipeline_body(baton.clone(), results(), 0),
                expected(&baton, results())
            );
            assert_eq!(
                pipeline_body(baton.clone(), Vec::new(), 16),
                expected(&baton, Vec::new())
            );
        }
    }
}

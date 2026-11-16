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
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::Serialize;
use tokio::net::TcpListener;

use crate::exec::{self, Limits};
use crate::host::{Database, DatabaseHost, DatabaseId, Principal, Resolved, Stream, StreamEntry};
use crate::proto::{CursorRequest, CursorResponse, PipelineRequest, PipelineResponse, Version};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = http_body_util::combinators::BoxBody<Bytes, BoxError>;

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
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(%error, "database listener accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = socket.set_nodelay(true);
        let host = host.clone();
        let access = access.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request| {
                let host = host.clone();
                let access = access.clone();
                async move { Ok::<_, Infallible>(handle(&host, &access, peer, request).await) }
            });
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .keep_alive(true)
                .serve_connection(TokioIo::new(socket), service)
                .await
            {
                tracing::debug!(%error, "database connection ended");
            }
        });
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
            let reserve = declared.unwrap_or(limit).clamp(1, limit);
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
    let task = tokio::task::spawn_blocking(move || {
        let _active = active;
        let _permit = permit;
        // The parsed requests live as long as the body did.
        let _buffered = buffered;
        let mut stream = stream;
        let outcome = exec::run_pipeline(&mut stream, &database, request.requests, limits);
        let baton = settle(
            &task_host,
            &database,
            principal,
            key,
            existing,
            stream,
            outcome.closed,
        );
        (outcome.results, outcome.too_large, baton)
    });
    match task.await {
        // The stream was closed; its transaction rolled back.
        Ok((_, true, _)) => error(
            StatusCode::BAD_REQUEST,
            "the pipeline's results exceed the response size limit; send fewer requests at a time",
        ),
        Ok((results, false, Ok(baton))) => json(
            StatusCode::OK,
            &PipelineResponse {
                baton,
                base_url: None,
                results,
            },
        ),
        Ok((_, false, Err(message))) => error(StatusCode::SERVICE_UNAVAILABLE, &message),
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
        let send = |line: Bytes| -> bool {
            let queued = line.len().clamp(1, CURSOR_QUEUE_BYTES) as u32;
            let Some(permit) = acquire(&queued_bytes, queued) else {
                return false;
            };
            let held = u32::try_from(line.len().clamp(1, max_buffered)).unwrap_or(u32::MAX);
            let Some(host_permit) = acquire(&host_bytes, held) else {
                return false;
            };
            match sender.try_send((line, permit, host_permit)) {
                Ok(()) => true,
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
                Err(tokio::sync::mpsc::error::TrySendError::Full(item)) => runtime
                    .block_on(tokio::time::timeout(send_timeout, sender.send(item)))
                    .is_ok_and(|sent| sent.is_ok()),
            }
        };
        let mut emit = |entry: crate::proto::CursorEntry| -> bool {
            let mut line = serde_json::to_vec(&entry).expect("cursor entry serializes");
            line.push(b'\n');
            send(Bytes::from(line))
        };
        let mut lead = first;
        lead.push(b'\n');
        let connected = send(Bytes::from(lead))
            && exec::run_cursor(
                &mut stream,
                &database,
                &request.batch,
                max_row_bytes,
                &mut emit,
            )
            .is_ok();
        if connected {
            restore_entry(&task_host, key, stream);
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
                // The line leaves the queue's byte budget as it is handed on.
                Some((line, _permit, _host_permit)) => {
                    Some((Ok(Frame::data(line)), (receiver, aborted, false)))
                }
                None if aborted.load(Ordering::Acquire) => Some((
                    Err::<Frame<Bytes>, BoxError>("the cursor was stopped before its end".into()),
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
        return Ok((key, Stream::new(host.inner.stored_sql.clone()), false));
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
            // The stream was closed while it ran (pause or shutdown).
            None => None,
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
    .map_err(|limit| limit.message)?;
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
    .map_err(|limit| limit.message)?;
    Ok(1)
}

fn restore_entry(host: &DatabaseHost, key: StreamKey, stream: Stream) {
    let mut streams = host.inner.streams.lock();
    if let Some(entry) = streams.get_mut(&key) {
        entry.last_used = Instant::now();
        entry.stream = Some(stream);
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
        Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
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
    match client().request(request).await {
        Ok(response) => response.map(|body| body.map_err(|e| -> BoxError { Box::new(e) }).boxed()),
        Err(e) => {
            tracing::warn!(%upstream, error = %e, "database forwarding failed");
            error(
                StatusCode::BAD_GATEWAY,
                "the server hosting this database is unreachable",
            )
        }
    }
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

fn full(bytes: Vec<u8>) -> Body {
    Full::new(Bytes::from(bytes))
        .map_err(|never| -> BoxError { match never {} })
        .boxed()
}

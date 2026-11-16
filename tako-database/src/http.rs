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
            let body = match read_body(request.into_body(), host.config().max_request_bytes).await {
                Ok(body) => body,
                Err(response) => return response,
            };
            match endpoint {
                Endpoint::Pipeline(version) => {
                    pipeline(host, database, principal, version, body).await
                }
                Endpoint::Cursor => cursor(host, database, principal, body).await,
            }
        }
    }
}

enum Endpoint {
    Pipeline(Version),
    Cursor,
}

async fn pipeline(
    host: &DatabaseHost,
    database: Arc<Database>,
    principal: Principal,
    version: Version,
    body: Bytes,
) -> Response<Body> {
    let request: PipelineRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("invalid pipeline: {e}")),
    };
    let config = host.config();
    if let Err(exec::ProtocolError(message)) =
        exec::validate_pipeline(&request.requests, version, config.max_condition_depth)
    {
        return error(StatusCode::BAD_REQUEST, &message);
    }
    let active = match DatabaseHost::enter(&database) {
        Ok(active) => active,
        Err(unavailable) => return error(StatusCode::SERVICE_UNAVAILABLE, &unavailable.0),
    };
    let permit = match database.permits.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "database is closing"),
    };
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
        (outcome.results, baton)
    });
    match task.await {
        Ok((results, Ok(baton))) => json(
            StatusCode::OK,
            &PipelineResponse {
                baton,
                base_url: None,
                results,
            },
        ),
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
) -> Response<Body> {
    let request: CursorRequest = match serde_json::from_slice(&body) {
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
    let active = match DatabaseHost::enter(&database) {
        Ok(active) => active,
        Err(unavailable) => return error(StatusCode::SERVICE_UNAVAILABLE, &unavailable.0),
    };
    let permit = match database.permits.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "database is closing"),
    };
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

    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(64);
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
        let mut stream = stream;
        // A client that stops reading must not hold the stream, its
        // transaction, and a request slot indefinitely.
        let send = |line: Bytes| -> bool {
            match sender.try_send(line) {
                Ok(()) => true,
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
                Err(tokio::sync::mpsc::error::TrySendError::Full(line)) => runtime
                    .block_on(tokio::time::timeout(send_timeout, sender.send(line)))
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
            && exec::run_cursor(&mut stream, &database, &request.batch, &mut emit).is_ok();
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
                Some(line) => Some((Ok(Frame::data(line)), (receiver, aborted, false))),
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
        return Ok((key, Stream::new(), false));
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
    if let Err(limit) = host.reserve_stream(database) {
        // Dropping the stream rolls back its transaction.
        return Err(limit.message);
    }
    host.inner.streams.lock().insert(
        key,
        StreamEntry {
            database: database.id.clone(),
            principal,
            sequence: 1,
            last_used: Instant::now(),
            stream: Some(stream),
        },
    );
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
    host.reserve_stream(database)
        .map_err(|limit| limit.message)?;
    host.inner.streams.lock().insert(
        key,
        StreamEntry {
            database: database.id.clone(),
            principal: principal.clone(),
            sequence: 1,
            last_used: Instant::now(),
            stream: None,
        },
    );
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

//! Primary-owned management dispatch. TLS proxies use only `relay`.
#![cfg(target_os = "linux")]

use std::{
    convert::Infallible,
    fmt::Display,
    fs::{self, Metadata},
    future::Future,
    io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    Request, Response, StatusCode, Version,
    body::{Body, Incoming},
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use tako_network::{NodeConfig, RequestId, node_config::protected::Directory};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};

use crate::{ServerState, native_management_ownership::PrimaryHandoff};

const SOCKET: &str = "management.sock";
const PUBLICATION_LOCK: &str = "agent-management-publish.lock";
const MAX_CONNECTIONS: usize = 32;
const MAX_HEADERS: usize = 128;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const HEADER_TIME: Duration = Duration::from_secs(30);
const CONNECT_TIME: Duration = Duration::from_secs(5);
const EXCHANGE_TIME: Duration = Duration::from_secs(4000);
static RELAYS: Semaphore = Semaphore::const_new(MAX_CONNECTIONS);

type HttpResponse = Response<Full<Bytes>>;

fn io_error(error: impl Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn service_uid() -> io::Result<u32> {
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 16 * 1024];
    let status = unsafe {
        libc::getpwnam_r(
            c"tako".as_ptr(),
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return Err(io_error("installed tako account is required"));
    }
    Ok(entry.pw_uid)
}

fn allowed_uid(uid: u32, tako_uid: u32) -> bool {
    uid == 0 || uid == tako_uid
}

fn authenticate(stream: &UnixStream, tako_uid: u32) -> io::Result<()> {
    if allowed_uid(stream.peer_cred()?.uid(), tako_uid) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "untrusted management peer",
        ))
    }
}

fn check_socket(path: &Path, tako_uid: u32) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_socket()
        && allowed_uid(meta.uid(), tako_uid)
        && meta.mode() & 0o7777 == 0o600
    {
        Ok(())
    } else {
        Err(io_error("unsafe native management socket"))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl From<&Metadata> for FileIdentity {
    fn from(meta: &Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
        }
    }
}

struct SocketOwner {
    directory: Directory,
    path: PathBuf,
    identity: FileIdentity,
}

impl Drop for SocketOwner {
    fn drop(&mut self) {
        // Publishers take this brief lock too; inode comparison alone races rename.
        let Ok(_lock) = self.directory.lock(PUBLICATION_LOCK) else {
            return;
        };
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|meta| FileIdentity::from(&meta) == self.identity)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn bind(data_dir: &Path, tako_uid: u32) -> io::Result<(UnixListener, SocketOwner)> {
    if !allowed_uid(unsafe { libc::geteuid() }, tako_uid) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "untrusted management owner",
        ));
    }
    let config = NodeConfig::load(data_dir).map_err(io_error)?;
    let root = config.network_dir(data_dir);
    let directory = Directory::open(&root, true).map_err(io_error)?;
    let lock = directory.lock(PUBLICATION_LOCK).map_err(io_error)?;
    let path = root.join(SOCKET);
    match check_socket(&path, tako_uid) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    let temporary = root.join(format!(
        "agent-management-{}.sock",
        RequestId::random().map_err(io_error)?
    ));
    let listener = UnixListener::bind(&temporary)?;
    let identity = FileIdentity::from(&fs::symlink_metadata(&temporary)?);
    let mut owner = SocketOwner {
        directory,
        path: temporary,
        identity,
    };
    let published = (|| -> io::Result<()> {
        fs::set_permissions(&owner.path, fs::Permissions::from_mode(0o600))?;
        check_socket(&owner.path, tako_uid)?;
        fs::rename(&owner.path, &path)?;
        owner.path = path;
        Ok(())
    })();
    drop(lock);
    published?;
    Ok((listener, owner))
}

/// Parent lifecycle starts this only for the selected primary and owns cancellation.
pub(crate) async fn serve(
    data_dir: PathBuf,
    state: Arc<ServerState>,
    handoff: PrimaryHandoff,
) -> io::Result<()> {
    let prepared = (|| {
        let uid = service_uid()?;
        let (listener, owner) = bind(&data_dir, uid)?;
        Ok::<_, io::Error>((uid, listener, owner))
    })();
    let (uid, listener, _owner) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::error!(%error, "Native primary management listener failed to bind");
            return Err(error);
        }
    };
    let generation = handoff.finish();
    let mut tasks = JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            _ = tasks.join_next(), if !tasks.is_empty() => (),
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error),
                };
                if authenticate(&stream, uid).is_err() { continue; }
                let state = state.clone();
                let generation = generation.clone();
                spawn_connection(&mut tasks, async move {
                    let _ = serve_connection(stream, move |request| {
                        let state = state.clone();
                        let admitted = generation.admit();
                        async move {
                            let Ok(guard) = admitted else {
                                return error(StatusCode::SERVICE_UNAVAILABLE, "Primary management ownership changed");
                            };
                            crate::native_management::handle_request(request, state, guard).await
                        }
                    }, EXCHANGE_TIME).await;
                });
            }
        }
    };
    tasks.shutdown().await;
    result
}

fn spawn_connection<F>(tasks: &mut JoinSet<()>, future: F) -> bool
where
    F: Future<Output = ()> + Send + 'static,
{
    if tasks.len() >= MAX_CONNECTIONS {
        return false;
    }
    tasks.spawn(future);
    true
}

async fn serve_connection<I, H, F>(stream: I, handler: H, deadline: Duration) -> io::Result<()>
where
    I: AsyncRead + AsyncWrite + Unpin,
    H: Fn(Request<Incoming>) -> F,
    F: Future<Output = HttpResponse>,
{
    let service = service_fn(move |request| {
        let response = handler(request);
        async move {
            let response = response.await;
            let response = if response
                .body()
                .size_hint()
                .upper()
                .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
            {
                error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Management response exceeds limit",
                )
            } else {
                response
            };
            Ok::<_, Infallible>(response)
        }
    });
    timeout(
        deadline,
        hyper::server::conn::http1::Builder::new()
            .keep_alive(false)
            .max_headers(MAX_HEADERS)
            .max_buf_size(MAX_HEADER_BYTES)
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_TIME)
            .serve_connection(TokioIo::new(stream), service),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "management connection expired"))?
    .map_err(io_error)
}

fn bounded_headers<B>(request: &Request<B>) -> bool {
    let uri = request.uri();
    let uri_bytes = uri
        .scheme_str()
        .map_or(0, |scheme| scheme.len() + 3)
        .saturating_add(
            uri.authority()
                .map_or(0, |authority| authority.as_str().len()),
        )
        .saturating_add(uri.path_and_query().map_or(0, |path| path.as_str().len()));
    let mut bytes = request
        .method()
        .as_str()
        .len()
        .saturating_add(uri_bytes)
        .saturating_add(16);
    for (name, value) in request.headers() {
        bytes = bytes
            .saturating_add(name.as_str().len())
            .saturating_add(value.len())
            .saturating_add(4);
    }
    request.headers().len() <= MAX_HEADERS && bytes <= MAX_HEADER_BYTES
}

/// Transport only. Failure can follow a committed mutation; never retry automatically.
pub(crate) async fn relay<B>(data_dir: &Path, request: Request<B>) -> HttpResponse
where
    B: Body<Data = Bytes> + Unpin + Send + 'static,
    B::Error: Display + 'static,
{
    if !bounded_headers(&request) {
        return error(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "Management headers exceed limit",
        );
    }
    let Ok(_permit) = RELAYS.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Primary management is busy",
        );
    };
    let connected = timeout(CONNECT_TIME, async {
        let uid = service_uid()?;
        let config = NodeConfig::load(data_dir).map_err(io_error)?;
        let root = config.network_dir(data_dir);
        let _directory = Directory::open(&root, true).map_err(io_error)?;
        let path = root.join(SOCKET);
        check_socket(&path, uid)?;
        let stream = UnixStream::connect(path).await?;
        authenticate(&stream, uid)?;
        Ok::<_, io::Error>(stream)
    })
    .await;
    match connected {
        Ok(Ok(stream)) => relay_io(stream, request, EXCHANGE_TIME).await,
        _ => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Primary management is unavailable",
        ),
    }
}

async fn relay_io<I, B>(stream: I, mut request: Request<B>, deadline: Duration) -> HttpResponse
where
    I: AsyncRead + AsyncWrite + Unpin,
    B: Body<Data = Bytes> + Unpin + 'static,
    B::Error: Display + 'static,
{
    if !bounded_headers(&request) {
        return error(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "Management headers exceed limit",
        );
    }
    *request.version_mut() = Version::HTTP_11;
    request
        .headers_mut()
        .insert(hyper::header::CONNECTION, "close".parse().unwrap());
    let request = request.map(|body| body.map_err(io_error));
    let exchange = async {
        let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
            .max_headers(MAX_HEADERS)
            .max_buf_size(MAX_HEADER_BYTES)
            .handshake(TokioIo::new(stream))
            .await
            .map_err(io_error)?;
        let mut connection = Box::pin(connection);
        let response = async {
            let response = sender.send_request(request).await.map_err(io_error)?;
            let (parts, body) = response.into_parts();
            if body
                .size_hint()
                .upper()
                .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
            {
                return Err(io_error("management response exceeds limit"));
            }
            let bytes = Limited::new(body, MAX_RESPONSE_BYTES)
                .collect()
                .await
                .map_err(io_error)?
                .to_bytes();
            Ok::<_, io::Error>(Response::from_parts(parts, Full::new(bytes)))
        };
        tokio::pin!(response);
        tokio::select! {
            biased;
            result = &mut response => result,
            _ = &mut connection => {
                drop(connection);
                response.await
            }
        }
    };
    match timeout(deadline, exchange).await {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => error(
            StatusCode::BAD_GATEWAY,
            "Primary management response is unavailable",
        ),
        Err(_) => error(
            StatusCode::GATEWAY_TIMEOUT,
            "Management outcome is unconfirmed",
        ),
    }
}

fn error(status: StatusCode, message: &'static str) -> HttpResponse {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .header("connection", "close")
        .body(Full::new(Bytes::from(
            serde_json::json!({"error": message}).to_string(),
        )))
        .unwrap()
}

#[cfg(test)]
#[path = "native_management_listener/tests.rs"]
mod tests;

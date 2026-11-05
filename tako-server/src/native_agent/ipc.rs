use super::{
    AgentError, AgentRequest, AgentResponse, CONNECTIONS, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
    QUEUE_SIZE, Result,
    controller::{self, Work},
    lifecycle::{self, BrokerOwner, Control},
};
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::Path,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use tako_network::{BootId, NodeConfig, api::SignedRequest, node_config::protected::Directory};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Semaphore, oneshot},
    task::JoinSet,
    time::timeout,
};

const FRAME_TIME: Duration = Duration::from_secs(5);
const RELAY_TIME: Duration = Duration::from_secs(60);

/// One JSON line and EOF. Callers half-close their write side. Bounded before
/// parsing; embedded/new trailing frames and missing delimiters are rejected.
async fn read_frame<R: AsyncRead + Unpin>(stream: &mut R, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if bytes.len() + n > limit + 1 {
            return Err(AgentError::Invalid("IPC frame exceeds limit"));
        }
        bytes.extend_from_slice(&chunk[..n]);
        if bytes[..bytes.len().saturating_sub(1)].contains(&b'\n') {
            return Err(AgentError::Invalid("IPC accepts exactly one JSON line"));
        }
    }
    if bytes.pop() != Some(b'\n') || bytes.is_empty() || bytes.contains(&b'\r') {
        return Err(AgentError::Invalid("IPC requires one terminated JSON line"));
    }
    Ok(bytes)
}

fn check_signed_size(signed: &SignedRequest) -> Result<()> {
    if signed.signature.len() > tako_network::api::MAX_SIGNATURE_BYTES
        || serde_json::to_vec(signed)?.len() > tako_network::api::MAX_RPC_BYTES
    {
        return Err(AgentError::Invalid("signed request exceeds limit"));
    }
    Ok(())
}

fn service_uid() -> Result<u32> {
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 16384];
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
        return Err(AgentError::Invalid("installed tako account is required"));
    }
    Ok(entry.pw_uid)
}
fn authorized(uid: u32, tako_uid: u32) -> bool {
    uid == 0 || uid == tako_uid
}
fn authenticate(stream: &UnixStream, tako_uid: u32) -> Result<()> {
    if !authorized(stream.peer_cred()?.uid(), tako_uid) {
        return Err(AgentError::Invalid("unauthorized Unix peer"));
    }
    Ok(())
}

/// Proxy entry point. Errors mean no acknowledged IPC result. A timeout may
/// follow a committed mutation; retry the signed protocol with its idempotency
/// identifiers, never assume timeout means no mutation occurred.
pub async fn relay(
    data_dir: &Path,
    method: &str,
    path: &str,
    bytes: &[u8],
) -> Result<AgentResponse> {
    if bytes.len() > tako_network::api::MAX_RPC_BYTES || method.len() > 16 || path.len() > 256 {
        return Err(AgentError::Invalid("RPC input exceeds limit"));
    }
    let signed: SignedRequest = serde_json::from_slice(bytes)?;
    check_signed_size(&signed)?;
    let config = NodeConfig::load(data_dir)?;
    let socket = config.socket_path(data_dir);
    Directory::open(&config.network_dir(data_dir), true)?;
    let uid = service_uid()?;
    let meta = std::fs::symlink_metadata(&socket)?;
    if !meta.file_type().is_socket() || meta.uid() != uid || meta.mode() & 0o7777 != 0o600 {
        return Err(AgentError::Invalid("unsafe agent socket"));
    }
    let request = AgentRequest {
        method: method.into(),
        path: path.into(),
        signed,
    };
    let mut wire = serde_json::to_vec(&request)?;
    if wire.len() > MAX_REQUEST_BYTES {
        return Err(AgentError::Invalid("IPC request exceeds limit"));
    }
    wire.push(b'\n');
    timeout(RELAY_TIME, async {
        let mut stream = UnixStream::connect(socket).await?;
        authenticate(&stream, uid)?;
        stream.write_all(&wire).await?;
        stream.shutdown().await?;
        let bytes = read_frame(&mut stream, MAX_RESPONSE_BYTES).await?;
        let response: AgentResponse = serde_json::from_slice(&bytes)?;
        if !(100..=599).contains(&response.status)
            || (response.response.is_some() && response.error.is_some())
        {
            return Err(AgentError::Invalid("invalid agent response"));
        }
        Ok(response)
    })
    .await
    .map_err(|_| AgentError::Timeout)?
}

/// Run only in the separate unprivileged systemd agent process. This function
/// owns SIGTERM/SIGINT and returns after bounded owned-child shutdown.
/// If DB owners cannot drain in 120 seconds, this dedicated process exits with
/// failure rather than detaching them or reporting a clean shutdown.
pub async fn run(data_dir: &Path) -> Result<()> {
    if data_dir != Path::new("/opt/tako") {
        return Err(AgentError::Invalid(
            "host helper requires /opt/tako data directory",
        ));
    }
    let uid = service_uid()?;
    if unsafe { libc::geteuid() } != uid || uid == 0 {
        return Err(AgentError::Invalid(
            "network agent must run as the unprivileged tako account",
        ));
    }
    // This is a dedicated process, before starting workers or creating sockets.
    unsafe {
        libc::umask(0o077);
    }
    let config = NodeConfig::load(data_dir)?;
    let directory = Directory::open(&config.network_dir(data_dir), true)?;
    let lock = directory.lock("agent-process-lock")?;
    let socket = config.socket_path(data_dir);
    match std::fs::symlink_metadata(&socket) {
        Ok(meta)
            if meta.file_type().is_socket()
                && meta.uid() == uid
                && meta.mode() & 0o7777 == 0o600 =>
        {
            std::fs::remove_file(&socket)?
        }
        Ok(_) => {
            return Err(AgentError::Invalid(
                "refusing to replace unexpected socket path",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let _socket_cleanup = SocketCleanup(socket);
    let boot = BootId::random()?;
    let (tx, rx) = mpsc::sync_channel(QUEUE_SIZE);
    let (finished_tx, mut finished_rx) = oneshot::channel();
    let (control, membership, broker_shutdown) = Control::new();
    let worker_control = control.clone();
    let broker_config = config.clone();
    let broker_data_dir = data_dir.to_owned();
    let worker_boot = boot.clone();
    let data_dir = data_dir.to_owned();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let worker = std::thread::Builder::new()
        .name("tako-network-agent".into())
        .spawn(move || {
            let outcome = controller::worker(config, data_dir, worker_boot, rx, worker_control);
            let _ = finished_tx.send(outcome);
        })?;
    // One broker for this agent lifetime, independent of every proxy reload.
    let broker_control = control.clone();
    let mut broker = tokio::spawn(async move {
        let mut owner = BrokerOwner::new(broker_control);
        let result = crate::native_channels::run(
            broker_data_dir,
            broker_config,
            membership,
            broker_shutdown,
        )
        .await;
        owner.complete(result.is_err());
        result
    });
    let permits = Arc::new(Semaphore::new(CONNECTIONS));
    let mut tasks = JoinSet::new();
    let mut worker_done = false;
    let mut broker_done = false;
    let outcome = loop {
        tokio::select! {
            biased;
            result = &mut broker => {
                broker_done = true;
                break broker_outcome(result, false);
            }
            result = &mut finished_rx => {
                worker_done = true;
                break result.map_err(|_| AgentError::Invalid("controller thread stopped unexpectedly")).and_then(|r| r);
            }
            _ = terminate.recv() => break Ok(()),
            _ = interrupt.recv() => break Ok(()),
            _ = tasks.join_next(), if !tasks.is_empty() => (),
            accepted = listener.accept() => {
                let (stream, _) = match accepted { Ok(value) => value, Err(error) => break Err(error.into()) };
                // Check SO_PEERCRED before allocating a task, frame, or queue item.
                if authenticate(&stream, uid).is_err() { continue; }
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let tx = tx.clone();
                let boot = boot.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let _ = serve(stream, tx, boot).await;
                });
            }
        }
    };
    // Withdrawal is synchronous and precedes accepting any further work or
    // asking either DB owner to drain. Failed broker/IPC exits request isolation.
    control.shutdown(outcome.is_err());
    tasks.abort_all();
    drop(tx);
    let joins = async {
        while tasks.join_next().await.is_some() {}
        let broker_result = if broker_done {
            Ok(())
        } else {
            broker_outcome((&mut broker).await, true)
        };
        // BrokerOwner has marked this before publishing completion; the
        // controller retains its DB lock until that marker is visible.
        if broker_result.is_err() {
            control.shutdown(true);
        }
        let worker_result = if worker_done {
            Ok(())
        } else {
            (&mut finished_rx)
                .await
                .map_err(|_| AgentError::Invalid("controller thread failed during shutdown"))
                .and_then(|result| result)
        };
        (broker_result, worker_result)
    };
    let (broker_result, worker_result) =
        match lifecycle::drain(lifecycle::DRAIN_DEADLINE, joins).await {
            Ok(results) => results,
            Err(_) => lifecycle::drain_timeout(),
        };
    // finished_rx is sent only after Controller has dropped. Joining must never
    // be replaced with dropping this handle (which would detach the DB owner).
    let joined = worker
        .join()
        .map_err(|_| AgentError::Invalid("controller thread panicked"));
    drop(lock);
    outcome.and(broker_result).and(worker_result).and(joined)
}
fn broker_outcome(
    result: std::result::Result<std::io::Result<()>, tokio::task::JoinError>,
    stopping: bool,
) -> Result<()> {
    match result {
        Ok(Ok(())) if stopping => Ok(()),
        Ok(Ok(())) => Err(AgentError::Invalid("channel broker exited unexpectedly")),
        Ok(Err(_)) => Err(AgentError::Invalid("channel broker failed")),
        Err(_) => Err(AgentError::Invalid("channel broker task failed")),
    }
}

struct SocketCleanup(std::path::PathBuf);
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn serve(mut stream: UnixStream, tx: mpsc::SyncSender<Work>, boot: BootId) -> Result<()> {
    let frame = timeout(FRAME_TIME, read_frame(&mut stream, MAX_REQUEST_BYTES))
        .await
        .map_err(|_| AgentError::Timeout)??;
    let request: AgentRequest = serde_json::from_slice(&frame)?;
    if request.method.len() > 16 || request.path.len() > 256 {
        return Err(AgentError::Invalid("IPC route exceeds limit"));
    }
    check_signed_size(&request.signed)?;
    let (reply, receiver) = oneshot::channel();
    let response = if tx
        .try_send(Work {
            request,
            reply,
            deadline: Instant::now() + RELAY_TIME,
        })
        .is_err()
    {
        AgentResponse::unavailable(&boot)
    } else {
        match timeout(RELAY_TIME, receiver).await {
            Ok(Ok(response)) => response,
            _ => AgentResponse::unavailable(&boot),
        }
    };
    let mut bytes = serde_json::to_vec(&response)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(AgentError::Invalid("IPC response exceeds limit"));
    }
    bytes.push(b'\n');
    timeout(FRAME_TIME, async {
        stream.write_all(&bytes).await?;
        stream.shutdown().await
    })
    .await
    .map_err(|_| AgentError::Timeout)??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn frame(bytes: &[u8], limit: usize) -> Result<Vec<u8>> {
        let (mut writer, mut reader) = tokio::io::duplex(bytes.len() + 1);
        writer.write_all(bytes).await.unwrap();
        writer.shutdown().await.unwrap();
        read_frame(&mut reader, limit).await
    }
    #[tokio::test]
    async fn bounded_single_line_and_eof_required() {
        assert_eq!(frame(b"{}\n", 2).await.unwrap(), b"{}");
        for bytes in [
            b"{}".as_slice(),
            b"{}\n{}\n",
            b"{}\n ",
            b"\n",
            b"{}\r\n",
            b"123\n",
        ] {
            assert!(frame(bytes, 2).await.is_err());
        }
    }
    #[tokio::test]
    async fn stalled_authenticated_peer_has_a_deadline() {
        let (_writer, mut reader) = tokio::io::duplex(8);
        assert!(
            timeout(Duration::from_millis(10), read_frame(&mut reader, 10))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn broker_errors_propagate_including_during_shutdown() {
        assert!(broker_outcome(Ok(Ok(())), false).is_err());
        broker_outcome(Ok(Ok(())), true).unwrap();
        assert!(broker_outcome(Ok(Err(std::io::Error::other("fixture"))), true).is_err());
        let panicked = tokio::spawn(async {
            panic!("fixture broker failure");
            #[allow(unreachable_code)]
            Ok(())
        })
        .await;
        assert!(broker_outcome(panicked, true).is_err());
    }
    #[test]
    fn root_and_service_only() {
        assert!(authorized(0, 900));
        assert!(authorized(900, 900));
        assert!(!authorized(901, 900));
    }
}

//! Single shared Tako internal socket.
//!
//! One socket per tako-server instance handles every server-side SDK RPC:
//! workflow enqueue + worker RPCs, server-side channel `publish()`, and any
//! future server-routed command. Commands carry an `app` field; the
//! handler uses a lookup closure to find the app's `RunsDb` and dispatch
//! signaler (channel publish takes a separate route).
//!
//! Path convention: `{data_dir}/internal.sock` (symlink) →
//! `{data_dir}/internal-{pid}.sock` (the actual bound socket). Mirrors
//! the management-socket pattern so two tako-server processes can hand
//! over cleanly during upgrade.
//!
//! Auth: filesystem access through the shared `tako-app` group plus a peer-UID
//! check binding each production request to its per-app Unix identity.

use std::ffi::CString;
use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tako_core::{Command, ErrorCode, Response, SocketError};
use tako_socket::serve_jsonl_connection;
use tokio::net::UnixListener;
use tokio::sync::oneshot;

use super::enqueue::RunsDb;

/// Callback fired whenever a durable write means work might be runnable.
/// The manager wires this to a dispatcher, not directly to the supervisor,
/// so enqueue acceptance and worker wake are separate steps.
pub type OnEnqueue = Arc<dyn Fn() + Send + Sync>;

/// Pre-enqueue probe: returns `Err(reason)` when the worker can't currently
/// process jobs (e.g. crash-looping after bootstrap failure). Gives the SDK
/// workflow `.enqueue()` call a chance to reject loudly instead of queuing
/// into a black hole.
pub type HealthCheck = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Fired whenever a worker successfully claims a run. Signals to the
/// supervisor's crash-loop guard that the current process is making
/// forward progress.
pub type OnClaimed = Arc<dyn Fn() + Send + Sync>;

/// Per-app handlers the internal socket needs to service one connection.
#[derive(Clone)]
pub struct AppHandlers {
    pub db: Arc<RunsDb>,
    pub limiter: Arc<crate::in_flight::InFlightLimiter>,
    pub accepting_new_work: Arc<AtomicBool>,
    pub on_enqueue: OnEnqueue,
    pub health_check: HealthCheck,
    pub on_claimed: OnClaimed,
}

/// Lookup: given an app name, return the handlers for that app, or
/// `None` if the app isn't registered.
pub type AppLookup = Arc<dyn Fn(&str) -> Option<AppHandlers> + Send + Sync>;

/// One `Command::ChannelPublish` or `Command::ChannelClose` from an
/// authenticated app process. `channel` and `params` select the stream;
/// `build` is the publisher's build, when it declared one.
pub struct ChannelPublishRequest<'a> {
    pub app: &'a str,
    pub channel: &'a str,
    pub params: &'a serde_json::Value,
    pub build: Option<&'a str>,
    pub write: ChannelWrite,
}

/// What a channel request writes to its stream.
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelWrite {
    /// One `{ type, data, lifecycle? }` publish payload.
    Publish(serde_json::Value),
    /// The stream's terminal marker, with the channel definition's lifecycle.
    Close {
        lifecycle: Option<serde_json::Value>,
    },
}

/// Closure that appends a `ChannelPublishPayload` to an app's channel
/// store. Returns the stored message as a JSON value on success, or the
/// error the socket replies with (its code tells a closed stream from bad
/// params). Abstracted this way so `tako-workflows` doesn't need a direct
/// dep on `tako-channels`.
pub type ChannelPublishFn =
    Arc<dyn Fn(ChannelPublishRequest<'_>) -> Result<serde_json::Value, SocketError> + Send + Sync>;

/// When set, every command's `app` must be authorized for the connecting
/// process uid (from `SO_PEERCRED`). Used in production so one app cannot
/// enqueue, claim, or publish as another.
pub type PeerAuthFn = Arc<dyn Fn(&str, u32) -> Result<(), String> + Send + Sync>;

/// Routes a validated destination after the socket has authenticated the caller.
pub type NetworkCallFn =
    Arc<dyn Fn(&str, &str, tako_core::network::NetworkInvocation) -> Response + Send + Sync>;

/// Handle to the running socket. Drop to stop accepting + remove files.
pub struct EnqueueSocketHandle {
    #[allow(dead_code)]
    symlink_path: PathBuf,
    actual_path: PathBuf,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl EnqueueSocketHandle {
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.await;
        }
        let _ = std::fs::remove_file(&self.actual_path);
    }
}

impl Drop for EnqueueSocketHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_file(&self.actual_path);
    }
}

/// Bind the internal socket and start the accept loop.
///
/// `symlink_path` is the well-known path SDKs connect to. The actual bind
/// happens on `{symlink_dir}/{stem}-{pid}.sock` (where `stem` is derived
/// from the symlink filename — e.g. `internal` → `internal-42.sock`) and
/// the symlink is atomically swapped to point at it. Same pattern as the
/// mgmt socket, so two tako-server processes can hand over without
/// dropping clients.
pub fn spawn(
    symlink_path: impl AsRef<Path>,
    lookup: AppLookup,
    channel_publish: Option<ChannelPublishFn>,
    peer_auth: Option<PeerAuthFn>,
    network_call: Option<NetworkCallFn>,
) -> std::io::Result<EnqueueSocketHandle> {
    let symlink_path = symlink_path.as_ref().to_path_buf();
    let dir = symlink_path
        .parent()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "socket path has no parent",
            )
        })?
        .to_path_buf();
    std::fs::create_dir_all(&dir)?;

    let pid = std::process::id();
    let stem = symlink_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "internal".to_string());
    let actual_path = dir.join(format!("{stem}-{pid}.sock"));

    // Stale pid-specific file from a previous run with the same pid.
    let _ = std::fs::remove_file(&actual_path);

    let std_listener = std::os::unix::net::UnixListener::bind(&actual_path)?;
    std_listener.set_nonblocking(true)?;
    configure_socket_permissions(&actual_path)?;

    // Atomically swap symlink: write temp, rename over target.
    let temp_link = symlink_path.with_extension("tmp");
    let _ = std::fs::remove_file(&temp_link);
    std::os::unix::fs::symlink(&actual_path, &temp_link)?;
    std::fs::rename(&temp_link, &symlink_path)?;

    tracing::info!(
        actual = %actual_path.display(),
        symlink = %symlink_path.display(),
        "Internal socket listening"
    );

    let listener = UnixListener::from_std(std_listener)?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let join = tokio::spawn(run(
        listener,
        lookup,
        channel_publish,
        peer_auth,
        network_call,
        shutdown_rx,
    ));

    Ok(EnqueueSocketHandle {
        symlink_path,
        actual_path,
        shutdown_tx: Some(shutdown_tx),
        join: Some(join),
    })
}

fn configure_socket_permissions(path: &Path) -> std::io::Result<()> {
    if let Some(gid) = app_socket_gid(lookup_group_gid, is_root(), &process_groups()?)? {
        chown_group(path, gid)?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
}

fn app_socket_gid(
    lookup: impl Fn(&str) -> std::io::Result<Option<u32>>,
    is_root: bool,
    groups: &[u32],
) -> std::io::Result<Option<u32>> {
    Ok(lookup("tako-app")?.filter(|gid| is_root || groups.contains(gid)))
}

fn process_groups() -> std::io::Result<Vec<u32>> {
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if count < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut groups = vec![0; count as usize];
    if unsafe { libc::getgroups(count, groups.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    groups.push(unsafe { libc::getegid() });
    Ok(groups)
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and returns the effective user id.
    unsafe { libc::geteuid() == 0 }
}

fn lookup_group_gid(name: &str) -> std::io::Result<Option<u32>> {
    let name = CString::new(name).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "group name contains interior NUL byte",
        )
    })?;
    let mut entry = std::mem::MaybeUninit::<libc::group>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buf = vec![0u8; group_buffer_size()];

    loop {
        // SAFETY: name is a valid C string; entry, result, and buf point to
        // writable storage for getgrnam_r.
        let rc = unsafe {
            libc::getgrnam_r(
                name.as_ptr(),
                entry.as_mut_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == 0 {
            if result.is_null() {
                return Ok(None);
            }
            // SAFETY: getgrnam_r returned success and result points at entry.
            let entry = unsafe { entry.assume_init() };
            return Ok(Some(entry.gr_gid));
        }
        if rc == libc::ERANGE {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        return Err(std::io::Error::from_raw_os_error(rc));
    }
}

fn group_buffer_size() -> usize {
    // SAFETY: sysconf has no preconditions for _SC_GETGR_R_SIZE_MAX.
    let size = unsafe { libc::sysconf(libc::_SC_GETGR_R_SIZE_MAX) };
    if size > 0 { size as usize } else { 16 * 1024 }
}

fn chown_group(path: &Path, gid: u32) -> std::io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path contains interior NUL byte",
        )
    })?;
    // SAFETY: path is a valid C string. uid -1 means unchanged on Unix.
    let rc = unsafe { libc::chown(path.as_ptr(), !0 as libc::uid_t, gid as libc::gid_t) };
    if rc == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

async fn run(
    listener: UnixListener,
    lookup: AppLookup,
    channel_publish: Option<ChannelPublishFn>,
    peer_auth: Option<PeerAuthFn>,
    network_call: Option<NetworkCallFn>,
    shutdown_rx: oneshot::Receiver<()>,
) {
    tokio::pin!(shutdown_rx);
    loop {
        tokio::select! {
            _ = &mut shutdown_rx => break,
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        let lookup = lookup.clone();
                        let channel_publish = channel_publish.clone();
                        let peer_auth = peer_auth.clone();
                        let network_call = network_call.clone();
                        let peer_uid = stream.peer_cred().ok().map(|cred| cred.uid());
                        tokio::spawn(async move {
                            let _ = serve_jsonl_connection(
                                stream,
                                move |cmd: Command| {
                                    let lookup = lookup.clone();
                                    let channel_publish = channel_publish.clone();
                                    let peer_auth = peer_auth.clone();
                                    let network_call = network_call.clone();
                                    async move {
                                        let is_network = matches!(&cmd, Command::NetworkCall { .. });
                                        let operation = move || {
                                            handle_command(
                                                &lookup,
                                                channel_publish.as_ref(),
                                                peer_auth.as_ref(),
                                                peer_uid,
                                                network_call.as_ref(),
                                                cmd,
                                            )
                                        };
                                        let result = if is_network {
                                            crate::blocking::run_network(operation).await
                                        } else {
                                            crate::blocking::run(operation)
                                                .await
                                                .map_err(|error| SocketError::unclassified(error.to_string()))
                                        };
                                        match result {
                                            Ok(response) => response,
                                            Err(SocketError { code: Some(code), message }) => {
                                                Response::coded_error(code, message)
                                            }
                                            Err(error) => {
                                                tracing::error!(%error, "Internal storage operation failed");
                                                Response::error("Internal storage operation failed".to_string())
                                            }
                                        }
                                    }
                                },
                                |e| {
                                    Response::coded_error(
                                        ErrorCode::InvalidRequest,
                                        format!("invalid request: {e}"),
                                    )
                                },
                            )
                            .await;
                        });
                    }
                    Err(e) => {
                        tracing::warn!(?e, "internal socket accept error");
                    }
                }
            }
        }
    }
}

/// Extract the app from any command the internal socket accepts. Returns
/// None for commands that don't carry an app (none currently — every
/// supported command carries it).
fn authorize_peer(
    peer_auth: Option<&PeerAuthFn>,
    peer_uid: Option<u32>,
    app: &str,
) -> Result<(), String> {
    let Some(auth) = peer_auth else {
        return Ok(());
    };
    let Some(uid) = peer_uid else {
        return Err("missing peer credentials".to_string());
    };
    auth(app, uid)
}

fn command_app(cmd: &Command) -> Option<&str> {
    match cmd {
        Command::EnqueueRun { app, .. }
        | Command::GetRunResult { app, .. }
        | Command::RegisterSchedules { app, .. }
        | Command::ClaimRun { app, .. }
        | Command::HeartbeatRun { app, .. }
        | Command::SaveStep { app, .. }
        | Command::CompleteRun { app, .. }
        | Command::CancelRun { app, .. }
        | Command::FailRun { app, .. }
        | Command::DeferRun { app, .. }
        | Command::WaitForEvent { app, .. }
        | Command::Signal { app, .. }
        | Command::ChannelPublish { app, .. }
        | Command::ChannelClose { app, .. }
        | Command::NetworkCall { app, .. } => Some(app),
        _ => None,
    }
}

fn write_channel(
    channel_publish: Option<&ChannelPublishFn>,
    operation: &str,
    request: ChannelPublishRequest<'_>,
) -> Response {
    let Some(publish) = channel_publish else {
        return Response::error(
            "channel publish is not configured on this internal socket".to_string(),
        );
    };
    match publish(request) {
        Ok(message) => Response::ok(message),
        Err(e) => e.context(format_args!("channel {operation} failed")).into(),
    }
}

fn handle_command(
    lookup: &AppLookup,
    channel_publish: Option<&ChannelPublishFn>,
    peer_auth: Option<&PeerAuthFn>,
    peer_uid: Option<u32>,
    network_call: Option<&NetworkCallFn>,
    cmd: Command,
) -> Response {
    let app = match command_app(&cmd) {
        Some(app) => app.to_string(),
        None => {
            return Response::coded_error(
                ErrorCode::InvalidRequest,
                format!(
                    "command {:?} not accepted on the internal socket",
                    std::mem::discriminant(&cmd)
                ),
            );
        }
    };
    if let Err(error) = authorize_peer(peer_auth, peer_uid, &app) {
        return Response::error(error);
    }

    if let Command::NetworkCall {
        address, request, ..
    } = cmd
    {
        let Some(route) = network_call else {
            return Response::coded_error(
                ErrorCode::Unavailable,
                "network primitives are unavailable",
            );
        };
        return match tako_core::network::resolve_address(&app, &address) {
            Ok((target, name)) => route(
                &app,
                &target,
                tako_core::network::NetworkInvocation { name, request },
            ),
            Err(error) => Response::coded_error(ErrorCode::InvalidRequest, error),
        };
    }

    // Channel publish and close take a different route — the channel store
    // lives outside the workflow manager, so we hand the request to the
    // caller-provided closure and skip the workflow app lookup.
    let cmd = match cmd {
        Command::ChannelPublish {
            channel,
            params,
            build,
            payload,
            ..
        } => {
            let request = ChannelPublishRequest {
                app: &app,
                channel: &channel,
                params: &params,
                build: build.as_deref(),
                write: ChannelWrite::Publish(payload),
            };
            return write_channel(channel_publish, "publish", request);
        }
        Command::ChannelClose {
            channel,
            params,
            build,
            lifecycle,
            ..
        } => {
            let request = ChannelPublishRequest {
                app: &app,
                channel: &channel,
                params: &params,
                build: build.as_deref(),
                write: ChannelWrite::Close { lifecycle },
            };
            return write_channel(channel_publish, "close", request);
        }
        cmd => cmd,
    };

    let Some(handlers) = lookup(&app) else {
        return Response::error(format!("unknown app: {app}"));
    };
    let AppHandlers {
        db,
        limiter,
        accepting_new_work,
        on_enqueue,
        health_check,
        on_claimed,
    } = handlers;

    match cmd {
        Command::EnqueueRun {
            name,
            payload,
            opts,
            ..
        } => {
            if !accepting_new_work.load(Ordering::SeqCst) {
                return Response::coded_error(
                    ErrorCode::Unavailable,
                    "workflow runtime is draining",
                );
            }
            if let Err(reason) = (health_check)() {
                return Response::coded_error(
                    ErrorCode::Unavailable,
                    format!("worker unhealthy: {reason}"),
                );
            }
            match db.enqueue(&name, &payload, &opts) {
                Ok(r) => {
                    (on_enqueue)();
                    Response::ok(r)
                }
                Err(e) => e.into_socket_error("enqueue").into(),
            }
        }
        Command::RegisterSchedules {
            build, schedules, ..
        } => {
            if !accepting_new_work.load(Ordering::SeqCst) {
                return Response::ok(serde_json::json!({ "count": 0 }));
            }
            match db.register_worker_schedules(build.as_deref(), &schedules) {
                Ok(count) => Response::ok(serde_json::json!({ "count": count })),
                Err(e) => e.into_socket_error("register_schedules").into(),
            }
        }
        Command::ClaimRun {
            worker_id,
            names,
            lease_ms,
            ..
        } => {
            if !accepting_new_work.load(Ordering::SeqCst) {
                return Response::ok(serde_json::Value::Null);
            }
            // At capacity, refresh ownership before refusing a claim: another
            // server may have reclaimed one of this process's expired leases.
            match limiter.claim(&db, &worker_id, &names, lease_ms) {
                Ok(Some(run)) => {
                    (on_claimed)();
                    Response::ok(run)
                }
                Ok(None) => Response::ok(serde_json::Value::Null),
                Err(e) => e.into_socket_error("claim").into(),
            }
        }
        Command::HeartbeatRun {
            id,
            worker_id,
            lease_ms,
            ..
        } => match db.heartbeat(&id, &worker_id, lease_ms) {
            Ok(()) => Response::ok(serde_json::json!({})),
            Err(e) => e.into_socket_error("heartbeat").into(),
        },
        Command::SaveStep {
            id,
            worker_id,
            step_name,
            result,
            ..
        } => match db.save_step(&id, &worker_id, &step_name, &result) {
            Ok(()) => Response::ok(serde_json::json!({})),
            Err(e) => e.into_socket_error("save_step").into(),
        },
        Command::GetRunResult { name, id, .. } => match db.result(&name, &id) {
            Ok(result) => Response::ok(result),
            Err(e) => e.into_socket_error("result").into(),
        },
        Command::CompleteRun {
            id,
            worker_id,
            result,
            ..
        } => match db.complete(&id, &worker_id, &result) {
            Ok(()) => {
                limiter.release_claim(&worker_id);
                Response::ok(serde_json::json!({}))
            }
            Err(e) => e.into_socket_error("complete").into(),
        },
        Command::CancelRun {
            id,
            worker_id,
            reason,
            ..
        } => match db.cancel(&id, &worker_id, reason.as_deref()) {
            Ok(()) => {
                limiter.release_claim(&worker_id);
                Response::ok(serde_json::json!({}))
            }
            Err(e) => e.into_socket_error("cancel").into(),
        },
        Command::FailRun {
            id,
            worker_id,
            error,
            next_run_at_ms,
            finalize,
            ..
        } => match db.fail(&id, &worker_id, &error, next_run_at_ms, finalize) {
            Ok(()) => {
                limiter.release_claim(&worker_id);
                Response::ok(serde_json::json!({}))
            }
            Err(e) => e.into_socket_error("fail").into(),
        },
        Command::DeferRun {
            id,
            worker_id,
            wake_at_ms,
            ..
        } => match db.defer(&id, &worker_id, wake_at_ms) {
            Ok(()) => {
                limiter.release_claim(&worker_id);
                Response::ok(serde_json::json!({}))
            }
            Err(e) => e.into_socket_error("defer").into(),
        },
        Command::WaitForEvent {
            id,
            worker_id,
            step_name,
            event_name,
            timeout_at_ms,
            ..
        } => match db.wait_for_event(&id, &worker_id, &step_name, &event_name, timeout_at_ms) {
            Ok(()) => {
                limiter.release_claim(&worker_id);
                Response::ok(serde_json::json!({}))
            }
            Err(e) => e.into_socket_error("wait_for_event").into(),
        },
        Command::Signal {
            event_name,
            payload,
            ..
        } => match db.signal(&event_name, &payload) {
            Ok(woken) => {
                if woken > 0 {
                    (on_enqueue)();
                }
                Response::ok(serde_json::json!({ "woken": woken }))
            }
            Err(e) => e.into_socket_error("signal").into(),
        },
        _ => unreachable!("command_app already filtered"),
    }
}

#[cfg(test)]
mod tests;

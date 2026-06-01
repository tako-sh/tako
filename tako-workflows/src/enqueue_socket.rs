//! Single shared Tako internal socket.
//!
//! One socket per tako-server instance handles every server-side SDK RPC:
//! workflow enqueue + worker RPCs, server-side channel `publish()`, and any
//! future server-routed command. Commands carry an `app` field; the
//! handler uses a lookup closure to find the app's `RunsDb` and supervisor
//! wake function (channel publish takes a separate route).
//!
//! Path convention: `{data_dir}/internal.sock` (symlink) →
//! `{data_dir}/internal-{pid}.sock` (the actual bound socket). Mirrors
//! the management-socket pattern so two tako-server processes can hand
//! over cleanly during upgrade.
//!
//! Auth: filesystem permissions only (`chmod 0660`, owned by the service
//! user/group so `tako-app` processes can connect).

use std::ffi::CString;
use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tako_core::{Command, Response};
use tako_socket::serve_jsonl_connection;
use tokio::net::UnixListener;
use tokio::sync::oneshot;

use super::cron::register_schedules;
use super::enqueue::RunsDb;

/// Callback fired whenever an enqueue or signal succeeds for a given app.
/// Used to wake the supervisor (so `workers = 0` scale-to-zero spawns).
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
    pub on_enqueue: OnEnqueue,
    pub health_check: HealthCheck,
    pub on_claimed: OnClaimed,
}

/// Lookup: given an app name, return the handlers for that app, or
/// `None` if the app isn't registered.
pub type AppLookup = Arc<dyn Fn(&str) -> Option<AppHandlers> + Send + Sync>;

/// Closure that appends a `ChannelPublishPayload` to an app's channel
/// store. Returns the stored message as a JSON value on success, or an
/// error string on failure. Abstracted this way so `tako-workflows`
/// doesn't need a direct dep on `tako-channels`.
pub type ChannelPublishFn =
    Arc<dyn Fn(&str, &str, serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync>;

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
    let join = tokio::spawn(run(listener, lookup, channel_publish, shutdown_rx));

    Ok(EnqueueSocketHandle {
        symlink_path,
        actual_path,
        shutdown_tx: Some(shutdown_tx),
        join: Some(join),
    })
}

fn configure_socket_permissions(path: &Path) -> std::io::Result<()> {
    if let Some(gid) = app_socket_gid_for_root(lookup_user_ids, is_root())? {
        chown_group(path, gid)?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
}

fn app_socket_gid_for_root(
    lookup: impl Fn(&str) -> std::io::Result<Option<(u32, u32)>>,
    is_root: bool,
) -> std::io::Result<Option<u32>> {
    if !is_root {
        return Ok(None);
    }
    Ok(lookup("tako-app")?.map(|(_uid, gid)| gid))
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and returns the effective user id.
    unsafe { libc::geteuid() == 0 }
}

fn lookup_user_ids(name: &str) -> std::io::Result<Option<(u32, u32)>> {
    let name = CString::new(name).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "user name contains interior NUL byte",
        )
    })?;
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buf = vec![0u8; passwd_buffer_size()];

    loop {
        // SAFETY: name is a valid C string; entry, result, and buf point to
        // writable storage for getpwnam_r.
        let rc = unsafe {
            libc::getpwnam_r(
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
            // SAFETY: getpwnam_r returned success and result points at entry.
            let entry = unsafe { entry.assume_init() };
            return Ok(Some((entry.pw_uid, entry.pw_gid)));
        }
        if rc == libc::ERANGE {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        return Err(std::io::Error::from_raw_os_error(rc));
    }
}

fn passwd_buffer_size() -> usize {
    // SAFETY: sysconf has no preconditions for _SC_GETPW_R_SIZE_MAX.
    let size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
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
                        tokio::spawn(async move {
                            let _ = serve_jsonl_connection(
                                stream,
                                move |cmd: Command| {
                                    let lookup = lookup.clone();
                                    let channel_publish = channel_publish.clone();
                                    async move { handle_command(&lookup, channel_publish.as_ref(), cmd) }
                                },
                                |e| Response::error(format!("invalid request: {e}")),
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
fn command_app(cmd: &Command) -> Option<&str> {
    match cmd {
        Command::EnqueueRun { app, .. }
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
        | Command::ChannelPublish { app, .. } => Some(app),
        _ => None,
    }
}

fn handle_command(
    lookup: &AppLookup,
    channel_publish: Option<&ChannelPublishFn>,
    cmd: Command,
) -> Response {
    let app = match command_app(&cmd) {
        Some(app) => app.to_string(),
        None => {
            return Response::error(format!(
                "command {:?} not accepted on the internal socket",
                std::mem::discriminant(&cmd)
            ));
        }
    };

    // Channel publish takes a different route — the channel store lives
    // outside the workflow manager, so we hand the payload to the
    // caller-provided closure and skip the workflow app lookup.
    if let Command::ChannelPublish {
        channel, payload, ..
    } = cmd
    {
        let Some(publish) = channel_publish else {
            return Response::error(
                "channel publish is not configured on this internal socket".to_string(),
            );
        };
        return match publish(&app, &channel, payload) {
            Ok(message) => Response::ok(message),
            Err(e) => Response::error(format!("channel publish failed: {e}")),
        };
    }

    let Some(handlers) = lookup(&app) else {
        return Response::error(format!("unknown app: {app}"));
    };
    let AppHandlers {
        db,
        limiter,
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
            if let Err(reason) = (health_check)() {
                return Response::error(format!("worker unhealthy: {reason}"));
            }
            match db.enqueue(&name, &payload, &opts) {
                Ok(r) => {
                    (on_enqueue)();
                    Response::ok(r)
                }
                Err(e) => Response::error(format!("enqueue failed: {e}")),
            }
        }
        Command::RegisterSchedules { schedules, .. } => match register_schedules(&db, &schedules) {
            Ok(()) => Response::ok(serde_json::json!({ "count": schedules.len() })),
            Err(e) => Response::error(format!("register_schedules failed: {e}")),
        },
        Command::ClaimRun {
            worker_id,
            names,
            lease_ms,
            ..
        } => {
            // Leaky bucket: refuse the claim without touching the queue
            // if the worker is already holding its max concurrent runs.
            // The SDK sees `Ok(None)` (same shape as "queue empty") and
            // backs off until an in-flight run terminates and a slot
            // frees up.
            if !limiter.try_acquire(&worker_id) {
                return Response::ok(serde_json::Value::Null);
            }
            match db.claim(&worker_id, &names, lease_ms) {
                Ok(Some(run)) => {
                    (on_claimed)();
                    Response::ok(run)
                }
                Ok(None) => {
                    // No work to do — give the slot back right away.
                    limiter.release(&worker_id);
                    Response::ok(serde_json::Value::Null)
                }
                Err(e) => {
                    limiter.release(&worker_id);
                    Response::error(format!("claim failed: {e}"))
                }
            }
        }
        Command::HeartbeatRun {
            id,
            worker_id,
            lease_ms,
            ..
        } => match db.heartbeat(&id, &worker_id, lease_ms) {
            Ok(()) => Response::ok(serde_json::json!({})),
            Err(e) => Response::error(format!("heartbeat failed: {e}")),
        },
        Command::SaveStep {
            id,
            worker_id,
            step_name,
            result,
            ..
        } => match db.save_step(&id, &worker_id, &step_name, &result) {
            Ok(()) => Response::ok(serde_json::json!({})),
            Err(e) => Response::error(format!("save_step failed: {e}")),
        },
        Command::CompleteRun { id, worker_id, .. } => match db.complete(&id, &worker_id) {
            Ok(()) => {
                limiter.release(&worker_id);
                Response::ok(serde_json::json!({}))
            }
            Err(e) => Response::error(format!("complete failed: {e}")),
        },
        Command::CancelRun {
            id,
            worker_id,
            reason,
            ..
        } => match db.cancel(&id, &worker_id, reason.as_deref()) {
            Ok(()) => {
                limiter.release(&worker_id);
                Response::ok(serde_json::json!({}))
            }
            Err(e) => Response::error(format!("cancel failed: {e}")),
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
                limiter.release(&worker_id);
                Response::ok(serde_json::json!({}))
            }
            Err(e) => Response::error(format!("fail failed: {e}")),
        },
        Command::DeferRun {
            id,
            worker_id,
            wake_at_ms,
            ..
        } => match db.defer(&id, &worker_id, wake_at_ms) {
            Ok(()) => {
                limiter.release(&worker_id);
                Response::ok(serde_json::json!({}))
            }
            Err(e) => Response::error(format!("defer failed: {e}")),
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
                limiter.release(&worker_id);
                Response::ok(serde_json::json!({}))
            }
            Err(e) => Response::error(format!("wait_for_event failed: {e}")),
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
            Err(e) => Response::error(format!("signal failed: {e}")),
        },
        _ => unreachable!("command_app already filtered"),
    }
}

#[cfg(test)]
mod tests;

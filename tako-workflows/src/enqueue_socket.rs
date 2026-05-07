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

use std::os::unix::fs::PermissionsExt;
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
    let _ = std::fs::set_permissions(&actual_path, std::fs::Permissions::from_mode(0o660));

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
mod tests {
    use super::*;
    use tako_core::EnqueueOpts;
    use tako_socket::{read_json_line, write_json_line};
    use tokio::io::BufReader;
    use tokio::net::UnixStream;

    fn test_limiter() -> Arc<crate::in_flight::InFlightLimiter> {
        Arc::new(crate::in_flight::InFlightLimiter::new(10))
    }

    fn lookup_for(map: std::collections::HashMap<String, Arc<RunsDb>>) -> AppLookup {
        Arc::new(move |app: &str| {
            map.get(app).map(|db| AppHandlers {
                db: db.clone(),
                limiter: test_limiter(),
                on_enqueue: Arc::new(|| {}),
                health_check: Arc::new(|| Ok(())),
                on_claimed: Arc::new(|| {}),
            })
        })
    }

    #[tokio::test]
    async fn internal_socket_is_group_accessible_for_app_processes() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("internal.sock");
        let handle = spawn(&sock, lookup_for(Default::default()), None).unwrap();
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;

        handle.shutdown().await;
        assert_eq!(mode, 0o660);
    }

    #[tokio::test]
    async fn enqueue_routes_by_app() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("internal.sock");
        let db_a = Arc::new(RunsDb::open_in_memory().unwrap());
        let db_b = Arc::new(RunsDb::open_in_memory().unwrap());

        let mut map = std::collections::HashMap::new();
        map.insert("a".to_string(), db_a.clone());
        map.insert("b".to_string(), db_b.clone());
        let handle = spawn(&sock, lookup_for(map), None).unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);

        let cmd = Command::EnqueueRun {
            app: "a".into(),
            name: "w".into(),
            payload: serde_json::json!({}),
            opts: EnqueueOpts::default(),
        };
        write_json_line(&mut w, &cmd).await.unwrap();
        let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
        assert!(resp.is_ok());

        // App 'a' should have one pending run; app 'b' should have zero.
        assert_eq!(db_a.pending_count().unwrap(), 1);
        assert_eq!(db_b.pending_count().unwrap(), 0);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn enqueue_rejects_when_health_check_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("internal.sock");
        let db = Arc::new(RunsDb::open_in_memory().unwrap());

        let db_for_lookup = db.clone();
        let lookup: AppLookup = Arc::new(move |_app: &str| {
            Some(AppHandlers {
                db: db_for_lookup.clone(),
                limiter: test_limiter(),
                on_enqueue: Arc::new(|| {}),
                health_check: Arc::new(|| Err("bootstrap crashed".to_string())),
                on_claimed: Arc::new(|| {}),
            })
        });
        let handle = spawn(&sock, lookup, None).unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);

        let cmd = Command::EnqueueRun {
            app: "a".into(),
            name: "w".into(),
            payload: serde_json::json!({}),
            opts: EnqueueOpts::default(),
        };
        write_json_line(&mut w, &cmd).await.unwrap();
        let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
        let err = resp.error_message().unwrap();
        assert!(
            err.contains("worker unhealthy") && err.contains("bootstrap crashed"),
            "expected unhealthy error with reason, got: {err}"
        );
        // DB must stay empty — enqueue short-circuited before db.enqueue().
        assert_eq!(db.pending_count().unwrap(), 0);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn claim_run_fires_on_claimed() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("internal.sock");
        let db = Arc::new(RunsDb::open_in_memory().unwrap());
        db.enqueue("w", &serde_json::json!({}), &EnqueueOpts::default())
            .unwrap();

        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = count.clone();
        let on_claimed: OnClaimed = Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let db_for_lookup = db.clone();
        let lookup: AppLookup = Arc::new(move |_app: &str| {
            Some(AppHandlers {
                db: db_for_lookup.clone(),
                limiter: test_limiter(),
                on_enqueue: Arc::new(|| {}),
                health_check: Arc::new(|| Ok(())),
                on_claimed: on_claimed.clone(),
            })
        });
        let handle = spawn(&sock, lookup, None).unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);

        let cmd = Command::ClaimRun {
            app: "a".into(),
            worker_id: "w1".into(),
            names: vec!["w".into()],
            lease_ms: 30_000,
        };
        write_json_line(&mut w, &cmd).await.unwrap();
        let _resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn claim_respects_in_flight_limiter() {
        use crate::in_flight::InFlightLimiter;

        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("internal.sock");
        let db = Arc::new(RunsDb::open_in_memory().unwrap());
        // Seed 3 runs so there's always something the DB could return.
        for _ in 0..3 {
            db.enqueue("w", &serde_json::json!({}), &EnqueueOpts::default())
                .unwrap();
        }
        // Cap at 2 concurrent in-flight for this worker.
        let limiter = Arc::new(InFlightLimiter::new(2));

        let db_for_lookup = db.clone();
        let limiter_for_lookup = limiter.clone();
        let lookup: AppLookup = Arc::new(move |_app: &str| {
            Some(AppHandlers {
                db: db_for_lookup.clone(),
                limiter: limiter_for_lookup.clone(),
                on_enqueue: Arc::new(|| {}),
                health_check: Arc::new(|| Ok(())),
                on_claimed: Arc::new(|| {}),
            })
        });
        let handle = spawn(&sock, lookup, None).unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);

        // First two claims succeed.
        for _ in 0..2 {
            let cmd = Command::ClaimRun {
                app: "a".into(),
                worker_id: "w1".into(),
                names: vec!["w".into()],
                lease_ms: 30_000,
            };
            write_json_line(&mut w, &cmd).await.unwrap();
            let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
            let v = resp.data().unwrap();
            assert!(!v.is_null(), "expected a run, got null: {resp:?}");
        }

        // Third claim: limiter refuses, DB row is NOT consumed, response
        // is a null payload (same shape as "queue empty").
        let cmd = Command::ClaimRun {
            app: "a".into(),
            worker_id: "w1".into(),
            names: vec!["w".into()],
            lease_ms: 30_000,
        };
        write_json_line(&mut w, &cmd).await.unwrap();
        let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
        assert!(resp.data().unwrap().is_null());
        // One run still pending — limiter refused BEFORE the DB claim fired.
        assert_eq!(db.pending_count().unwrap(), 1);

        // Complete one run → slot frees → next claim succeeds.
        limiter.release("w1");
        let cmd = Command::ClaimRun {
            app: "a".into(),
            worker_id: "w1".into(),
            names: vec!["w".into()],
            lease_ms: 30_000,
        };
        write_json_line(&mut w, &cmd).await.unwrap();
        let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
        assert!(!resp.data().unwrap().is_null());

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn claim_without_work_does_not_hold_a_slot() {
        use crate::in_flight::InFlightLimiter;

        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("internal.sock");
        let db = Arc::new(RunsDb::open_in_memory().unwrap());
        let limiter = Arc::new(InFlightLimiter::new(1));

        let db_for_lookup = db.clone();
        let limiter_for_lookup = limiter.clone();
        let lookup: AppLookup = Arc::new(move |_app: &str| {
            Some(AppHandlers {
                db: db_for_lookup.clone(),
                limiter: limiter_for_lookup.clone(),
                on_enqueue: Arc::new(|| {}),
                health_check: Arc::new(|| Ok(())),
                on_claimed: Arc::new(|| {}),
            })
        });
        let handle = spawn(&sock, lookup, None).unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);

        // Empty queue: claim reserves a slot, DB returns nothing, slot
        // must be released so the next claim isn't rejected by the cap.
        for _ in 0..5 {
            let cmd = Command::ClaimRun {
                app: "a".into(),
                worker_id: "w1".into(),
                names: vec!["w".into()],
                lease_ms: 30_000,
            };
            write_json_line(&mut w, &cmd).await.unwrap();
            let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
            assert!(resp.data().unwrap().is_null());
        }
        assert_eq!(limiter.count("w1"), 0);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn complete_releases_a_slot() {
        use crate::in_flight::InFlightLimiter;

        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("internal.sock");
        let db = Arc::new(RunsDb::open_in_memory().unwrap());
        db.enqueue("w", &serde_json::json!({}), &EnqueueOpts::default())
            .unwrap();
        let limiter = Arc::new(InFlightLimiter::new(1));

        let db_for_lookup = db.clone();
        let limiter_for_lookup = limiter.clone();
        let lookup: AppLookup = Arc::new(move |_app: &str| {
            Some(AppHandlers {
                db: db_for_lookup.clone(),
                limiter: limiter_for_lookup.clone(),
                on_enqueue: Arc::new(|| {}),
                health_check: Arc::new(|| Ok(())),
                on_claimed: Arc::new(|| {}),
            })
        });
        let handle = spawn(&sock, lookup, None).unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);

        // Claim.
        let cmd = Command::ClaimRun {
            app: "a".into(),
            worker_id: "w1".into(),
            names: vec!["w".into()],
            lease_ms: 30_000,
        };
        write_json_line(&mut w, &cmd).await.unwrap();
        let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
        let run = resp.data().unwrap();
        let id = run.get("id").and_then(|v| v.as_str()).unwrap().to_string();
        assert_eq!(limiter.count("w1"), 1);

        // Complete.
        let cmd = Command::CompleteRun {
            app: "a".into(),
            id,
            worker_id: "w1".into(),
        };
        write_json_line(&mut w, &cmd).await.unwrap();
        let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
        assert!(resp.is_ok());
        assert_eq!(limiter.count("w1"), 0);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_app_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("internal.sock");
        let handle = spawn(&sock, lookup_for(Default::default()), None).unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);

        let cmd = Command::EnqueueRun {
            app: "ghost".into(),
            name: "w".into(),
            payload: serde_json::json!({}),
            opts: EnqueueOpts::default(),
        };
        write_json_line(&mut w, &cmd).await.unwrap();
        let resp: Response = read_json_line(&mut r).await.unwrap().unwrap();
        assert!(resp.error_message().unwrap().contains("unknown app"));

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn on_enqueue_fires_for_signal_with_waiters_only() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("internal.sock");
        let db = Arc::new(RunsDb::open_in_memory().unwrap());
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let counter = count.clone();
        let on_enq: OnEnqueue = Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let db_for_lookup = db.clone();
        let lookup: AppLookup = Arc::new(move |_app: &str| {
            Some(AppHandlers {
                db: db_for_lookup.clone(),
                limiter: test_limiter(),
                on_enqueue: on_enq.clone(),
                health_check: Arc::new(|| Ok(())),
                on_claimed: Arc::new(|| {}),
            })
        });
        let handle = spawn(&sock, lookup, None).unwrap();

        // Signal with no waiters → should NOT fire on_enqueue.
        let stream = UnixStream::connect(&sock).await.unwrap();
        let (_r, mut w) = stream.into_split();
        let cmd = Command::Signal {
            app: "a".into(),
            event_name: "noop".into(),
            payload: serde_json::json!({}),
        };
        write_json_line(&mut w, &cmd).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);

        // Now seed a waiter and signal again — should fire.
        let r = db
            .enqueue("w", &serde_json::json!({}), &EnqueueOpts::default())
            .unwrap();
        let _ = db.claim("w1", &["w".into()], 30_000).unwrap();
        db.wait_for_event(&r.id, "w1", "step", "evt", None).unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (_r, mut w) = stream.into_split();
        let cmd = Command::Signal {
            app: "a".into(),
            event_name: "evt".into(),
            payload: serde_json::json!({"x": 1}),
        };
        write_json_line(&mut w, &cmd).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_removes_pid_socket_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("internal.sock");
        let handle = spawn(&sock, lookup_for(Default::default()), None).unwrap();
        assert!(sock.exists() || sock.is_symlink());
        handle.shutdown().await;
    }
}

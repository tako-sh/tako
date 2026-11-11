use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::control::State;
use crate::protocol::{DevEvent, Response};
use crate::state;
use tokio::io::AsyncBufReadExt;

const APP_STARTUP_NOTICE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
#[error("app startup cancelled")]
pub(crate) struct AppStartupCancelled;

// Interrupt readiness before taking the lifecycle lock, which startup holds.
pub(crate) fn cancel_app_startup(state: &Arc<Mutex<State>>, config_path: &str) {
    if let Some(cancel) = state.lock().unwrap().app_startups.get(config_path) {
        cancel.send_replace(());
    }
}

fn startup_cancellation(
    state: &Arc<Mutex<State>>,
    config_path: &str,
) -> tokio::sync::watch::Receiver<()> {
    state
        .lock()
        .unwrap()
        .app_startups
        .entry(config_path.to_string())
        .or_insert_with(|| tokio::sync::watch::channel(()).0)
        .subscribe()
}

pub(crate) async fn monitor_handoff_pid(
    state: Arc<Mutex<State>>,
    config_path: String,
    project_dir: String,
    pid: u32,
) {
    let sysinfo_pid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;

        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[sysinfo_pid]), false);
        if sys.process(sysinfo_pid).is_none() {
            let mut s = state.lock().unwrap();
            let still_current = s
                .apps
                .get(&config_path)
                .and_then(|a| a.pid)
                .map(|p| p == pid)
                .unwrap_or(false);
            if still_current {
                if let Some(app) = s.apps.get_mut(&config_path) {
                    app.is_idle = true;
                    app.pid = None;
                }
                let route_id = format!("reg:{}", config_path);
                s.routes.set_active(&route_id, false);
                state::remove_pid_file(&project_dir, &config_path);
            }
            tracing::info!(config_path = %config_path, project_dir = %project_dir, pid = pid, still_current = still_current, "handoff'd process exited");
            break;
        }
    }
}

pub(crate) fn broadcast_app_status(state: &Arc<Mutex<State>>, config_path: &str, status: &str) {
    let s = state.lock().unwrap();
    let app_name = s
        .apps
        .get(config_path)
        .map(|a| a.name.clone())
        .unwrap_or_default();
    s.events.broadcast(Response::Event {
        event: DevEvent::AppStatusChanged {
            config_path: config_path.to_string(),
            app_name,
            status: status.to_string(),
        },
    });
}

pub(crate) fn kill_app_process(pid: u32) {
    if pid == 0 {
        return;
    }
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

async fn kill_and_reap_app_process(
    child: &mut tokio::process::Child,
    pid: Option<u32>,
) -> Option<std::process::ExitStatus> {
    if let Some(pid) = pid {
        kill_app_process(pid);
    }

    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "failed to reap app process after startup ended");
            None
        }
        Err(_) => {
            tracing::warn!("timed out reaping app process after startup ended");
            None
        }
    }
}

pub(crate) fn broadcast_dev_event(state: &Arc<Mutex<State>>, event: DevEvent) {
    let s = state.lock().unwrap();
    // A spawn/readiness task may finish after unregistration. Do not recreate
    // cached lifecycle state (or notify clients) for an app that was removed.
    let config_path = match &event {
        DevEvent::AppLaunching { config_path, .. }
        | DevEvent::AppPid { config_path, .. }
        | DevEvent::AppStarted { config_path, .. }
        | DevEvent::AppReady { config_path, .. }
        | DevEvent::AppProcessExited { config_path, .. }
        | DevEvent::AppError { config_path, .. } => Some(config_path),
        _ => None,
    };
    if config_path.is_some_and(|path| !s.apps.contains_key(path)) {
        return;
    }
    s.events.broadcast(Response::Event { event });
}

pub(crate) fn app_name_for(state: &Arc<Mutex<State>>, config_path: &str) -> String {
    let s = state.lock().unwrap();
    s.apps
        .get(config_path)
        .map(|a| a.name.clone())
        .unwrap_or_default()
}

pub(crate) fn push_user_action(buf: &state::LogBuffer, kind: &str) {
    let payload = serde_json::json!({
        "ts": now_unix_millis(),
        "level": "info",
        "scope": "tako",
        "kind": kind,
    });
    buf.push(payload.to_string());
}

pub(crate) fn push_scoped_log(buf: &state::LogBuffer, level: &str, scope: &str, message: &str) {
    let payload = serde_json::json!({
        "ts": now_unix_millis(),
        "level": level.to_ascii_lowercase(),
        "scope": scope,
        "msg": message,
    });
    buf.push(payload.to_string());
}

fn now_unix_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) async fn spawn_and_monitor_app(
    state: Arc<Mutex<State>>,
    config_path: &str,
) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
    // Subscribe before waiting for the lock so a queued startup is cancellable too.
    let cancel_rx = startup_cancellation(&state, config_path);
    let lock = crate::compose::app_lock(&state, config_path);
    let _guard = lock.lock().await;
    spawn_and_monitor_app_locked(state, config_path, cancel_rx).await
}

// The caller holds this app's lifecycle lock through spawn and readiness.
async fn spawn_and_monitor_app_locked(
    state: Arc<Mutex<State>>,
    config_path: &str,
    mut cancel_rx: tokio::sync::watch::Receiver<()>,
) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
    if cancel_rx.has_changed().unwrap_or(true) {
        return Err(AppStartupCancelled.into());
    }
    if *state.lock().unwrap().shutdown_tx.borrow() {
        return Err("development server is shutting down".into());
    }
    // Another queued start may have completed while we waited for the lock.
    if let Some(app) = state.lock().unwrap().apps.get(config_path)
        && let Some(pid) = app.pid
    {
        return Ok(pid);
    }
    let (project_dir, app_clone, log_buffer, internal_socket) = {
        let s = state.lock().unwrap();
        let app = s.apps.get(config_path).ok_or("app not found")?;
        (
            app.project_dir.clone(),
            app.clone(),
            app.log_buffer.clone(),
            s.internal_socket.clone(),
        )
    };
    let app_name = app_clone.name.clone();

    broadcast_dev_event(
        &state,
        DevEvent::AppLaunching {
            config_path: config_path.to_string(),
            app_name: app_name.clone(),
        },
    );

    let route_id = format!("reg:{}", config_path);
    let (mut child, readiness_fd) =
        spawn_app(&project_dir, &app_clone, internal_socket.as_deref()).await?;
    let pid = child.id().ok_or("failed to get child PID")?;

    // Store the PID and clear the idle flag immediately so management commands
    // (restart, unregister) work right away without waiting for readiness.
    {
        let mut s = state.lock().unwrap();
        if let Some(app) = s.apps.get_mut(config_path) {
            app.pid = Some(pid);
            app.is_idle = false;
        }
    }

    state::write_pid_file(&project_dir, config_path, pid);
    broadcast_dev_event(
        &state,
        DevEvent::AppPid {
            config_path: config_path.to_string(),
            app_name: app_name.clone(),
            pid,
        },
    );

    // Dev commands may compile for minutes before the HTTP process even starts.
    // Keep waiting for readiness, but allow lifecycle commands and shutdown to
    // interrupt the wait and detect exits even if a descendant retains fd 4.
    let mut shutdown_rx = state.lock().unwrap().shutdown_tx.subscribe();
    let readiness: Result<(), Box<dyn std::error::Error + Send + Sync>> = tokio::select! {
        biased;
        _ = cancel_rx.changed() => Err(AppStartupCancelled.into()),
        _ = shutdown_rx.wait_for(|shutdown| *shutdown) => Err(AppStartupCancelled.into()),
        status = child.wait() => Err(match status {
            Ok(status) => format!("app exited before signaling readiness ({status})").into(),
            Err(error) => format!("failed to wait for app readiness: {error}").into(),
        }),
        result = activate_after_readiness(&state, config_path, &route_id, &app_clone, readiness_fd) => {
            result.map_err(Into::into)
        }
    };
    if let Err(e) = readiness {
        kill_and_reap_app_process(&mut child, Some(pid)).await;
        state::remove_pid_file(&project_dir, config_path);
        {
            let mut s = state.lock().unwrap();
            if let Some(app) = s.apps.get_mut(config_path) {
                app.pid = None;
                app.is_idle = true;
            }
            s.routes.set_active(&route_id, false);
        }
        return Err(e);
    }

    let state_for_monitor = state.clone();
    let config_for_monitor = config_path.to_string();
    let dir_for_monitor = project_dir.clone();
    let buf_for_monitor = log_buffer.clone();
    tokio::spawn(async move {
        let exit_status = child.wait().await;
        let code_str = exit_status
            .as_ref()
            .ok()
            .and_then(|s| s.code())
            .map(|c| format!("exit code {c}"))
            .unwrap_or_else(|| {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    if let Some(sig) = exit_status.as_ref().ok().and_then(|s| s.signal()) {
                        return format!("killed by signal {sig}");
                    }
                }
                "signal".to_string()
            });

        let (still_current, exit_app_name) = {
            let mut s = state_for_monitor.lock().unwrap();
            let current = s
                .apps
                .get(&config_for_monitor)
                .and_then(|a| a.pid)
                .map(|p| p == pid)
                .unwrap_or(false);

            let app_name = s
                .apps
                .get(&config_for_monitor)
                .map(|a| a.name.clone())
                .unwrap_or_default();

            if current {
                if let Some(app) = s.apps.get_mut(&config_for_monitor) {
                    app.is_idle = true;
                    app.pid = None;
                }
                let route_id = format!("reg:{}", config_for_monitor);
                s.routes.set_active(&route_id, false);
                state::remove_pid_file(&dir_for_monitor, &config_for_monitor);
                s.events.broadcast(Response::Event {
                    event: DevEvent::AppStatusChanged {
                        config_path: config_for_monitor.clone(),
                        app_name: app_name.clone(),
                        status: "idle".to_string(),
                    },
                });
            }
            (current, app_name)
        };

        if still_current {
            let msg = format!("App exited ({code_str})");
            push_scoped_log(&buf_for_monitor, "Fatal", "tako", &msg);
            broadcast_dev_event(
                &state_for_monitor,
                DevEvent::AppProcessExited {
                    config_path: config_for_monitor.clone(),
                    app_name: exit_app_name,
                    message: msg,
                },
            );

            tracing::info!(config_path = %config_for_monitor, pid = pid, "app process exited, marking idle");
        }
    });

    broadcast_dev_event(
        &state,
        DevEvent::AppStarted {
            config_path: config_path.to_string(),
            app_name,
        },
    );

    Ok(pid)
}

/// Create a Unix pipe for readiness signaling.
///
/// The write end is passed to the child as fd 4; the read end is kept by the parent
/// to receive the bound port written by the SDK.
#[cfg(unix)]
pub(crate) fn create_readiness_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe() is a standard POSIX call; fds is a valid 2-element array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe() returned 0, so both fds are valid.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

#[cfg(unix)]
fn create_bootstrap_pipe(
    token: &str,
    secrets: &std::collections::HashMap<String, String>,
    storages: &std::collections::HashMap<String, tako_core::StorageBinding>,
) -> std::io::Result<(OwnedFd, std::thread::JoinHandle<std::io::Result<()>>)> {
    let bytes = tako_core::bootstrap::envelope_bytes(token, secrets, storages);
    tako_spawn::create_payload_pipe(bytes)
}

/// Read the bound port from the app's readiness pipe (fd 4).
///
/// Returns the port when the SDK writes `{port}\n`, or `None` if the pipe
/// closes without a valid port (e.g. the process crashed before signaling).
#[cfg(unix)]
async fn wait_for_readiness(readiness_fd: OwnedFd) -> Option<u16> {
    // A blocking file read cannot be cancelled while another process holds the
    // pipe open. Tokio's pipe receiver releases the fd when this future is dropped.
    let pipe = tokio::net::unix::pipe::Receiver::from_owned_fd(readiness_fd).ok()?;
    let mut lines = tokio::io::BufReader::new(pipe).lines();
    match lines.next_line().await {
        Ok(Some(line)) => line.trim().parse::<u16>().ok().filter(|port| *port != 0),
        _ => None,
    }
}

/// Wait for the app to signal its bound port on fd 4, then activate the proxy route.
///
/// On non-Unix platforms (no readiness pipe), the route is activated immediately.
async fn activate_after_readiness(
    state: &Arc<Mutex<State>>,
    config_path: &str,
    route_id: &str,
    app: &state::RuntimeApp,
    #[cfg(unix)] readiness_fd: Option<OwnedFd>,
    #[cfg(not(unix))] _readiness_fd: Option<()>,
) -> Result<(), String> {
    #[cfg(unix)]
    if let Some(fd) = readiness_fd {
        let readiness = wait_for_readiness(fd);
        tokio::pin!(readiness);
        let started = tokio::time::Instant::now();
        let mut notices = tokio::time::interval_at(
            started + APP_STARTUP_NOTICE_INTERVAL,
            APP_STARTUP_NOTICE_INTERVAL,
        );
        notices.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let port = loop {
            tokio::select! {
                biased;
                port = &mut readiness => break port,
                _ = notices.tick() => {
                    push_scoped_log(
                        &app.log_buffer,
                        "info",
                        "tako",
                        &format!("App has not started after {}s; still waiting…", started.elapsed().as_secs()),
                    );
                }
            }
        };

        match port {
            Some(port) => {
                let mut s = state.lock().unwrap();
                if let Some(app) = s.apps.get_mut(config_path) {
                    app.upstream_port = port;
                }
                s.routes.activate_with_port(route_id, port);
                return Ok(());
            }
            None => {
                return Err(readiness_failure_message(app));
            }
        }
    }

    // Fallback for platforms without a readiness pipe: activate with existing port.
    state.lock().unwrap().routes.set_active(route_id, true);
    Ok(())
}

fn readiness_failure_message(app: &state::RuntimeApp) -> String {
    app.readiness_failure_hint
        .clone()
        .unwrap_or_else(|| "app closed fd 4 without signaling a valid readiness port".to_string())
}

/// Merge user-supplied env with Tako's runtime contract.
///
/// The contract (`PORT`, `HOST`, `TAKO_APP_NAME`, and — when available —
/// `TAKO_INTERNAL_SOCKET`) must always win over user env so a stray
/// `HOST=0.0.0.0` or `TAKO_APP_NAME=impostor` can't silently break the
/// proxy route or SDK RPC fanout.
pub(crate) fn build_spawn_env(
    app: &state::RuntimeApp,
    internal_socket: Option<&std::path::Path>,
) -> std::collections::HashMap<String, String> {
    let mut env = app.env.clone();
    tako_core::instance_env::TakoRuntimeEnv {
        app_name: &app.name,
        internal_socket,
    }
    .apply(&mut env);
    env
}

async fn spawn_app(
    project_dir: &str,
    app: &state::RuntimeApp,
    internal_socket: Option<&std::path::Path>,
) -> Result<(tokio::process::Child, Option<OwnedFd>), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(runtime) = &app.compose {
        return crate::compose::spawn(runtime, &app.log_buffer).await;
    }
    if app.command.is_empty() {
        return Err("app has empty command".into());
    }

    // Create the fd 4 readiness pipe: child writes its bound port, parent reads it.
    #[cfg(unix)]
    let readiness_pipe = create_readiness_pipe().ok();
    #[cfg(unix)]
    let write_raw: Option<std::os::fd::RawFd> = readiness_pipe.as_ref().map(|(_, w)| w.as_raw_fd());
    #[cfg(unix)]
    let (bootstrap_read, bootstrap_writer) =
        create_bootstrap_pipe(&app.bootstrap_token, &app.secrets, &app.storages)?;
    #[cfg(unix)]
    let bootstrap_raw: std::os::fd::RawFd = bootstrap_read.as_raw_fd();

    let mut cmd = tokio::process::Command::new(&app.command[0]);
    if app.command.len() > 1 {
        cmd.args(&app.command[1..]);
    }
    cmd.current_dir(project_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    #[cfg(unix)]
    cmd.process_group(0);

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(move || {
            #[cfg(target_os = "linux")]
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            // Expose the read end of the bootstrap pipe as fd 3 in the child.
            if bootstrap_raw != 3 {
                if libc::dup2(bootstrap_raw, 3) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(bootstrap_raw);
            }
            // Expose the write end of the readiness pipe as fd 4 in the child.
            if let Some(fd) = write_raw {
                if fd != 4 {
                    if libc::dup2(fd, 4) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(fd);
                }
            } else {
                // No readiness pipe; close fd 4 so the SDK fails silently.
                libc::close(4);
            }
            Ok(())
        });
    }

    let bin_dir = std::path::Path::new(project_dir).join("node_modules/.bin");
    if bin_dir.is_dir() {
        let current_path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{current_path}", bin_dir.display()));
    }

    for (k, v) in build_spawn_env(app, internal_socket) {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn()?;
    #[cfg(unix)]
    {
        drop(bootstrap_read);
        bootstrap_writer
            .join()
            .map_err(|_| std::io::Error::other("bootstrap writer thread panicked"))??;
    }

    // Keep the read end; dropping the write end (OwnedFd) closes it in the parent
    // so the parent gets EOF when the child closes its copy.
    #[cfg(unix)]
    let readiness_fd = readiness_pipe.map(|(read, _write)| read);
    #[cfg(not(unix))]
    let readiness_fd: Option<OwnedFd> = None;

    forward_child_pipes(&mut child, &app.log_buffer);

    Ok((child, readiness_fd))
}

pub(crate) fn forward_child_pipes(
    child: &mut tokio::process::Child,
    log_buffer: &state::LogBuffer,
) {
    if let Some(stdout) = child.stdout.take() {
        let buf = log_buffer.clone();
        tokio::spawn(async move {
            drain_pipe_to_buffer(stdout, buf, "info").await;
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let buf = log_buffer.clone();
        tokio::spawn(async move {
            drain_pipe_to_buffer(stderr, buf, "warn").await;
        });
    }
}

/// Forward subprocess output to the log buffer.
///
/// Lines that look like JSON objects (start with `{`) are forwarded as-is —
/// the SDK's structured logger emits them and the renderer parses them.
/// Anything else is wrapped as a plain scoped log at `default_level`
/// so raw `console.log` and crash dumps still surface.
async fn drain_pipe_to_buffer(
    pipe: impl tokio::io::AsyncRead + Unpin,
    buf: state::LogBuffer,
    default_level: &str,
) {
    let reader = tokio::io::BufReader::new(pipe);
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        forward_child_log_line(&buf, line, default_level, "app");
    }
}

/// Push a single child-process output line into a `LogBuffer`, passing
/// structured JSON (`{...}`) through verbatim and wrapping anything else
/// as a scoped log at `default_level`. Shared by the HTTP-app stdio drainer
/// and the worker-supervisor log sink.
pub(crate) fn forward_child_log_line(
    buf: &state::LogBuffer,
    line: String,
    default_level: &str,
    scope: &str,
) {
    if line.trim_start().starts_with('{') {
        buf.push(line);
    } else {
        let payload = serde_json::json!({
            "ts": now_unix_millis(),
            "level": default_level,
            "scope": scope,
            "msg": line,
        });
        buf.push(payload.to_string());
    }
}

pub(crate) async fn handle_wake_on_request(state: Arc<Mutex<State>>, host: String, path: String) {
    let route_id = {
        let s = state.lock().unwrap();
        let Some(target) = s.routes.lookup(&host, &path) else {
            return;
        };
        if target.active {
            return;
        }
        target.app_id
    };
    let Some(config_path) = route_id.strip_prefix("reg:") else {
        return;
    };
    wake_registration(state, config_path, Some((host, path))).await;
}

/// Wake an already-selected app without resolving its private calls through public routes.
pub(crate) async fn wake_registration(
    state: Arc<Mutex<State>>,
    config_path: &str,
    request_route: Option<(String, String)>,
) {
    let cancel_rx = startup_cancellation(&state, config_path);
    let lock = crate::compose::app_lock(&state, config_path);
    let _guard = lock.lock().await;
    let (app_name, log_buffer) = {
        let mut s = state.lock().unwrap();
        // Registration, restart, or another wake may have completed while we waited.
        if *s.shutdown_tx.borrow()
            || request_route.as_ref().is_some_and(|(host, path)| {
                !s.routes.lookup(host, path).is_some_and(|target| {
                    target.app_id == format!("reg:{config_path}") && !target.active
                })
            })
        {
            return;
        }
        let Some(app) = s.apps.get_mut(config_path) else {
            return;
        };
        if !app.is_idle {
            return;
        }
        app.is_idle = false;
        (app.name.clone(), app.log_buffer.clone())
    };

    tracing::info!(app_name = %app_name, "waking idle app on request");
    if let Err(error) = spawn_and_monitor_app_locked(state.clone(), config_path, cancel_rx).await {
        if let Some(app) = state.lock().unwrap().apps.get_mut(config_path) {
            app.is_idle = true;
        }
        if error.is::<AppStartupCancelled>() {
            return;
        }
        let message = error.to_string();
        push_scoped_log(&log_buffer, "Error", "tako", &message);
        broadcast_dev_event(
            &state,
            DevEvent::AppError {
                config_path: config_path.to_string(),
                app_name,
                message,
            },
        );
    }
}

pub(crate) fn kill_all_app_processes(state: &Arc<Mutex<State>>) {
    let s = state.lock().unwrap();
    for (config_path, app) in &s.apps {
        if let Some(pid) = app.pid
            && pid > 0
        {
            tracing::info!(app = %app.name, pid = pid, "killing app process group on shutdown");
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            state::remove_pid_file(&app.project_dir, config_path);
        }
    }
}

#[cfg(test)]
mod tests;

pub(crate) async fn stale_app_cleanup_loop(state: Arc<Mutex<State>>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    loop {
        ticker.tick().await;
        let removed = {
            let s = state.lock().unwrap();
            let mut removed =
                s.db.as_ref()
                    .and_then(|db| db.cleanup_stale().ok())
                    .unwrap_or_default();
            // Also retry Compose cleanup after a previous Docker failure.
            removed.extend(
                s.apps
                    .iter()
                    .filter(|(key, app)| {
                        app.compose.is_some() && !std::path::Path::new(key).exists()
                    })
                    .map(|(key, _)| key.clone()),
            );
            removed.sort();
            removed.dedup();
            removed
        };
        for config_path in removed {
            cancel_app_startup(&state, &config_path);
            let lock = crate::compose::app_lock(&state, &config_path);
            let _guard = lock.lock().await;
            if let Err(error) = crate::compose::stop(&state, &config_path).await {
                tracing::warn!(%error, "failed to stop stale Compose app; will retry");
                continue;
            }
            let mut s = state.lock().unwrap();
            s.events.forget_app(&config_path);
            if let Some(app) = s.apps.remove(&config_path) {
                if let Some(pid) = app.pid {
                    kill_app_process(pid);
                }
                state::remove_pid_file(&app.project_dir, &config_path);
            }
            s.routes.remove_app(&format!("reg:{config_path}"));
        }
    }
}

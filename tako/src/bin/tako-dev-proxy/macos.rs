use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio::sync::Mutex;

use crate::control::{CONTROL_SOCKET_PATH, ProxyCommand, ProxyResponse};

const HTTP_SOCKET_NAME: &str = "http";
const HTTPS_SOCKET_NAME: &str = "https";
const HTTP_UPSTREAM: &str = "127.0.0.1:47830";
const HTTPS_UPSTREAM: &str = "127.0.0.1:47831";
const LOOPBACK_ADDR: &str = "127.77.0.1";
const LOOPBACK_INTERFACE: &str = "lo0";
const DEV_PROXY_LABEL: &str = "sh.tako.dev-proxy";
const DEV_PROXY_PLIST_PATH: &str =
    "/Library/Application Support/Tako/launchd/sh.tako.dev-proxy.plist";
const IDLE_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);
const IDLE_TICK: Duration = Duration::from_secs(60);
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

unsafe extern "C" {
    fn launch_activate_socket(
        name: *const libc::c_char,
        fds: *mut *mut libc::c_int,
        cnt: *mut libc::size_t,
    ) -> libc::c_int;
}

#[derive(Clone)]
struct ProxyState {
    active_connections: Arc<AtomicUsize>,
    // This lock is only used for a best-effort idle timestamp and is never held
    // across an await point, so a synchronous mutex keeps the state simple here.
    last_activity: Arc<std::sync::Mutex<Instant>>,
}

impl ProxyState {
    fn new() -> Self {
        Self {
            active_connections: Arc::new(AtomicUsize::new(0)),
            last_activity: Arc::new(std::sync::Mutex::new(Instant::now())),
        }
    }

    fn connection_started(&self) -> ActiveConnectionGuard {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
        self.record_activity();
        ActiveConnectionGuard {
            state: self.clone(),
        }
    }

    fn record_activity(&self) {
        if let Ok(mut last_activity) = self.last_activity.lock() {
            *last_activity = Instant::now();
        }
    }

    fn should_exit_for_idle(&self) -> bool {
        let idle_for = self
            .last_activity
            .lock()
            .map(|instant| instant.elapsed())
            .unwrap_or_default();
        self.active_connections.load(Ordering::Relaxed) == 0 && idle_for >= IDLE_TIMEOUT
    }
}

struct ActiveConnectionGuard {
    state: ProxyState,
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.state
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);
        self.state.record_activity();
    }
}

struct LanState {
    enabled: bool,
    addr: Option<String>,
    handle: Option<tokio::task::JoinHandle<()>>,
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
}

impl LanState {
    fn new() -> Self {
        Self {
            enabled: false,
            addr: None,
            handle: None,
            shutdown_tx: None,
        }
    }
}

pub(crate) async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let state = ProxyState::new();
    let mut listeners = activated_listeners(HTTPS_SOCKET_NAME)?
        .into_iter()
        .map(|listener| (listener, HTTPS_UPSTREAM))
        .collect::<Vec<_>>();
    listeners.extend(
        activated_listeners(HTTP_SOCKET_NAME)?
            .into_iter()
            .map(|listener| (listener, HTTP_UPSTREAM)),
    );

    let (error_tx, mut error_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    for (listener, upstream) in listeners {
        let state = state.clone();
        let error_tx = error_tx.clone();
        tokio::spawn(async move {
            if let Err(error) = run_listener(listener, upstream, state).await {
                let _ = error_tx.send(error.to_string());
            }
        });
    }
    drop(error_tx);

    let lan_state = Arc::new(Mutex::new(LanState::new()));
    let proxy_state = state.clone();
    spawn_control_socket(lan_state, proxy_state);

    let mut idle_tick = tokio::time::interval(IDLE_TICK);
    loop {
        tokio::select! {
            // Treat either launchd socket as required local ingress. If one listener
            // fails, restart the whole helper into a clean state instead of running
            // half-alive with only HTTP or HTTPS working.
            Some(error) = error_rx.recv() => return Err(error.into()),
            _ = idle_tick.tick() => {
                if state.should_exit_for_idle() {
                    return Ok(());
                }
            }
        }
    }
}

fn spawn_control_socket(lan_state: Arc<Mutex<LanState>>, proxy_state: ProxyState) {
    tokio::spawn(async move {
        let _ = std::fs::remove_file(CONTROL_SOCKET_PATH);
        let listener = match UnixListener::bind(CONTROL_SOCKET_PATH) {
            Ok(l) => l,
            Err(_) => return,
        };
        // Allow non-root clients to connect
        let _ = std::fs::set_permissions(
            CONTROL_SOCKET_PATH,
            std::os::unix::fs::PermissionsExt::from_mode(0o666),
        );

        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let lan_state = lan_state.clone();
            let proxy_state = proxy_state.clone();
            tokio::spawn(handle_control_connection(stream, lan_state, proxy_state));
        }
    });
}

async fn handle_control_connection(
    stream: tokio::net::UnixStream,
    lan_state: Arc<Mutex<LanState>>,
    proxy_state: ProxyState,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let response = match serde_json::from_str::<ProxyCommand>(&line) {
            Ok(cmd) => handle_command(cmd, &lan_state, &proxy_state).await,
            Err(e) => ProxyResponse::Error {
                message: format!("invalid command: {e}"),
            },
        };
        let mut json = serde_json::to_string(&response).unwrap_or_default();
        json.push('\n');
        if writer.write_all(json.as_bytes()).await.is_err() {
            break;
        }
    }
}

async fn handle_command(
    cmd: ProxyCommand,
    lan_state: &Arc<Mutex<LanState>>,
    proxy_state: &ProxyState,
) -> ProxyResponse {
    match cmd {
        ProxyCommand::EnableLan { bind_addr } => {
            let addr = bind_addr.unwrap_or_else(|| "0.0.0.0".to_string());
            match enable_lan(lan_state, proxy_state, &addr).await {
                Ok(()) => ProxyResponse::LanEnabled { addr },
                Err(e) => ProxyResponse::Error {
                    message: e.to_string(),
                },
            }
        }
        ProxyCommand::DisableLan => {
            disable_lan(lan_state).await;
            ProxyResponse::LanDisabled
        }
        ProxyCommand::Status => {
            let state = lan_state.lock().await;
            ProxyResponse::Status {
                lan_enabled: state.enabled,
                lan_addr: state.addr.clone(),
            }
        }
    }
}

async fn enable_lan(
    lan_state: &Arc<Mutex<LanState>>,
    proxy_state: &ProxyState,
    bind_addr: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = lan_state.lock().await;
    let task_finished = state
        .handle
        .as_ref()
        .map(|handle| handle.is_finished())
        .unwrap_or(true);
    if super::should_keep_existing_lan_listener(
        state.enabled,
        state.addr.as_deref(),
        task_finished,
        bind_addr,
    ) {
        return Ok(());
    }

    // Clear the "enabled" flags up-front so a bind failure mid-rebind
    // leaves state consistent with reality (no listener → not enabled).
    // They're restored to `true` at the bottom only after both listeners
    // are successfully bound and the accept task is spawned.
    state.enabled = false;
    state.addr = None;

    // Tear down existing LAN listener if any
    if let Some(tx) = state.shutdown_tx.take() {
        let _ = tx.send(true);
    }
    if let Some(handle) = state.handle.take() {
        handle.abort();
        let _ = handle.await;
    }

    let https_addr = format!("{bind_addr}:443");
    let http_addr = format!("{bind_addr}:80");

    let https_listener = super::bind_lan_listener(&https_addr, "HTTPS").await?;
    let http_listener = super::bind_lan_listener(&http_addr, "HTTP").await?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let proxy_state_clone = proxy_state.clone();
    let handle = tokio::spawn(async move {
        let mut shutdown_rx_https = shutdown_rx.clone();
        let mut shutdown_rx_http = shutdown_rx;
        let ps_https = proxy_state_clone.clone();
        let ps_http = proxy_state_clone;

        let https_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = https_listener.accept() => {
                        if let Ok((stream, _)) = result {
                            ps_https.record_activity();
                            let ps = ps_https.clone();
                            tokio::spawn(async move {
                                proxy_connection(stream, HTTPS_UPSTREAM, ps).await;
                            });
                        }
                    }
                    _ = shutdown_rx_https.changed() => break,
                }
            }
        });

        let http_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = http_listener.accept() => {
                        if let Ok((stream, _)) = result {
                            ps_http.record_activity();
                            let ps = ps_http.clone();
                            tokio::spawn(async move {
                                proxy_connection(stream, HTTP_UPSTREAM, ps).await;
                            });
                        }
                    }
                    _ = shutdown_rx_http.changed() => break,
                }
            }
        });

        let _ = tokio::join!(https_task, http_task);
    });

    state.enabled = true;
    state.addr = Some(bind_addr.to_string());
    state.handle = Some(handle);
    state.shutdown_tx = Some(shutdown_tx);
    Ok(())
}

async fn disable_lan(lan_state: &Arc<Mutex<LanState>>) {
    let mut state = lan_state.lock().await;
    if let Some(tx) = state.shutdown_tx.take() {
        let _ = tx.send(true);
    }
    if let Some(handle) = state.handle.take() {
        handle.abort();
        let _ = handle.await;
    }
    state.enabled = false;
    state.addr = None;
}

pub(crate) fn bootstrap() -> Result<(), Box<dyn std::error::Error>> {
    ensure_running_as_root(unsafe { libc::geteuid() })?;
    ensure_loopback_alias()?;
    rebootstrap_proxy_launchd_job()?;
    Ok(())
}

async fn run_listener(
    listener: TcpListener,
    upstream: &'static str,
    state: ProxyState,
) -> Result<(), std::io::Error> {
    loop {
        let (stream, _) = listener.accept().await?;
        state.record_activity();
        let state = state.clone();
        tokio::spawn(async move {
            proxy_connection(stream, upstream, state).await;
        });
    }
}

async fn proxy_connection(stream: TcpStream, upstream: &'static str, state: ProxyState) {
    let _guard = state.connection_started();
    let Ok(Ok(mut upstream_stream)) =
        tokio::time::timeout(UPSTREAM_CONNECT_TIMEOUT, TcpStream::connect(upstream)).await
    else {
        return;
    };
    let mut downstream = stream;
    let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream_stream).await;
}

fn activated_listeners(name: &str) -> Result<Vec<TcpListener>, Box<dyn std::error::Error>> {
    let name = CString::new(name)?;
    let mut fds = std::ptr::null_mut();
    let mut count = 0usize;
    let rc = unsafe { launch_activate_socket(name.as_ptr(), &mut fds, &mut count) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc).into());
    }
    if fds.is_null() || count == 0 {
        return Err(format!(
            "launchd did not provide any sockets for {}",
            name.to_string_lossy()
        )
        .into());
    }

    let raw_fds = take_activated_socket_fds(fds, count);
    let mut listeners = Vec::with_capacity(raw_fds.len());
    for fd in raw_fds {
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let std_listener = std::net::TcpListener::from(owned);
        std_listener.set_nonblocking(true)?;
        listeners.push(TcpListener::from_std(std_listener)?);
    }
    Ok(listeners)
}

fn take_activated_socket_fds(fds: *mut libc::c_int, count: usize) -> Vec<libc::c_int> {
    unsafe {
        let slice = std::slice::from_raw_parts(fds, count);
        let raw_fds = slice.to_vec();
        libc::free(fds.cast());
        raw_fds
    }
}

fn ensure_loopback_alias() -> Result<(), Box<dyn std::error::Error>> {
    if loopback_alias_ready()? {
        return Ok(());
    }

    run_checked(
        Command::new("ifconfig").args(["lo0", "alias", LOOPBACK_ADDR, "up"]),
        "assigning Tako loopback alias",
    )
}

fn loopback_alias_ready() -> Result<bool, Box<dyn std::error::Error>> {
    let output = Command::new("ifconfig").arg(LOOPBACK_INTERFACE).output()?;
    if !output.status.success() {
        return Ok(false);
    }
    Ok(loopback_alias_present(
        &String::from_utf8_lossy(&output.stdout),
        LOOPBACK_ADDR,
    ))
}

fn loopback_alias_present(ifconfig_output: &str, ip: &str) -> bool {
    ifconfig_output.lines().any(|line| {
        let mut parts = line.split_whitespace();
        matches!(parts.next(), Some("inet")) && parts.next() == Some(ip)
    })
}

fn ensure_running_as_root(euid: u32) -> Result<(), Box<dyn std::error::Error>> {
    if euid == 0 {
        return Ok(());
    }
    Err("tako-dev-proxy bootstrap must run as root".into())
}

fn rebootstrap_proxy_launchd_job() -> Result<(), Box<dyn std::error::Error>> {
    let label = format!("system/{DEV_PROXY_LABEL}");
    let bootout = Command::new("launchctl")
        .args(["bootout", &label])
        .status()?;
    if !(bootout.success() || bootout.code() == Some(3)) {
        return Err("booting out dev proxy launchd service failed".into());
    }
    run_checked(
        Command::new("launchctl").args(["bootstrap", "system", DEV_PROXY_PLIST_PATH]),
        "bootstrapping dev proxy launchd service",
    )?;
    run_checked(
        Command::new("launchctl").args(["enable", &label]),
        "enabling dev proxy launchd service",
    )
}

fn run_checked(command: &mut Command, context: &str) -> Result<(), Box<dyn std::error::Error>> {
    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("exit status {}", output.status)
    };
    Err(format!("{context} failed: {detail}").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_names_map_to_expected_upstreams() {
        assert_eq!(HTTP_SOCKET_NAME, "http");
        assert_eq!(HTTPS_SOCKET_NAME, "https");
        assert_eq!(HTTP_UPSTREAM, "127.0.0.1:47830");
        assert_eq!(HTTPS_UPSTREAM, "127.0.0.1:47831");
    }

    #[test]
    fn idle_exit_requires_zero_connections_and_elapsed_timeout() {
        let state = ProxyState::new();
        assert!(!state.should_exit_for_idle());
        *state.last_activity.lock().expect("lock") =
            Instant::now() - IDLE_TIMEOUT - Duration::from_secs(1);
        assert!(state.should_exit_for_idle());
        state.active_connections.store(1, Ordering::Relaxed);
        assert!(!state.should_exit_for_idle());
    }

    #[test]
    fn loopback_alias_present_matches_assigned_ipv4_lines() {
        assert!(loopback_alias_present(
            "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST>\n\tinet 127.0.0.1 netmask 0xff000000\n\tinet 127.77.0.1 netmask 0xff000000 alias\n",
            "127.77.0.1",
        ));
        assert!(!loopback_alias_present(
            "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST>\n\tinet 127.0.0.1 netmask 0xff000000\n",
            "127.77.0.1",
        ));
    }

    #[test]
    fn run_checked_failure_includes_stderr() {
        let err = run_checked(
            Command::new("sh").args(["-c", "echo boom >&2; exit 7"]),
            "demo command",
        )
        .expect_err("expected failing command");

        let text = err.to_string();
        assert!(text.contains("demo command failed"));
        assert!(text.contains("boom"));
    }

    #[test]
    fn ensure_running_as_root_reports_clear_error_for_non_root() {
        let err = ensure_running_as_root(501).expect_err("non-root should fail");
        assert!(err.to_string().contains("must run as root"));
    }

    #[test]
    fn ensure_running_as_root_accepts_root() {
        ensure_running_as_root(0).expect("root should succeed");
    }

    #[test]
    fn reuses_existing_lan_listener_when_state_matches_request() {
        assert!(super::super::should_keep_existing_lan_listener(
            true,
            Some("0.0.0.0"),
            false,
            "0.0.0.0"
        ));
        assert!(!super::super::should_keep_existing_lan_listener(
            true,
            Some("0.0.0.0"),
            true,
            "0.0.0.0"
        ));
        assert!(!super::super::should_keep_existing_lan_listener(
            true,
            Some("127.0.0.1"),
            false,
            "0.0.0.0"
        ));
    }

    #[test]
    fn retries_only_for_addr_in_use() {
        assert!(super::super::should_retry_lan_bind(&std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "busy"
        )));
        assert!(!super::super::should_retry_lan_bind(&std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "nope"
        )));
    }
}

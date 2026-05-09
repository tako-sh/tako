use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio::sync::Mutex;

use crate::control::{CONTROL_SOCKET_PATH, ProxyCommand, ProxyResponse};

const HTTPS_UPSTREAM: &str = "127.0.0.1:47831";
const HTTP_UPSTREAM: &str = "127.0.0.1:47830";
const IDLE_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);
const IDLE_TICK: Duration = Duration::from_secs(60);
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct ProxyState {
    active_connections: Arc<AtomicUsize>,
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
    let lan_state = Arc::new(Mutex::new(LanState::new()));

    let _ = std::fs::remove_file(CONTROL_SOCKET_PATH);
    let listener = UnixListener::bind(CONTROL_SOCKET_PATH)?;
    let _ = std::fs::set_permissions(
        CONTROL_SOCKET_PATH,
        std::os::unix::fs::PermissionsExt::from_mode(0o666),
    );

    let mut idle_tick = tokio::time::interval(IDLE_TICK);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    state.record_activity();
                    let lan_state = lan_state.clone();
                    let proxy_state = state.clone();
                    tokio::spawn(handle_control_connection(stream, lan_state, proxy_state));
                }
            }
            _ = idle_tick.tick() => {
                if state.should_exit_for_idle() {
                    // Clean up control socket on exit
                    let _ = std::fs::remove_file(CONTROL_SOCKET_PATH);
                    return Ok(());
                }
            }
        }
    }
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
    let ps = proxy_state.clone();
    let handle = tokio::spawn(async move {
        let mut shutdown_https = shutdown_rx.clone();
        let mut shutdown_http = shutdown_rx;
        let ps_https = ps.clone();
        let ps_http = ps;

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
                    _ = shutdown_https.changed() => break,
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
                    _ = shutdown_http.changed() => break,
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

async fn proxy_connection(stream: TcpStream, upstream: &str, state: ProxyState) {
    let _guard = state.connection_started();
    let Ok(Ok(mut upstream_stream)) =
        tokio::time::timeout(UPSTREAM_CONNECT_TIMEOUT, TcpStream::connect(upstream)).await
    else {
        return;
    };
    let mut downstream = stream;
    let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream_stream).await;
}

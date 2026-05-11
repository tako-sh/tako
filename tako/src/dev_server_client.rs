use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

// Keep this above the daemon-side proxy bind wait window (~12s) so we can
// report daemon exit/log details instead of a generic connect timeout.
const DEV_SERVER_STARTUP_WAIT_ATTEMPTS: usize = 300;
const DEV_SERVER_STARTUP_WAIT_INTERVAL_MS: u64 = 50;
const DEV_SERVER_CONNECTION_CLOSED_MESSAGE: &str = "dev-server closed connection";

fn socket_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(crate::paths::tako_data_dir()?.join("dev-server.sock"))
}

fn dev_server_log_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(crate::paths::tako_data_dir()?.join("dev-server.log"))
}

fn open_dev_server_log(log_path: &std::path::Path) -> Result<std::fs::File, std::io::Error> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(log_path)
}

fn read_dev_server_log_tail(log_path: &std::path::Path, max_lines: usize) -> String {
    let Ok(contents) = std::fs::read_to_string(log_path) else {
        return String::new();
    };
    let lines: Vec<&str> = contents.lines().collect();
    let keep = lines.len().saturating_sub(max_lines);
    let tail = lines[keep..].join("\n");
    tail.trim().to_string()
}

fn format_dev_server_connect_error(
    log_path: &std::path::Path,
    status: Option<std::process::ExitStatus>,
) -> String {
    let tail = read_dev_server_log_tail(log_path, 40);
    let status_hint = status
        .map(|s| format!(" (daemon exited: {s})"))
        .unwrap_or_default();
    if tail.is_empty() {
        format!("could not connect to tako-dev-server{status_hint}")
    } else {
        format!("could not connect to tako-dev-server{status_hint}\nlast daemon log lines:\n{tail}")
    }
}

pub(crate) struct LineClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl LineClient {
    fn new(stream: UnixStream) -> Self {
        let (r, w) = stream.into_split();
        Self {
            reader: BufReader::new(r),
            writer: w,
        }
    }

    async fn send_line(&mut self, s: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.writer.write_all(s.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        Ok(())
    }

    async fn read_line(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let mut line = String::new();
        if self.reader.read_line(&mut line).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                DEV_SERVER_CONNECTION_CLOSED_MESSAGE,
            )
            .into());
        }
        Ok(line)
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ListedApp {
    pub app_name: String,
    pub variant: Option<String>,
    pub hosts: Vec<String>,
    pub upstream_port: u16,
    pub pid: Option<u32>,
}

pub async fn ensure_running(
    listen_addr: &str,
    dns_ip: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let log_path = dev_server_log_path().unwrap_or_else(|_| PathBuf::from("dev-server.log"));

    if let Ok(stream) = UnixStream::connect(&sock).await {
        let mut c = LineClient::new(stream);
        ping(&mut c).await?;
        return Ok(());
    }

    // If we can't connect to the daemon, we're about to spawn one. Avoid noisy
    // daemon stderr output by checking bind errors ourselves.
    if let Err(e) = std::net::TcpListener::bind(listen_addr) {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            return Err(format!("dev server listen {} is already in use", listen_addr).into());
        }
        return Err(format!("dev server listen {} is not available: {}", listen_addr, e).into());
    }

    let mut child = spawn_dev_server(listen_addr, dns_ip, &log_path)?;
    for _ in 0..DEV_SERVER_STARTUP_WAIT_ATTEMPTS {
        tokio::time::sleep(Duration::from_millis(DEV_SERVER_STARTUP_WAIT_INTERVAL_MS)).await;
        if let Ok(stream) = UnixStream::connect(&sock).await {
            let mut c = LineClient::new(stream);
            ping(&mut c).await?;
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(format_dev_server_connect_error(&log_path, Some(status)).into());
        }
    }

    if let Some(status) = child.try_wait()? {
        return Err(format_dev_server_connect_error(&log_path, Some(status)).into());
    }

    Err(format_dev_server_connect_error(&log_path, None).into())
}

fn spawn_dev_server(
    listen_addr: &str,
    dns_ip: &str,
    log_path: &std::path::Path,
) -> Result<std::process::Child, Box<dyn std::error::Error>> {
    use std::process::Stdio;

    let mut running_from_source_checkout = false;

    // Try repo-local target paths first when running from a source checkout.
    if let Ok(exe) = std::env::current_exe()
        && let Some(root) = crate::paths::repo_root_from_exe(&exe)
    {
        running_from_source_checkout = true;
        let candidates = repo_local_dev_server_candidates(&root);
        if repo_local_dev_server_build_needed(
            file_modified_time(&exe),
            file_modified_time(&candidates[0]),
        ) {
            let _ = maybe_build_repo_local_dev_server(&root);
        }

        for cand in candidates {
            if cand.exists() {
                let log_file = open_dev_server_log(log_path)?;
                let log_file_err = log_file.try_clone()?;
                let child = std::process::Command::new(cand)
                    .args(["--listen", listen_addr, "--dns-ip", dns_ip])
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(log_file))
                    .stderr(Stdio::from(log_file_err))
                    .spawn()?;
                return Ok(child);
            }
        }
    }

    // Fall back to PATH.
    let log_file = open_dev_server_log(log_path)?;
    let log_file_err = log_file.try_clone()?;
    match std::process::Command::new("tako-dev-server")
        .args(["--listen", listen_addr, "--dns-ip", dns_ip])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()
    {
        Ok(child) => Ok(child),
        Err(e) => {
            Err(format_missing_dev_server_spawn_error(running_from_source_checkout, &e).into())
        }
    }
}

fn format_missing_dev_server_spawn_error(
    running_from_source_checkout: bool,
    spawn_error: &std::io::Error,
) -> String {
    if running_from_source_checkout {
        return format!(
            "failed to spawn 'tako-dev-server' ({spawn_error}). If you're running from a source checkout, build it with: cargo build -p tako --bin tako-dev-server"
        );
    }

    format!(
        "failed to spawn 'tako-dev-server' ({spawn_error}). Reinstall Tako CLI and retry: curl -fsSL https://tako.sh/install.sh | sh"
    )
}

fn repo_local_dev_server_candidates(root: &std::path::Path) -> [PathBuf; 2] {
    [
        root.join("target").join("debug").join("tako-dev-server"),
        root.join("target").join("release").join("tako-dev-server"),
    ]
}

fn file_modified_time(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn repo_local_dev_server_build_needed(
    tako_modified: Option<SystemTime>,
    dev_server_modified: Option<SystemTime>,
) -> bool {
    match (tako_modified, dev_server_modified) {
        (_, None) => true,
        (Some(tako), Some(dev_server)) => dev_server < tako,
        (None, Some(_)) => false,
    }
}

fn maybe_build_repo_local_dev_server(root: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("cargo")
        .args(repo_local_dev_server_build_args())
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|_| ())
}

fn repo_local_dev_server_build_args() -> [&'static str; 5] {
    ["build", "-p", "tako", "--bin", "tako-dev-server"]
}

async fn ping(c: &mut LineClient) -> Result<(), Box<dyn std::error::Error>> {
    c.send_line(r#"{"type":"Ping"}"#).await?;
    let line = c.read_line().await?;
    if line.trim() == r#"{"type":"Pong"}"# {
        return Ok(());
    }
    Err(format!("unexpected response: {}", line).into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DevServerEvent {
    RequestStarted {
        host: String,
        path: String,
    },
    RequestFinished {
        host: String,
        path: String,
    },
    AppStatusChanged {
        config_path: String,
        app_name: String,
        status: String,
    },
    RestartRequested {
        config_path: String,
        app_name: String,
    },
    ClientConnected {
        config_path: String,
        app_name: String,
        client_id: u32,
    },
    ClientDisconnected {
        config_path: String,
        app_name: String,
        client_id: u32,
    },
    LanModeChanged {
        enabled: bool,
        lan_ip: Option<String>,
        ca_url: Option<String>,
    },
    AppLaunching {
        config_path: String,
        app_name: String,
    },
    AppPid {
        config_path: String,
        app_name: String,
        pid: u32,
    },
    AppStarted {
        config_path: String,
        app_name: String,
    },
    AppReady {
        config_path: String,
        app_name: String,
    },
    AppProcessExited {
        config_path: String,
        app_name: String,
        message: String,
    },
    AppError {
        config_path: String,
        app_name: String,
        message: String,
    },
}

fn parse_event_line(line: &str) -> Option<DevServerEvent> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(|t| t.as_str()) != Some("Event") {
        return None;
    }

    let event = value.get("event")?;
    match event.get("type").and_then(|t| t.as_str())? {
        "RequestStarted" => Some(DevServerEvent::RequestStarted {
            host: event.get("host")?.as_str()?.to_string(),
            path: event.get("path")?.as_str()?.to_string(),
        }),
        "RequestFinished" => Some(DevServerEvent::RequestFinished {
            host: event.get("host")?.as_str()?.to_string(),
            path: event.get("path")?.as_str()?.to_string(),
        }),
        "AppStatusChanged" => Some(DevServerEvent::AppStatusChanged {
            config_path: event.get("config_path")?.as_str()?.to_string(),
            app_name: event.get("app_name")?.as_str()?.to_string(),
            status: event.get("status")?.as_str()?.to_string(),
        }),
        "RestartRequested" => Some(DevServerEvent::RestartRequested {
            config_path: event.get("config_path")?.as_str()?.to_string(),
            app_name: event.get("app_name")?.as_str()?.to_string(),
        }),
        "ClientConnected" => Some(DevServerEvent::ClientConnected {
            config_path: event.get("config_path")?.as_str()?.to_string(),
            app_name: event.get("app_name")?.as_str()?.to_string(),
            client_id: event.get("client_id")?.as_u64()? as u32,
        }),
        "ClientDisconnected" => Some(DevServerEvent::ClientDisconnected {
            config_path: event.get("config_path")?.as_str()?.to_string(),
            app_name: event.get("app_name")?.as_str()?.to_string(),
            client_id: event.get("client_id")?.as_u64()? as u32,
        }),
        "LanModeChanged" => Some(DevServerEvent::LanModeChanged {
            enabled: event.get("enabled")?.as_bool()?,
            lan_ip: event
                .get("lan_ip")
                .and_then(|v| v.as_str())
                .map(String::from),
            ca_url: event
                .get("ca_url")
                .and_then(|v| v.as_str())
                .map(String::from),
        }),
        "AppLaunching" => Some(DevServerEvent::AppLaunching {
            config_path: event.get("config_path")?.as_str()?.to_string(),
            app_name: event.get("app_name")?.as_str()?.to_string(),
        }),
        "AppPid" => Some(DevServerEvent::AppPid {
            config_path: event.get("config_path")?.as_str()?.to_string(),
            app_name: event.get("app_name")?.as_str()?.to_string(),
            pid: event.get("pid")?.as_u64()? as u32,
        }),
        "AppStarted" => Some(DevServerEvent::AppStarted {
            config_path: event.get("config_path")?.as_str()?.to_string(),
            app_name: event.get("app_name")?.as_str()?.to_string(),
        }),
        "AppReady" => Some(DevServerEvent::AppReady {
            config_path: event.get("config_path")?.as_str()?.to_string(),
            app_name: event.get("app_name")?.as_str()?.to_string(),
        }),
        "AppProcessExited" => Some(DevServerEvent::AppProcessExited {
            config_path: event.get("config_path")?.as_str()?.to_string(),
            app_name: event.get("app_name")?.as_str()?.to_string(),
            message: event.get("message")?.as_str()?.to_string(),
        }),
        "AppError" => Some(DevServerEvent::AppError {
            config_path: event.get("config_path")?.as_str()?.to_string(),
            app_name: event.get("app_name")?.as_str()?.to_string(),
            message: event.get("message")?.as_str()?.to_string(),
        }),
        _ => None,
    }
}

pub async fn subscribe_events()
-> Result<tokio::sync::mpsc::UnboundedReceiver<DevServerEvent>, Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    c.send_line(r#"{"type":"SubscribeEvents"}"#).await?;

    // Wait for Subscribed.
    let line = c.read_line().await?;
    let v: serde_json::Value = serde_json::from_str(&line)?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("Subscribed") => {}
        Some("Error") => return Err(format!("dev-server error: {}", v).into()),
        _ => return Err(format!("unexpected response: {}", line).into()),
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let line = match c.read_line().await {
                Ok(l) => l,
                Err(_) => break,
            };
            if line.trim().is_empty() {
                continue;
            }
            let Some(ev) = parse_event_line(&line) else {
                continue;
            };
            let _ = tx.send(ev);
        }
    });

    Ok(rx)
}

pub async fn list_apps() -> Result<Vec<ListedApp>, Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    c.send_line(r#"{"type":"ListApps"}"#).await?;
    let line = c.read_line().await?;
    let v: serde_json::Value = serde_json::from_str(&line)?;
    if v.get("type").and_then(|t| t.as_str()) != Some("Apps") {
        return Err(format!("unexpected response: {}", line).into());
    }
    let apps = v
        .get("apps")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(apps
        .into_iter()
        .filter_map(|a| {
            let hosts = a
                .get("hosts")
                .and_then(|h| h.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(ListedApp {
                app_name: a.get("app_name")?.as_str()?.to_string(),
                variant: a
                    .get("variant")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                hosts,
                upstream_port: a.get("upstream_port")?.as_u64()? as u16,
                pid: a.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32),
            })
        })
        .collect())
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RegisteredAppInfo {
    pub config_path: String,
    pub project_dir: String,
    pub app_name: String,
    pub variant: Option<String>,
    pub hosts: Vec<String>,
    pub upstream_port: u16,
    pub status: String,
    pub pid: Option<u32>,
    pub client_pid: Option<u32>,
}

pub struct RegisterAppRequest<'a> {
    pub config_path: &'a str,
    pub project_dir: &'a str,
    pub app_name: &'a str,
    pub variant: Option<&'a str>,
    pub hosts: &'a [String],
    pub command: &'a [String],
    pub env: &'a std::collections::HashMap<String, String>,
    pub readiness_failure_hint: Option<&'a str>,
    pub worker_command: Option<&'a [String]>,
}

pub async fn register_app(
    args: RegisterAppRequest<'_>,
) -> Result<String, Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    let mut req = serde_json::json!({
        "type": "RegisterApp",
        "config_path": args.config_path,
        "project_dir": args.project_dir,
        "app_name": args.app_name,
        "hosts": args.hosts,
        "command": args.command,
        "env": args.env,
        "client_pid": std::process::id(),
    });
    if let Some(v) = args.variant {
        req["variant"] = serde_json::Value::String(v.to_string());
    }
    if let Some(hint) = args.readiness_failure_hint {
        req["readiness_failure_hint"] = serde_json::Value::String(hint.to_string());
    }
    if let Some(wc) = args.worker_command {
        req["worker_command"] = serde_json::json!(wc);
    }
    c.send_line(&req.to_string()).await?;
    let line = c.read_line().await?;
    let v: serde_json::Value = serde_json::from_str(&line)?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("AppRegistered") => Ok(v
            .get("url")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()),
        Some("Error") => Err(format!("dev-server error: {}", v).into()),
        _ => Err(format!("unexpected response: {}", line).into()),
    }
}

pub async fn unregister_app(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    let req = serde_json::json!({
        "type": "UnregisterApp",
        "config_path": config_path,
    });
    c.send_line(&req.to_string()).await?;
    let line = c.read_line().await?;
    let v: serde_json::Value = serde_json::from_str(&line)?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("AppUnregistered") => Ok(()),
        Some("Error") => Err(format!("dev-server error: {}", v).into()),
        _ => Err(format!("unexpected response: {}", line).into()),
    }
}

pub async fn restart_app(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    let req = serde_json::json!({
        "type": "RestartApp",
        "config_path": config_path,
    });
    c.send_line(&req.to_string()).await?;
    let line = c.read_line().await?;
    let v: serde_json::Value = serde_json::from_str(&line)?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("AppRestarting") => Ok(()),
        Some("Error") => Err(format!("dev-server error: {}", v).into()),
        _ => Err(format!("unexpected response: {}", line).into()),
    }
}

pub async fn toggle_lan(
    enabled: bool,
) -> Result<(bool, Option<String>, Option<String>), Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    let req = serde_json::json!({
        "type": "ToggleLan",
        "enabled": enabled,
    });
    c.send_line(&req.to_string()).await?;
    let line = c.read_line().await?;
    let v: serde_json::Value = serde_json::from_str(&line)?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("LanToggled") => {
            let enabled = v.get("enabled").and_then(|b| b.as_bool()).unwrap_or(false);
            let lan_ip = v.get("lan_ip").and_then(|s| s.as_str()).map(String::from);
            let ca_url = v.get("ca_url").and_then(|s| s.as_str()).map(String::from);
            Ok((enabled, lan_ip, ca_url))
        }
        Some("Error") => Err(v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
            .to_string()
            .into()),
        _ => Err(format!("unexpected response: {}", line).into()),
    }
}

/// Connect to the daemon as a named client. The returned `LineClient` must be
/// kept alive for the duration of the CLI session — dropping it triggers a
/// `ClientDisconnected` event in the daemon.
pub async fn connect_client(
    config_path: &str,
    client_id: u32,
) -> Result<LineClient, Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    let req = serde_json::json!({
        "type": "ConnectClient",
        "config_path": config_path,
        "client_id": client_id,
    });
    c.send_line(&req.to_string()).await?;
    let line = c.read_line().await?;
    let v: serde_json::Value = serde_json::from_str(&line)?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("Error") => Err(format!("dev-server error: {}", v).into()),
        _ => Ok(c),
    }
}

pub async fn list_registered_apps() -> Result<Vec<RegisteredAppInfo>, Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    c.send_line(r#"{"type":"ListRegisteredApps"}"#).await?;
    let line = c.read_line().await?;
    let v: serde_json::Value = serde_json::from_str(&line)?;
    if v.get("type").and_then(|t| t.as_str()) != Some("RegisteredApps") {
        return Err(format!("unexpected response: {}", line).into());
    }
    let apps = v
        .get("apps")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(apps
        .into_iter()
        .filter_map(|a| {
            let hosts = a
                .get("hosts")
                .and_then(|h| h.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(RegisteredAppInfo {
                config_path: a.get("config_path")?.as_str()?.to_string(),
                project_dir: a.get("project_dir")?.as_str()?.to_string(),
                app_name: a.get("app_name")?.as_str()?.to_string(),
                variant: a
                    .get("variant")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                hosts,
                upstream_port: a.get("upstream_port")?.as_u64()? as u16,
                status: a
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("stopped")
                    .to_string(),
                pid: a.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32),
                client_pid: a
                    .get("client_pid")
                    .and_then(|p| p.as_u64())
                    .map(|p| p as u32),
            })
        })
        .collect())
}

pub async fn info() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    c.send_line(r#"{"type":"Info"}"#).await?;
    let line = c.read_line().await?;
    Ok(serde_json::from_str(&line)?)
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum LogStreamEntry {
    Entry { id: u64, line: String },
    Truncated,
}

pub async fn subscribe_logs(
    config_path: &str,
    after: Option<u64>,
) -> Result<tokio::sync::mpsc::UnboundedReceiver<LogStreamEntry>, Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    let req = serde_json::json!({
        "type": "SubscribeLogs",
        "config_path": config_path,
        "after": after,
    });
    c.send_line(&req.to_string()).await?;

    // Wait for LogsSubscribed.
    let line = c.read_line().await?;
    let v: serde_json::Value = serde_json::from_str(&line)?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("LogsSubscribed") => {}
        Some("Error") => return Err(format!("dev-server error: {}", v).into()),
        _ => return Err(format!("unexpected response: {}", line).into()),
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let line = match c.read_line().await {
                Ok(l) => l,
                Err(_) => break,
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            match v.get("type").and_then(|t| t.as_str()) {
                Some("LogEntry") => {
                    let id = v.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
                    let entry_line = v
                        .get("line")
                        .and_then(|l| l.as_str())
                        .unwrap_or("")
                        .to_string();
                    if tx
                        .send(LogStreamEntry::Entry {
                            id,
                            line: entry_line,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Some("LogsTruncated") if tx.send(LogStreamEntry::Truncated).is_err() => {
                    break;
                }
                _ => {}
            }
        }
    });

    Ok(rx)
}

pub async fn stop_server() -> Result<(), Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    c.send_line(r#"{"type":"StopServer"}"#).await?;
    let line = c.read_line().await?;
    let v: serde_json::Value = serde_json::from_str(&line)?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("Stopping") => Ok(()),
        Some("Error") => Err(format!("dev-server error: {}", v).into()),
        _ => Err(format!("unexpected response: {}", line).into()),
    }
}

#[cfg(test)]
mod tests;

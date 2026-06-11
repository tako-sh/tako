use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::net::UnixStream;

use super::connection::{LineClient, ping, socket_path};

// Keep this above the daemon-side proxy bind wait window (~12s) so we can
// report daemon exit/log details instead of a generic connect timeout.
pub(super) const DEV_SERVER_STARTUP_WAIT_ATTEMPTS: usize = 300;
pub(super) const DEV_SERVER_STARTUP_WAIT_INTERVAL_MS: u64 = 50;

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

fn dev_server_log_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(crate::paths::tako_data_dir()?.join("dev-server.log"))
}

fn open_dev_server_log(log_path: &Path) -> Result<std::fs::File, std::io::Error> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(log_path)
}

pub(super) fn read_dev_server_log_tail(log_path: &Path, max_lines: usize) -> String {
    let Ok(contents) = std::fs::read_to_string(log_path) else {
        return String::new();
    };
    let lines: Vec<&str> = contents.lines().collect();
    let keep = lines.len().saturating_sub(max_lines);
    let tail = lines[keep..].join("\n");
    tail.trim().to_string()
}

pub(super) fn format_dev_server_connect_error(
    log_path: &Path,
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

fn spawn_dev_server(
    listen_addr: &str,
    dns_ip: &str,
    log_path: &Path,
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

pub(super) fn format_missing_dev_server_spawn_error(
    running_from_source_checkout: bool,
    spawn_error: &std::io::Error,
) -> String {
    if running_from_source_checkout {
        return format!(
            "failed to spawn 'tako-dev-server' ({spawn_error}). If you're running from a source checkout, build it with: cargo build -p tako-cli --bin tako-dev-server"
        );
    }

    format!(
        "failed to spawn 'tako-dev-server' ({spawn_error}). Reinstall Tako CLI and retry: curl -fsSL https://tako.sh/install.sh | sh"
    )
}

pub(super) fn repo_local_dev_server_candidates(root: &Path) -> [PathBuf; 2] {
    [
        root.join("target").join("debug").join("tako-dev-server"),
        root.join("target").join("release").join("tako-dev-server"),
    ]
}

fn file_modified_time(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

pub(super) fn repo_local_dev_server_build_needed(
    tako_modified: Option<SystemTime>,
    dev_server_modified: Option<SystemTime>,
) -> bool {
    match (tako_modified, dev_server_modified) {
        (_, None) => true,
        (Some(tako), Some(dev_server)) => dev_server < tako,
        (None, Some(_)) => false,
    }
}

fn maybe_build_repo_local_dev_server(root: &Path) -> std::io::Result<()> {
    std::process::Command::new("cargo")
        .args(repo_local_dev_server_build_args())
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|_| ())
}

pub(super) fn repo_local_dev_server_build_args() -> [&'static str; 5] {
    ["build", "-p", "tako", "--bin", "tako-dev-server"]
}

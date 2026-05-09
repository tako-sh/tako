mod control;

use std::time::Duration;

fn should_keep_existing_lan_listener(
    enabled: bool,
    current_addr: Option<&str>,
    task_finished: bool,
    requested_addr: &str,
) -> bool {
    enabled && current_addr == Some(requested_addr) && !task_finished
}

fn should_retry_lan_bind(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::AddrInUse
}

async fn bind_lan_listener(addr: &str, label: &str) -> Result<tokio::net::TcpListener, String> {
    // Give the bind up to ~3s to succeed. Real-world LAN teardown + rebind can
    // race with the just-shut-down listener dropping its socket under load,
    // and the client already shows "Starting LAN mode…" during this window
    // so the delay is explained.
    const MAX_ATTEMPTS: usize = 30;
    const RETRY_DELAY: Duration = Duration::from_millis(100);

    let mut last_error = None;
    for attempt in 0..MAX_ATTEMPTS {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(error) if should_retry_lan_bind(&error) && attempt + 1 < MAX_ATTEMPTS => {
                last_error = Some(error);
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(error) => {
                return Err(format!("failed to bind LAN {label} on {addr}: {error}"));
            }
        }
    }

    let error = last_error.expect("retry loop should capture the last bind error");
    Err(format!("failed to bind LAN {label} on {addr}: {error}"))
}

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if matches!(std::env::args().nth(1).as_deref(), Some("bootstrap")) {
        macos::bootstrap()
    } else {
        macos::run().await
    }
}

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run().await
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("tako-dev-proxy is only supported on macOS and Linux".into())
}

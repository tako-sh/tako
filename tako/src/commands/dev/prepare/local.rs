//! Cross-platform local dev helpers: HTTPS connectivity probe, error detection.

use std::time::Duration;

#[cfg(test)]
use tokio::time::timeout;

pub(crate) async fn localhost_https_host_reachable_via_ip(
    host: &str,
    connect_ip: std::net::Ipv4Addr,
    port: u16,
    timeout_ms: u64,
) -> Result<(), String> {
    if !connect_ip.is_loopback() {
        return Err("local HTTPS probe requires a loopback address".to_string());
    }
    let mut base_url = format!("https://{host}");
    if port != 443 {
        base_url.push(':');
        base_url.push_str(&port.to_string());
    }
    base_url.push('/');

    let addr = std::net::SocketAddr::from((connect_ip, port));
    // Skip TLS verification — the probe checks connectivity (proxy + dev
    // server responding), not certificate validity. The browser does its own
    // chain verification against the system trust store.
    // CodeQL[rust/disabled-certificate-check]: loopback connectivity probe; redirects and proxies are disabled.
    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_millis(timeout_ms))
        .timeout(Duration::from_millis(timeout_ms))
        .resolve(host, addr)
        .build()
    {
        Ok(client) => client,
        Err(e) => return Err(format!("failed to build HTTPS probe client: {e}")),
    };

    client
        .get(base_url)
        .send()
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub(crate) async fn wait_for_https_host_reachable_via_ip(
    host: &str,
    connect_ip: std::net::Ipv4Addr,
    port: u16,
    attempts: usize,
    timeout_ms: u64,
    retry_delay_ms: u64,
) -> Result<(), String> {
    let mut last_error = "probe did not return a response".to_string();
    for attempt in 0..attempts {
        match localhost_https_host_reachable_via_ip(host, connect_ip, port, timeout_ms).await {
            Ok(()) => return Ok(()),
            Err(e) => last_error = e,
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(Duration::from_millis(retry_delay_ms)).await;
        }
    }
    Err(last_error)
}

pub(crate) fn local_https_probe_host(primary_host: &str) -> &str {
    primary_host
}

#[cfg(test)]
pub(crate) async fn tcp_probe(addr: (&str, u16), timeout_ms: u64) -> bool {
    timeout(
        std::time::Duration::from_millis(timeout_ms),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

pub(crate) fn is_dev_server_unavailable_error_message(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("connection refused")
        || normalized.contains("no such file or directory")
        || normalized.contains("operation not permitted")
        || normalized.contains("permission denied")
}

use tokio::net::UnixStream;

use super::connection::{LineClient, socket_path};

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

pub async fn enable_tunnel(config_path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let (enabled, url, _) = toggle_tunnel(config_path, true).await?;
    url.filter(|url| enabled && !url.is_empty())
        .ok_or_else(|| "dev server did not return a public tunnel URL".into())
}

pub async fn toggle_tunnel(
    config_path: &str,
    enabled: bool,
) -> Result<(bool, Option<String>, Option<u64>), Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    let req = serde_json::json!({
        "type": "ToggleTunnel",
        "config_path": config_path,
        "enabled": enabled,
    });
    c.send_line(&req.to_string()).await?;
    let line = c.read_line().await?;
    let v: serde_json::Value = serde_json::from_str(&line)?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("TunnelToggled") => {
            let enabled = v.get("enabled").and_then(|b| b.as_bool()).unwrap_or(false);
            let url = v.get("url").and_then(|s| s.as_str()).map(String::from);
            let expires_at = v.get("expires_at").and_then(|value| value.as_u64());
            Ok((enabled, url, expires_at))
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

pub async fn info() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    c.send_line(r#"{"type":"Info"}"#).await?;
    let line = c.read_line().await?;
    Ok(serde_json::from_str(&line)?)
}

pub async fn stop_server() -> Result<(), Box<dyn std::error::Error>> {
    request_stop(UnixStream::connect(socket_path()?).await?).await
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DevServerStop {
    NotNeeded,
    Stopped,
    StillStopping,
}

pub(crate) async fn stop_server_if_running() -> Result<DevServerStop, Box<dyn std::error::Error>> {
    use std::time::Duration;
    let sock = socket_path()?;
    if !tokio::time::timeout(Duration::from_secs(60), request_stop_if_running(&sock))
        .await
        .map_err(|_| "timed out requesting local dev server shutdown")??
    {
        return Ok(DevServerStop::NotNeeded);
    }
    Ok(wait_for_shutdown(&sock).await?)
}

async fn request_stop_if_running(
    sock: &std::path::Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    use std::io::ErrorKind;
    let stream = match UnixStream::connect(sock).await {
        Ok(stream) => stream,
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::NotFound
                    | ErrorKind::ConnectionRefused
                    | ErrorKind::NotConnected
                    | ErrorKind::ConnectionReset
            ) =>
        {
            return Ok(false);
        }
        Err(e) => return Err(e.into()),
    };
    // Older daemons can exit before flushing Stopping. Socket removal follows app cleanup.
    if let Err(e) = request_stop(stream).await {
        if e.downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == ErrorKind::UnexpectedEof)
            && !sock.try_exists()?
        {
            return Ok(true);
        }
        return Err(e);
    }
    Ok(true)
}

async fn wait_for_shutdown(sock: &std::path::Path) -> std::io::Result<DevServerStop> {
    use std::time::Duration;
    use tokio::time::Instant;
    let deadline = Instant::now() + Duration::from_secs(60);
    while sock.try_exists()? {
        // The daemon acknowledged the request. Sequential Compose shutdown can
        // take minutes; leaving it to finish is a successful upgrade.
        if Instant::now() >= deadline {
            return Ok(DevServerStop::StillStopping);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(DevServerStop::Stopped)
}

async fn request_stop(stream: UnixStream) -> Result<(), Box<dyn std::error::Error>> {
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
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn acknowledged_slow_shutdown_is_still_in_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("dev-server.sock");
        let _listener = tokio::net::UnixListener::bind(&sock).unwrap();
        assert_eq!(
            wait_for_shutdown(&sock).await.unwrap(),
            DevServerStop::StillStopping
        );
    }
}

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

pub async fn info() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await?;
    let mut c = LineClient::new(stream);
    c.send_line(r#"{"type":"Info"}"#).await?;
    let line = c.read_line().await?;
    Ok(serde_json::from_str(&line)?)
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

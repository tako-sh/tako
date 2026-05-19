use tokio::net::UnixStream;

use super::connection::{LineClient, socket_path};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ListedApp {
    pub app_name: String,
    pub variant: Option<String>,
    pub hosts: Vec<String>,
    pub upstream_port: u16,
    pub pid: Option<u32>,
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
    pub images: &'a tako_images::ImagesConfig,
    pub storages: &'a std::collections::HashMap<String, tako_core::StorageBinding>,
    pub readiness_failure_hint: Option<&'a str>,
    pub worker_command: Option<&'a [String]>,
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
            let hosts = json_string_array(&a, "hosts");
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
            let hosts = json_string_array(&a, "hosts");
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
        "images": args.images,
        "storages": args.storages,
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

/// Connect to the daemon as a named client. The returned `LineClient` must be
/// kept alive for the duration of the CLI session - dropping it triggers a
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

fn json_string_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|h| h.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

use tokio::net::UnixStream;

use super::connection::{LineClient, socket_path};

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

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum LogStreamEntry {
    Entry { id: u64, line: String },
    Truncated,
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

pub(super) fn parse_event_line(line: &str) -> Option<DevServerEvent> {
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

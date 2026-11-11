use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::sync::mpsc;

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
    TunnelModeChanged {
        config_path: String,
        app_name: String,
        enabled: bool,
        url: Option<String>,
        expires_at: Option<u64>,
        close_reason: Option<TunnelCloseReason>,
    },
    TunnelConnectionChanged {
        config_path: String,
        app_name: String,
        connected: bool,
        url: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelCloseReason {
    User,
    Timeout,
    Shutdown,
    ConnectionClosed,
    ConnectionError,
    LimitExceeded,
    Replaced,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum LogStreamEntry {
    Entry { id: u64, line: String },
    Truncated,
}

pub async fn subscribe_events(
    config_path: &str,
) -> Result<mpsc::Receiver<DevServerEvent>, Box<dyn std::error::Error>> {
    let sock = socket_path()?;
    subscribe_events_at(&sock, config_path)
        .await
        .map_err(Into::into)
}

async fn connect_events(sock: &Path, config_path: Option<&str>) -> Result<LineClient, String> {
    let stream = UnixStream::connect(sock).await.map_err(|e| e.to_string())?;
    let mut c = LineClient::new(stream);
    let request = serde_json::json!({"type": "SubscribeEvents", "config_path": config_path});
    c.send_line(&request.to_string())
        .await
        .map_err(|e| e.to_string())?;

    // Wait for Subscribed.
    let line = c.read_line().await.map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("Subscribed") => Ok(c),
        Some("Error") => Err(format!("dev-server error: {}", v)),
        _ => Err(format!("unexpected response: {}", line)),
    }
}

const EVENT_RECONNECT_TIMEOUT: Duration = Duration::from_secs(5);

async fn subscribe_events_at(
    sock: &Path,
    config_path: &str,
) -> Result<mpsc::Receiver<DevServerEvent>, String> {
    // The owning CLI subscribes before registration. Only reconnects request a
    // snapshot, since an absent registration then means the app was stopped.
    let mut c = tokio::time::timeout(EVENT_RECONNECT_TIMEOUT, connect_events(sock, None))
        .await
        .map_err(|_| "dev-server event subscription timed out".to_string())??;
    let sock = sock.to_owned();
    let config_path = config_path.to_string();
    let (tx, rx) = mpsc::channel(512);
    tokio::spawn(async move {
        loop {
            loop {
                let line = tokio::select! {
                    _ = tx.closed() => return,
                    line = c.read_line() => match line {
                        Ok(line) => line,
                        Err(_) => break,
                    },
                };
                if let Some(event) = parse_event_line(&line)
                    && tx.send(event).await.is_err()
                {
                    return;
                }
            }
            // Overflow closes only the subscription, not the app. Keep the
            // public receiver alive while reconnecting and reconciling state.
            // A dead/unresponsive daemon still closes it within a finite time.
            let reconnect = async {
                let mut delay = Duration::from_millis(100);
                loop {
                    tokio::time::sleep(delay).await;
                    if let Ok(client) = connect_events(&sock, Some(&config_path)).await {
                        return client;
                    }
                    delay = (delay * 2).min(Duration::from_secs(1));
                }
            };
            c = tokio::select! {
                _ = tx.closed() => return,
                result = tokio::time::timeout(EVENT_RECONNECT_TIMEOUT, reconnect) => match result {
                    Ok(client) => client,
                    Err(_) => return,
                },
            };
        }
    });

    Ok(rx)
}

pub async fn subscribe_logs(
    config_path: &str,
    after: Option<u64>,
) -> Result<tokio::sync::mpsc::Receiver<LogStreamEntry>, Box<dyn std::error::Error>> {
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

    let (tx, rx) = tokio::sync::mpsc::channel(512);
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
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Some("LogsTruncated") if tx.send(LogStreamEntry::Truncated).await.is_err() => {
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
        "TunnelModeChanged" => Some(DevServerEvent::TunnelModeChanged {
            config_path: event.get("config_path")?.as_str()?.to_string(),
            app_name: event.get("app_name")?.as_str()?.to_string(),
            enabled: event.get("enabled")?.as_bool()?,
            url: event.get("url").and_then(|v| v.as_str()).map(String::from),
            expires_at: event.get("expires_at").and_then(|v| v.as_u64()),
            close_reason: event
                .get("close_reason")
                .and_then(|v| v.as_str())
                .and_then(parse_tunnel_close_reason),
        }),
        "TunnelConnectionChanged" => Some(DevServerEvent::TunnelConnectionChanged {
            config_path: event.get("config_path")?.as_str()?.to_string(),
            app_name: event.get("app_name")?.as_str()?.to_string(),
            connected: event.get("connected")?.as_bool()?,
            url: event.get("url")?.as_str()?.to_string(),
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

fn parse_tunnel_close_reason(value: &str) -> Option<TunnelCloseReason> {
    match value {
        "user" => Some(TunnelCloseReason::User),
        "timeout" => Some(TunnelCloseReason::Timeout),
        "shutdown" => Some(TunnelCloseReason::Shutdown),
        "connection_closed" => Some(TunnelCloseReason::ConnectionClosed),
        "connection_error" => Some(TunnelCloseReason::ConnectionError),
        "limit_exceeded" => Some(TunnelCloseReason::LimitExceeded),
        "replaced" => Some(TunnelCloseReason::Replaced),
        _ => None,
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use tokio::net::UnixListener;
    use tokio::time::timeout;

    async fn accept_subscription(listener: &UnixListener, recovering: bool) -> LineClient {
        let (stream, _) = listener.accept().await.unwrap();
        let mut server = LineClient::new(stream);
        let request: serde_json::Value =
            serde_json::from_str(&server.read_line().await.unwrap()).unwrap();
        assert_eq!(request["type"], "SubscribeEvents");
        assert_eq!(
            request["config_path"],
            if recovering {
                serde_json::json!("/app/tako.toml")
            } else {
                serde_json::Value::Null
            }
        );
        server.send_line(r#"{"type":"Subscribed"}"#).await.unwrap();
        server
    }

    #[tokio::test]
    async fn reconnect_preserves_receiver_and_delivers_snapshot_then_live_events() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("events.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let mut first = accept_subscription(&listener, false).await;
            first.send_line(r#"{"type":"Event","event":{"type":"AppPid","config_path":"/app/tako.toml","app_name":"app","pid":10}}"#).await.unwrap();
            drop(first);
            let mut second = accept_subscription(&listener, true).await;
            // Reconcile an exit and disabled tunnel missed during the gap.
            for line in [
                r#"{"type":"Event","event":{"type":"AppProcessExited","config_path":"/app/tako.toml","app_name":"app","message":"App exited (exit code 42)"}}"#,
                r#"{"type":"Event","event":{"type":"TunnelModeChanged","config_path":"/app/tako.toml","app_name":"app","enabled":false}}"#,
                r#"{"type":"Event","event":{"type":"AppPid","config_path":"/app/tako.toml","app_name":"app","pid":20}}"#,
            ] {
                second.send_line(line).await.unwrap();
            }
            // Dropping the consumer must close the idle socket and task.
            assert!(
                timeout(Duration::from_secs(1), second.read_line())
                    .await
                    .unwrap()
                    .is_err()
            );
        });
        let mut events = subscribe_events_at(&sock, "/app/tako.toml").await.unwrap();
        let mut received = Vec::new();
        for _ in 0..4 {
            received.push(
                timeout(Duration::from_secs(2), events.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(matches!(
            received[0],
            DevServerEvent::AppPid { pid: 10, .. }
        ));
        assert!(
            matches!(&received[1], DevServerEvent::AppProcessExited { message, .. } if message.contains("42"))
        );
        assert!(matches!(
            received[2],
            DevServerEvent::TunnelModeChanged { enabled: false, .. }
        ));
        assert!(matches!(
            received[3],
            DevServerEvent::AppPid { pid: 20, .. }
        ));
        drop(events);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn missed_stop_is_delivered_after_reconnect() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("events.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            drop(accept_subscription(&listener, false).await);
            let mut second = accept_subscription(&listener, true).await;
            second.send_line(r#"{"type":"Event","event":{"type":"AppStatusChanged","config_path":"/app/tako.toml","app_name":"","status":"stopped"}}"#).await.unwrap();
            assert!(
                timeout(Duration::from_secs(1), second.read_line())
                    .await
                    .unwrap()
                    .is_err()
            );
        });
        let mut events = subscribe_events_at(&sock, "/app/tako.toml").await.unwrap();
        let event = timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(event, DevServerEvent::AppStatusChanged { status, .. } if status == "stopped")
        );
        drop(events);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn unavailable_daemon_eventually_closes_receiver() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("events.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            drop(accept_subscription(&listener, false).await);
            drop(listener);
        });
        let mut events = subscribe_events_at(&sock, "/app/tako.toml").await.unwrap();
        server.await.unwrap();
        assert!(
            timeout(
                EVENT_RECONNECT_TIMEOUT + Duration::from_secs(1),
                events.recv()
            )
            .await
            .unwrap()
            .is_none()
        );
    }
}

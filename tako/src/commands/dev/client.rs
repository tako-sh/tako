use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio::sync::watch;

#[cfg(test)]
use tokio::time::timeout;

use super::runner::bootstrap_dev_events;
use super::{
    DevEvent, ScopedLog, TunnelCloseReason, infer_preset_name_from_ref, load_dev_tako_toml, output,
    resolve_dev_preset_ref,
};

#[derive(Debug, Clone)]
pub(super) struct ConnectedDevClient {
    pub(super) config_key: String,
    pub(super) config_path: PathBuf,
    pub(super) project_dir: PathBuf,
    pub(super) url: String,
    pub(super) pid: Option<u32>,
    pub(super) tunnel_enabled: bool,
    pub(super) tunnel_url: Option<String>,
}

pub(super) fn parse_log_line(line: &str) -> Option<ScopedLog> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(log) = serde_json::from_str::<ScopedLog>(trimmed) {
        return Some(log);
    }

    Some(ScopedLog::info("app", trimmed.to_string()))
}

pub(super) fn host_and_port_from_url(url: &str) -> Option<(String, u16)> {
    let no_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host_port = no_scheme.split('/').next().unwrap_or("");
    if host_port.is_empty() {
        return None;
    }

    if let Some((host, port)) = host_port.rsplit_once(':')
        && let Ok(port) = port.parse::<u16>()
    {
        return Some((host.to_string(), port));
    }

    Some((host_port.to_string(), 443))
}

pub(super) async fn run_connected_dev_client(
    app_name: &str,
    interactive: bool,
    tunnel: bool,
    session: ConnectedDevClient,
    display_hosts: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut public_port = host_and_port_from_url(&session.url)
        .map(|(_, p)| p)
        .unwrap_or(443);
    let mut lan_enabled = false;
    let mut tunnel_enabled = session.tunnel_enabled;

    if let Ok(info) = crate::dev_server_client::info().await {
        public_port = info
            .get("info")
            .and_then(|i| i.get("port"))
            .and_then(|p| p.as_u64())
            .map(|p| p as u16)
            .unwrap_or(public_port);
        lan_enabled = info
            .get("info")
            .and_then(|i| i.get("lan_enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }

    let my_client_id = std::process::id();

    let (log_tx, log_rx) = mpsc::channel::<ScopedLog>(1000);
    let (event_tx, event_rx) = mpsc::channel::<DevEvent>(32);

    for event in bootstrap_dev_events("running", session.pid) {
        let _ = event_tx.send(event).await;
    }
    let (control_tx, mut control_rx) = mpsc::channel::<output::ControlCmd>(32);
    let (stop_tx, stop_rx) = watch::channel(false);

    {
        let event_tx = event_tx.clone();
        let stop_tx = stop_tx.clone();
        let config_key = session.config_key.clone();
        let sid = my_client_id;
        tokio::spawn(async move {
            let mut got_stop = false;

            let connected = async {
                let ev_rx = crate::dev_server_client::subscribe_events().await.ok()?;
                Some(ev_rx)
            };

            if let Some(mut ev_rx) = connected.await {
                let _client_conn = crate::dev_server_client::connect_client(&config_key, sid)
                    .await
                    .ok();

                while let Some(ev) = ev_rx.recv().await {
                    use crate::dev_server_client::DevServerEvent;
                    match ev {
                        DevServerEvent::AppStatusChanged {
                            ref config_path,
                            ref status,
                            ..
                        } if config_path == &config_key && status == "stopped" => {
                            got_stop = true;
                            break;
                        }
                        DevServerEvent::ClientConnected {
                            ref config_path,
                            client_id,
                            ..
                        } if config_path == &config_key => {
                            let _ = event_tx
                                .send(DevEvent::ClientConnected {
                                    is_self: client_id == sid,
                                    client_id,
                                })
                                .await;
                        }
                        DevServerEvent::ClientDisconnected {
                            ref config_path,
                            client_id,
                            ..
                        } if config_path == &config_key => {
                            let _ = event_tx
                                .send(DevEvent::ClientDisconnected { client_id })
                                .await;
                        }
                        DevServerEvent::AppLaunching {
                            ref config_path, ..
                        } if config_path == &config_key => {
                            let _ = event_tx.send(DevEvent::AppLaunching).await;
                        }
                        DevServerEvent::AppStarted {
                            ref config_path, ..
                        } if config_path == &config_key => {
                            let _ = event_tx.send(DevEvent::AppStarted).await;
                        }
                        DevServerEvent::AppReady {
                            ref config_path, ..
                        } if config_path == &config_key => {
                            let _ = event_tx.send(DevEvent::AppReady).await;
                        }
                        DevServerEvent::AppPid {
                            ref config_path,
                            pid,
                            ..
                        } if config_path == &config_key => {
                            let _ = event_tx.send(DevEvent::AppPid(pid)).await;
                        }
                        DevServerEvent::AppProcessExited {
                            ref config_path,
                            ref message,
                            ..
                        } if config_path == &config_key => {
                            let _ = event_tx
                                .send(DevEvent::AppProcessExited(message.clone()))
                                .await;
                        }
                        DevServerEvent::AppError {
                            ref config_path,
                            ref message,
                            ..
                        } if config_path == &config_key => {
                            let _ = event_tx.send(DevEvent::AppError(message.clone())).await;
                        }
                        DevServerEvent::LanModeChanged {
                            enabled,
                            ref lan_ip,
                            ref ca_url,
                        } => {
                            let _ = event_tx
                                .send(DevEvent::LanModeChanged {
                                    enabled,
                                    lan_ip: lan_ip.clone(),
                                    ca_url: ca_url.clone(),
                                })
                                .await;
                        }
                        DevServerEvent::TunnelModeChanged {
                            ref config_path,
                            enabled,
                            ref url,
                            expires_at,
                            close_reason,
                            ..
                        } if config_path == &config_key => {
                            let _ = event_tx
                                .send(DevEvent::TunnelModeChanged {
                                    enabled,
                                    url: url.clone(),
                                    expires_at,
                                    close_reason: close_reason.map(TunnelCloseReason::from),
                                })
                                .await;
                        }
                        DevServerEvent::TunnelConnectionChanged {
                            ref config_path,
                            connected,
                            ref url,
                            ..
                        } if config_path == &config_key => {
                            let _ = event_tx
                                .send(DevEvent::TunnelConnectionChanged {
                                    connected,
                                    url: url.clone(),
                                })
                                .await;
                        }
                        _ => {}
                    }
                }
            }

            let _ = stop_tx.send(true);
            let msg = if got_stop {
                "stopped by another client".to_string()
            } else {
                "disconnected from dev server".to_string()
            };
            let _ = event_tx.send(DevEvent::ExitWithMessage(msg)).await;
        });
    }

    {
        let log_tx = log_tx.clone();
        let config_key_for_logs = session.config_key.clone();
        tokio::spawn(async move {
            let Ok(mut rx) =
                crate::dev_server_client::subscribe_logs(&config_key_for_logs, None).await
            else {
                return;
            };
            while let Some(entry) = rx.recv().await {
                match entry {
                    crate::dev_server_client::LogStreamEntry::Entry { line, .. } => {
                        if let Some(log) = parse_log_line(&line) {
                            let _ = log_tx.send(log).await;
                        }
                    }
                    crate::dev_server_client::LogStreamEntry::Truncated => {
                        let _ = log_tx
                            .send(ScopedLog::info("tako", "earlier logs trimmed"))
                            .await;
                    }
                }
            }
        });
    }

    {
        let log_tx = log_tx.clone();
        let event_tx = event_tx.clone();
        let stop_tx = stop_tx.clone();
        let config_key = session.config_key.clone();
        tokio::spawn(async move {
            while let Some(cmd) = control_rx.recv().await {
                match cmd {
                    output::ControlCmd::Restart => {
                        let result = crate::dev_server_client::restart_app(&config_key)
                            .await
                            .map_err(|e| e.to_string());
                        if let Err(msg) = result {
                            let _ = log_tx
                                .send(ScopedLog::error("tako", format!("Restart failed: {}", msg)))
                                .await;
                        }
                    }
                    output::ControlCmd::Terminate => {
                        let _ = crate::dev_server_client::unregister_app(&config_key)
                            .await
                            .map_err(|e| e.to_string());
                        let _ = stop_tx.send(true);
                        break;
                    }
                    output::ControlCmd::ToggleLan => {
                        let target = !lan_enabled;
                        if target {
                            let _ = event_tx.send(DevEvent::LanStarting).await;
                        }
                        let result = crate::dev_server_client::toggle_lan(target)
                            .await
                            .map_err(|e| e.to_string());
                        match result {
                            Ok((enabled, _, _)) => {
                                lan_enabled = enabled;
                            }
                            Err(msg) => {
                                if target {
                                    let _ = event_tx.send(DevEvent::LanFailed).await;
                                }
                                let _ = log_tx
                                    .send(ScopedLog::error(
                                        "tako",
                                        format!("LAN toggle failed: {}", msg),
                                    ))
                                    .await;
                            }
                        }
                    }
                    output::ControlCmd::ToggleTunnel => {
                        let current =
                            crate::dev_server_client::registered_tunnel_enabled(&config_key)
                                .await
                                .unwrap_or(tunnel_enabled);
                        let target = !current;
                        if target {
                            let _ = event_tx.send(DevEvent::TunnelStarting).await;
                        }
                        let result = crate::dev_server_client::toggle_tunnel(&config_key, target)
                            .await
                            .map_err(|e| e.to_string());
                        match result {
                            Ok((enabled, _, _)) => {
                                tunnel_enabled = enabled;
                            }
                            Err(msg) => {
                                if target {
                                    let _ = event_tx.send(DevEvent::TunnelFailed).await;
                                }
                                let _ = log_tx
                                    .send(ScopedLog::error(
                                        "tako",
                                        format!("Tunnel toggle failed: {}", msg),
                                    ))
                                    .await;
                            }
                        }
                    }
                }
            }
        });
    }

    if tunnel && !tunnel_enabled {
        let _ = control_tx.send(output::ControlCmd::ToggleTunnel).await;
    }

    if interactive {
        let adapter_name = if let Ok(cfg) = load_dev_tako_toml(&session.config_path) {
            if let Ok(preset_ref) = resolve_dev_preset_ref(&session.project_dir, &cfg) {
                match crate::build::load_dev_build_preset(&session.project_dir, &preset_ref).await {
                    Ok((preset, _)) => preset.name,
                    Err(_) => infer_preset_name_from_ref(&preset_ref),
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        output::run_dev_output(
            app_name.to_string(),
            adapter_name,
            display_hosts,
            public_port,
            lan_enabled,
            session.tunnel_url.clone(),
            log_rx,
            event_rx,
            control_tx,
        )
        .await?;
    } else {
        println!("{}", session.url);
        println!("Connected to running dev app '{}'.", app_name);

        let mut log_rx = log_rx;
        let mut event_rx = event_rx;
        let mut stop_rx = stop_rx.clone();
        tokio::select! {
            _ = async {
                loop {
                    tokio::select! {
                        Some(log) = log_rx.recv() => {
                            println!(
                                "{} {:<5} [{}] {}",
                                log.timestamp, log.level, log.scope, log.message
                            );
                        }
                        Some(event) = event_rx.recv() => {
                            match event {
                                DevEvent::AppStopped => println!("○ App stopped (idle)"),
                                DevEvent::AppError(e) => eprintln!("App error: {}", e),
                                DevEvent::ExitWithMessage(msg) => {
                                    println!("{}", msg);
                                    break;
                                }
                                DevEvent::ClientConnected { is_self, client_id } => {
                                    if !is_self {
                                        println!("Client {} connected", client_id);
                                    }
                                }
                                DevEvent::ClientDisconnected { client_id } => {
                                    println!("Client {} disconnected", client_id);
                                }
                                DevEvent::AppLaunching
                                | DevEvent::AppStarted
                                | DevEvent::AppReady
                                | DevEvent::AppPid(_)
                                | DevEvent::AppProcessExited(_)
                                | DevEvent::LanStarting
                                | DevEvent::LanFailed
                                | DevEvent::LanModeChanged { .. }
                                | DevEvent::TunnelStarting
                                | DevEvent::TunnelFailed
                                | DevEvent::TunnelModeChanged {
                                    enabled: true, ..
                                } => {}
                                | DevEvent::TunnelConnectionChanged {
                                    connected: true, ..
                                } => {
                                    println!("Tunnel reconnected");
                                }
                                | DevEvent::TunnelConnectionChanged {
                                    connected: false, ..
                                } => {
                                    println!("Tunnel reconnecting: connection lost");
                                }
                                | DevEvent::TunnelModeChanged {
                                    enabled: false,
                                    close_reason,
                                    ..
                                } => {
                                    let message = close_reason
                                        .map(TunnelCloseReason::log_message)
                                        .unwrap_or("Tunnel off");
                                    println!("{}", message);
                                }
                            }
                        }
                        else => break,
                    }
                }
            } => {}
            _ = async {
                while stop_rx.changed().await.is_ok() {
                    if *stop_rx.borrow() {
                        break;
                    }
                }
            } => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }

    let _ = stop_tx.send(true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn exit_with_message_event_breaks_output_loop() {
        let (event_tx, mut event_rx) = mpsc::channel::<DevEvent>(32);

        event_tx
            .send(DevEvent::ExitWithMessage(
                "stopped by another client".to_string(),
            ))
            .await
            .unwrap();

        let event = timeout(Duration::from_millis(100), event_rx.recv())
            .await
            .expect("should not time out")
            .expect("channel should not be closed");

        match event {
            DevEvent::ExitWithMessage(msg) => {
                assert_eq!(msg, "stopped by another client");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[tokio::test]
    async fn event_subscription_sends_exit_when_channel_closes() {
        let (event_tx, mut event_rx) = mpsc::channel::<DevEvent>(32);
        let (stop_tx, _stop_rx) = watch::channel(false);

        let event_tx_clone = event_tx.clone();
        let stop_tx_clone = stop_tx.clone();
        tokio::spawn(async move {
            let got_stop = false;

            let _ = stop_tx_clone.send(true);
            let msg = if got_stop {
                "stopped by another client".to_string()
            } else {
                "disconnected from dev server".to_string()
            };
            let _ = event_tx_clone.send(DevEvent::ExitWithMessage(msg)).await;
        });

        let event = timeout(Duration::from_millis(200), event_rx.recv())
            .await
            .expect("should not time out")
            .expect("channel should not be closed");

        match event {
            DevEvent::ExitWithMessage(msg) => {
                assert_eq!(msg, "disconnected from dev server");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[tokio::test]
    async fn event_subscription_sends_exit_on_stopped_status() {
        let (event_tx, mut event_rx) = mpsc::channel::<DevEvent>(32);
        let (stop_tx, _stop_rx) = watch::channel(false);

        let event_tx_clone = event_tx.clone();
        let stop_tx_clone = stop_tx.clone();
        let config_key = "/proj/tako.toml".to_string();

        tokio::spawn(async move {
            let mut got_stop = false;

            let events = vec![crate::dev_server_client::DevServerEvent::AppStatusChanged {
                config_path: "/proj/tako.toml".to_string(),
                app_name: "my-app".to_string(),
                status: "stopped".to_string(),
            }];

            for ev in events {
                if let crate::dev_server_client::DevServerEvent::AppStatusChanged {
                    ref config_path,
                    ref status,
                    ..
                } = ev
                    && config_path == &config_key
                    && status == "stopped"
                {
                    got_stop = true;
                    break;
                }
            }

            let _ = stop_tx_clone.send(true);
            let msg = if got_stop {
                "stopped by another client".to_string()
            } else {
                "disconnected from dev server".to_string()
            };
            let _ = event_tx_clone.send(DevEvent::ExitWithMessage(msg)).await;
        });

        let event = timeout(Duration::from_millis(200), event_rx.recv())
            .await
            .expect("should not time out")
            .expect("channel should not be closed");

        match event {
            DevEvent::ExitWithMessage(msg) => {
                assert_eq!(msg, "stopped by another client");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[tokio::test]
    async fn event_subscription_ignores_non_matching_app_name() {
        let (event_tx, mut event_rx) = mpsc::channel::<DevEvent>(32);
        let (stop_tx, _stop_rx) = watch::channel(false);

        let event_tx_clone = event_tx.clone();
        let stop_tx_clone = stop_tx.clone();
        let config_key = "/proj/tako.toml".to_string();

        tokio::spawn(async move {
            let mut got_stop = false;

            let events = vec![crate::dev_server_client::DevServerEvent::AppStatusChanged {
                config_path: "/other/tako.toml".to_string(),
                app_name: "other-app".to_string(),
                status: "stopped".to_string(),
            }];

            for ev in events {
                if let crate::dev_server_client::DevServerEvent::AppStatusChanged {
                    ref config_path,
                    ref status,
                    ..
                } = ev
                    && config_path == &config_key
                    && status == "stopped"
                {
                    got_stop = true;
                    break;
                }
            }

            let _ = stop_tx_clone.send(true);
            let msg = if got_stop {
                "stopped by another client".to_string()
            } else {
                "disconnected from dev server".to_string()
            };
            let _ = event_tx_clone.send(DevEvent::ExitWithMessage(msg)).await;
        });

        let event = timeout(Duration::from_millis(200), event_rx.recv())
            .await
            .expect("should not time out")
            .expect("channel should not be closed");

        match event {
            DevEvent::ExitWithMessage(msg) => {
                assert_eq!(msg, "disconnected from dev server");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[tokio::test]
    async fn event_subscription_ignores_idle_status() {
        let (event_tx, mut event_rx) = mpsc::channel::<DevEvent>(32);
        let (stop_tx, _stop_rx) = watch::channel(false);

        let event_tx_clone = event_tx.clone();
        let stop_tx_clone = stop_tx.clone();
        let config_key = "/proj/tako.toml".to_string();

        tokio::spawn(async move {
            let mut got_stop = false;

            let events = vec![crate::dev_server_client::DevServerEvent::AppStatusChanged {
                config_path: "/proj/tako.toml".to_string(),
                app_name: "my-app".to_string(),
                status: "idle".to_string(),
            }];

            for ev in events {
                if let crate::dev_server_client::DevServerEvent::AppStatusChanged {
                    ref config_path,
                    ref status,
                    ..
                } = ev
                    && config_path == &config_key
                    && status == "stopped"
                {
                    got_stop = true;
                    break;
                }
            }

            let _ = stop_tx_clone.send(true);
            let msg = if got_stop {
                "stopped by another client".to_string()
            } else {
                "disconnected from dev server".to_string()
            };
            let _ = event_tx_clone.send(DevEvent::ExitWithMessage(msg)).await;
        });

        let event = timeout(Duration::from_millis(200), event_rx.recv())
            .await
            .expect("should not time out")
            .expect("channel should not be closed");

        match event {
            DevEvent::ExitWithMessage(msg) => {
                assert_eq!(msg, "disconnected from dev server");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[tokio::test]
    async fn child_exit_monitor_detects_nonzero_exit() {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("exit 42")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        let _ = child.wait().await;

        let status = child.try_wait().unwrap();
        assert!(status.is_some(), "child should have exited");
        let status = status.unwrap();
        assert!(!status.success());
        assert_eq!(status.code(), Some(42));
    }

    #[tokio::test]
    async fn child_exit_monitor_detects_clean_exit() {
        let mut child = tokio::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        let _ = child.wait().await;

        let status = child.try_wait().unwrap();
        assert!(status.is_some(), "child should have exited");
        assert!(status.unwrap().success());
    }
}

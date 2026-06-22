use tokio::sync::{mpsc, watch};

use super::super::{
    DevEvent, ScopedLog, TunnelCloseReason, output_render::format_tunnel_block, parse_log_line,
};

pub(super) async fn spawn_dev_event_forwarder(
    config_key: String,
    event_tx: mpsc::Sender<DevEvent>,
    should_exit_tx: watch::Sender<bool>,
    log_tx: mpsc::Sender<ScopedLog>,
) {
    let mut ev_rx = match crate::dev_server_client::subscribe_events().await {
        Ok(rx) => Some(rx),
        Err(e) => {
            let _ = log_tx
                .send(ScopedLog::warn(
                    "tako",
                    format!("failed to subscribe to dev server events: {}", e),
                ))
                .await;
            None
        }
    };

    if let Some(mut ev_rx) = ev_rx.take() {
        tokio::spawn(async move {
            use crate::dev_server_client::DevServerEvent;
            while let Some(ev) = ev_rx.recv().await {
                match ev {
                    DevServerEvent::AppStatusChanged {
                        ref config_path,
                        ref status,
                        ..
                    } if config_path == &config_key && status == "stopped" => {
                        let _ = event_tx
                            .send(DevEvent::ExitWithMessage(
                                "stopped by another client".to_string(),
                            ))
                            .await;
                        let _ = should_exit_tx.send(true);
                        break;
                    }
                    DevServerEvent::ClientConnected {
                        ref config_path,
                        client_id,
                        ..
                    } if config_path == &config_key => {
                        let _ = event_tx
                            .send(DevEvent::ClientConnected {
                                is_self: false,
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
        });
    }
}

pub(super) fn spawn_log_forwarder(config_key: String, log_tx: mpsc::Sender<ScopedLog>) {
    tokio::spawn(async move {
        let Ok(mut rx) = crate::dev_server_client::subscribe_logs(&config_key, None).await else {
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

pub(super) async fn run_non_interactive_output(
    mut log_rx: mpsc::Receiver<ScopedLog>,
    mut event_rx: mpsc::Receiver<DevEvent>,
    mut should_exit_rx: watch::Receiver<bool>,
) {
    tokio::select! {
        _ = async {
            loop {
                tokio::select! {
                    Some(log) = log_rx.recv() => {
                        crate::output::stream_line(&format!(
                            "{} {:<5} [{}] {}",
                            log.timestamp, log.level, log.scope, log.message
                        ));
                    }
                    Some(event) = event_rx.recv() => {
                        if handle_non_interactive_event(event) {
                            break;
                        }
                    }
                }
            }
        } => {}
        _ = async {
            while should_exit_rx.changed().await.is_ok() {
                if *should_exit_rx.borrow() {
                    break;
                }
            }
        } => {}
    }
}

fn handle_non_interactive_event(event: DevEvent) -> bool {
    match event {
        DevEvent::AppStarted => {}
        DevEvent::AppReady => {
            crate::output::stream_line("App started");
        }
        DevEvent::AppLaunching => {
            crate::output::stream_line("Starting app…");
        }
        DevEvent::AppStopped => {
            crate::output::stream_line("○ App stopped (idle)");
        }
        DevEvent::AppPid(pid) => {
            crate::output::stream_line(&format!("App pid {}", pid));
        }
        DevEvent::AppProcessExited(_) => {}
        DevEvent::AppError(e) => {
            eprintln!("App error: {}", e);
        }
        DevEvent::ClientConnected { is_self, client_id } => {
            if !is_self {
                crate::output::stream_line(&format!("Client {} connected", client_id));
            }
        }
        DevEvent::ClientDisconnected { client_id } => {
            crate::output::stream_line(&format!("Client {} disconnected", client_id));
        }
        DevEvent::LanModeChanged {
            enabled, lan_ip, ..
        } => {
            if enabled {
                if let Some(ip) = lan_ip {
                    crate::output::stream_line(&format!("LAN mode enabled — {}", ip));
                }
            } else {
                crate::output::stream_line("LAN mode disabled");
            }
        }
        DevEvent::LanStarting | DevEvent::LanFailed => {}
        DevEvent::TunnelModeChanged {
            enabled,
            url,
            close_reason,
            ..
        } => {
            for line in non_interactive_tunnel_mode_lines(enabled, url.as_deref(), close_reason) {
                crate::output::stream_line(&line);
            }
        }
        DevEvent::TunnelConnectionChanged { connected, .. } => {
            if connected {
                crate::output::stream_line("Tunnel reconnected");
            } else {
                crate::output::stream_line("Tunnel reconnecting: connection lost");
            }
        }
        DevEvent::TunnelStarting | DevEvent::TunnelFailed => {}
        DevEvent::ExitWithMessage(msg) => {
            crate::output::stream_line(&msg);
            return true;
        }
    }
    false
}

fn non_interactive_tunnel_mode_lines(
    enabled: bool,
    url: Option<&str>,
    close_reason: Option<TunnelCloseReason>,
) -> Vec<String> {
    if enabled {
        url.map(format_tunnel_block).unwrap_or_default()
    } else {
        vec![
            close_reason
                .map(TunnelCloseReason::log_message)
                .unwrap_or("Tunnel off")
                .to_string(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_interactive_tunnel_enable_prints_public_url_block() {
        let lines = non_interactive_tunnel_mode_lines(
            true,
            Some("https://bun-e2e-bhed5ab6yy.tako.website"),
            None,
        );

        let rendered = lines.join("\n");
        assert!(rendered.contains("Your app is now available on the public internet"));
        assert!(rendered.contains("https://bun-e2e-bhed5ab6yy.tako.website"));
    }

    #[test]
    fn non_interactive_tunnel_disable_prints_close_reason() {
        let lines =
            non_interactive_tunnel_mode_lines(false, None, Some(TunnelCloseReason::LimitExceeded));

        assert_eq!(
            lines,
            vec!["Tunnel off: active tunnel limit reached (5 per machine)"]
        );
    }
}

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{mpsc, watch};

use super::super::{DevEvent, ScopedLog, output};

pub(super) struct ControlLoop {
    pub(super) config_key: String,
    pub(super) initial_lan_enabled: bool,
    pub(super) initial_tunnel_enabled: bool,
    pub(super) control_rx: mpsc::Receiver<output::ControlCmd>,
    pub(super) log_tx: mpsc::Sender<ScopedLog>,
    pub(super) event_tx: mpsc::Sender<DevEvent>,
    pub(super) should_exit_tx: watch::Sender<bool>,
    pub(super) terminate_requested: Arc<AtomicBool>,
}

pub(super) fn spawn_control_loop(control: ControlLoop) {
    let ControlLoop {
        config_key,
        initial_lan_enabled,
        initial_tunnel_enabled,
        mut control_rx,
        log_tx,
        event_tx,
        should_exit_tx,
        terminate_requested,
    } = control;

    tokio::spawn(async move {
        let mut lan_enabled = initial_lan_enabled;
        let mut tunnel_enabled = initial_tunnel_enabled;
        while let Some(cmd_in) = control_rx.recv().await {
            match cmd_in {
                output::ControlCmd::Restart => {
                    let result = crate::dev_server_client::restart_app(&config_key)
                        .await
                        .map_err(|e| e.to_string());
                    if let Err(msg) = result {
                        let _ = log_tx
                            .send(ScopedLog::error("tako", format!("restart failed: {}", msg)))
                            .await;
                    }
                }
                output::ControlCmd::Terminate => {
                    terminate_requested.store(true, Ordering::Relaxed);
                    let _ = crate::dev_server_client::unregister_app(&config_key).await;
                    let _ = should_exit_tx.send(true);
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
                    let current = crate::dev_server_client::registered_tunnel_enabled(&config_key)
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

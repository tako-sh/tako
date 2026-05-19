use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{mpsc, watch};

use super::super::{ScopedLog, output};

pub(super) fn spawn_control_loop(
    config_key: String,
    initial_lan_enabled: bool,
    mut control_rx: mpsc::Receiver<output::ControlCmd>,
    log_tx: mpsc::Sender<ScopedLog>,
    should_exit_tx: watch::Sender<bool>,
    terminate_requested: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let mut lan_enabled = initial_lan_enabled;
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
                    let result = crate::dev_server_client::toggle_lan(target)
                        .await
                        .map_err(|e| e.to_string());
                    match result {
                        Ok((enabled, _, _)) => {
                            lan_enabled = enabled;
                        }
                        Err(msg) => {
                            let _ = log_tx
                                .send(ScopedLog::error(
                                    "tako",
                                    format!("LAN toggle failed: {}", msg),
                                ))
                                .await;
                        }
                    }
                }
            }
        }
    });
}

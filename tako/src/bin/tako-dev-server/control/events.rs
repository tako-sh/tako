use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::protocol::{DevEvent, Response};

#[derive(Default)]
struct Inner {
    subs: Vec<mpsc::Sender<Response>>,
    // One current lifecycle/connection event per registered app, not a replay log.
    app_states: HashMap<String, DevEvent>,
    tunnel_states: HashMap<String, DevEvent>,
}

#[derive(Clone, Default)]
pub(crate) struct EventsHub {
    inner: Arc<Mutex<Inner>>,
}
impl EventsHub {
    pub(crate) fn subscribe(&self) -> mpsc::Receiver<Response> {
        let (tx, rx) = mpsc::channel(512);
        self.inner.lock().unwrap().subs.push(tx);
        rx
    }

    pub(crate) fn current_app_event(&self, config_path: &str) -> Option<DevEvent> {
        self.inner
            .lock()
            .unwrap()
            .app_states
            .get(config_path)
            .cloned()
    }

    pub(crate) fn current_tunnel_event(&self, config_path: &str) -> Option<DevEvent> {
        self.inner
            .lock()
            .unwrap()
            .tunnel_states
            .get(config_path)
            .cloned()
    }

    pub(crate) fn forget_app(&self, config_path: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.app_states.remove(config_path);
        inner.tunnel_states.remove(config_path);
    }

    pub(crate) fn broadcast(&self, r: Response) {
        let mut inner = self.inner.lock().unwrap();
        if let Response::Event { event } = &r {
            match event {
                DevEvent::AppLaunching { config_path, .. }
                | DevEvent::AppStarted { config_path, .. }
                | DevEvent::AppReady { config_path, .. }
                | DevEvent::AppProcessExited { config_path, .. }
                | DevEvent::AppError { config_path, .. } => {
                    inner.app_states.insert(config_path.clone(), event.clone());
                }
                DevEvent::AppStatusChanged {
                    config_path,
                    status,
                    ..
                } => {
                    // Successful starts announce ready before running. Keep
                    // that lifecycle state available for recovery snapshots.
                    if status != "running" {
                        inner.app_states.remove(config_path);
                    }
                    if status == "stopped" {
                        inner.tunnel_states.remove(config_path);
                    }
                }
                DevEvent::TunnelConnectionChanged { config_path, .. } => {
                    inner
                        .tunnel_states
                        .insert(config_path.clone(), event.clone());
                }
                DevEvent::TunnelModeChanged { config_path, .. } => {
                    inner.tunnel_states.remove(config_path);
                }
                _ => {}
            }
        }
        // Disconnect a lagging status subscriber so it can reconnect and obtain
        // current state, rather than retaining an unbounded stale event queue.
        inner.subs.retain(|tx| tx.try_send(r.clone()).is_ok());
    }
}

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use crate::state;
use crate::state::RuntimeApp;

use super::events::EventsHub;

pub(crate) struct State {
    pub(crate) events: EventsHub,

    pub(crate) shutdown_tx: watch::Sender<bool>,
    idle_generation: Arc<std::sync::atomic::AtomicU64>,

    pub(crate) routes: crate::proxy::Routes,
    pub(crate) local_dns_enabled: bool,
    pub(crate) local_dns_port: u16,

    pub(crate) listen_port: u16,
    pub(crate) listen_addr: String,
    pub(crate) advertised_ip: String,
    pub(crate) control_clients: u32,

    pub(crate) lan_enabled: bool,
    pub(crate) lan_ip: Option<String>,
    pub(crate) mdns: Option<crate::lan::MdnsPublisher>,

    pub(crate) db: Option<state::DevStateStore>,
    pub(crate) apps: std::collections::HashMap<String, RuntimeApp>,

    /// Path to the shared Tako internal unix socket. `Some` once
    /// `workflows.start_socket()` succeeds in main; injected into every
    /// spawned app process as `TAKO_INTERNAL_SOCKET` so
    /// workflow `.enqueue()` / channel `.publish()` work the same way in
    /// dev as they do on deployed servers.
    pub(crate) internal_socket: Option<std::path::PathBuf>,

    /// Shared workflow manager. `Some` once `main` constructs it. On each
    /// `RegisterApp` we call `ensure()` with a `workers: 0` scale-to-zero
    /// spec and a short idle timeout — so the worker subprocess only
    /// exists while there's real work, and every wake re-spawns it
    /// (picking up whatever code the user just edited, no watcher needed).
    pub(crate) workflows: Option<Arc<tako_workflows::WorkflowManager>>,
}

impl State {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        shutdown_tx: watch::Sender<bool>,
        routes: crate::proxy::Routes,
        events: EventsHub,
        local_dns_enabled: bool,
        local_dns_port: u16,
        listen_port: u16,
        listen_addr: String,
        advertised_ip: String,
    ) -> Self {
        Self {
            events,
            shutdown_tx,
            idle_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            routes,
            local_dns_enabled,
            local_dns_port,
            listen_port,
            listen_addr,
            advertised_ip,
            control_clients: 0,
            lan_enabled: false,
            lan_ip: None,
            mdns: None,
            db: None,
            apps: std::collections::HashMap::new(),
            internal_socket: None,
            workflows: None,
        }
    }

    pub(super) fn cancel_idle_exit(&mut self) {
        let _ = self
            .idle_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub(super) fn schedule_idle_exit(&mut self) {
        let generation = self
            .idle_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let shutdown_tx = self.shutdown_tx.clone();
        let idle_generation = self.idle_generation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if idle_generation.load(std::sync::atomic::Ordering::SeqCst) == generation {
                let _ = shutdown_tx.send(true);
            }
        });
    }
}

pub(super) struct ControlClientSubscription {
    state: Arc<Mutex<State>>,
}

impl ControlClientSubscription {
    pub(super) fn register(state: &Arc<Mutex<State>>) -> Self {
        if let Ok(mut s) = state.lock() {
            s.control_clients = s.control_clients.saturating_add(1);
        }
        Self {
            state: state.clone(),
        }
    }
}
impl Drop for ControlClientSubscription {
    fn drop(&mut self) {
        if let Ok(mut s) = self.state.lock() {
            s.control_clients = s.control_clients.saturating_sub(1);
        }
    }
}

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::ServerState;
use crate::instances::{HealthChecker, HealthConfig};
use crate::runtime_events::{handle_health_event, handle_idle_event, handle_instance_event};
use crate::scaling::{IdleConfig, IdleMonitor};
use crate::socket::SocketServer;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

pub(super) fn spawn_management_http(rt: &Runtime, state: Arc<ServerState>, host: Option<String>) {
    let Some(host) = host else {
        return;
    };

    rt.spawn(async move {
        if let Err(error) = crate::management_http::serve(host, state).await {
            tracing::error!("Remote management HTTP stopped: {error}");
        }
    });
}

pub(super) fn spawn_instance_event_bridge(rt: &Runtime, state: Arc<ServerState>) {
    if let Some(mut event_rx) = state.app_manager().take_event_receiver() {
        let state_clone = state.clone();
        rt.spawn(async move {
            while let Some(event) = event_rx.recv().await {
                handle_instance_event(&state_clone, event).await;
            }
        });
    }
}

pub(super) fn spawn_health_monitoring(rt: &Runtime, state: Arc<ServerState>) {
    let (health_event_tx, mut health_event_rx) = mpsc::channel(256);
    let health_checker = Arc::new(HealthChecker::new(HealthConfig::default(), health_event_tx));
    let app_manager = state.app_manager();
    let health_checker_clone = health_checker.clone();
    rt.spawn(async move {
        let mut app_tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

        loop {
            let app_set: HashSet<_> = app_manager.list_apps().into_iter().collect();

            for app_name in &app_set {
                if !app_tasks.contains_key(app_name)
                    && let Some(app) = app_manager.get_app(app_name)
                {
                    let checker = health_checker_clone.clone();
                    let task = tokio::spawn(async move {
                        checker.monitor_app(app).await;
                    });
                    app_tasks.insert(app_name.clone(), task);
                }
            }

            app_tasks.retain(|app_name, task| {
                if !app_set.contains(app_name) {
                    task.abort();
                    false
                } else {
                    true
                }
            });

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    let health_state = state.clone();
    rt.spawn(async move {
        while let Some(event) = health_event_rx.recv().await {
            handle_health_event(&health_state, event).await;
        }
    });
}

pub(super) fn spawn_idle_monitoring(rt: &Runtime, state: Arc<ServerState>) {
    let (idle_event_tx, mut idle_event_rx) = mpsc::channel(256);
    let idle_monitor = Arc::new(IdleMonitor::new(IdleConfig::default(), idle_event_tx));
    let app_manager = state.app_manager();
    let idle_monitor_clone = idle_monitor.clone();
    rt.spawn(async move {
        let mut app_tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

        loop {
            let app_set: HashSet<_> = app_manager.list_apps().into_iter().collect();

            for app_name in &app_set {
                if !app_tasks.contains_key(app_name)
                    && let Some(app) = app_manager.get_app(app_name)
                {
                    let monitor = idle_monitor_clone.clone();
                    let task = tokio::spawn(async move {
                        monitor.monitor_app(app).await;
                    });
                    app_tasks.insert(app_name.clone(), task);
                }
            }

            app_tasks.retain(|app_name, task| {
                if !app_set.contains(app_name) {
                    task.abort();
                    false
                } else {
                    true
                }
            });

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    let idle_state = state.clone();
    rt.spawn(async move {
        while let Some(event) = idle_event_rx.recv().await {
            handle_idle_event(&idle_state, event).await;
        }
    });
}

pub(super) fn spawn_management_socket(
    rt: &Runtime,
    state: Arc<ServerState>,
    socket_listener: Option<std::os::unix::net::UnixListener>,
) {
    if let Some(socket_listener) = socket_listener {
        rt.spawn(async move {
            if let Err(e) = SocketServer::serve(socket_listener, move |cmd| {
                let state = state.clone();
                async move { state.handle_command(cmd).await }
            })
            .await
            {
                tracing::error!("Socket server error: {}", e);
            }
        });
    }
}

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use crate::control::State;

pub(crate) fn app_lock(state: &Arc<Mutex<State>>, key: &str) -> Arc<tokio::sync::Mutex<()>> {
    state
        .lock()
        .unwrap()
        .app_locks
        .entry(key.into())
        .or_default()
        .clone()
}

pub(crate) async fn stop(state: &Arc<Mutex<State>>, key: &str) -> Result<(), String> {
    let runtime = state
        .lock()
        .unwrap()
        .apps
        .get(key)
        .and_then(|app| app.compose.clone());
    if let Some(runtime) = runtime {
        runtime.stop().await?;
    }
    Ok(())
}

pub(crate) async fn stop_all(state: &Arc<Mutex<State>>) {
    let keys: Vec<_> = state.lock().unwrap().apps.keys().cloned().collect();
    for key in keys {
        let lock = app_lock(state, &key);
        let _guard = lock.lock().await;
        if let Err(error) = stop(state, &key).await {
            tracing::error!(%error, "failed to stop development Compose project");
        }
    }
}

pub(crate) async fn spawn(
    runtime: &tako_compose::ComposeRuntime,
    log_buffer: &crate::state::LogBuffer,
) -> Result<(tokio::process::Child, Option<OwnedFd>), Box<dyn std::error::Error + Send + Sync>> {
    match runtime.start_development().await {
        Ok(mut child) => {
            crate::process::forward_child_pipes(&mut child, log_buffer);
            Ok((child, None))
        }
        Err(error) => Err(match runtime.stop().await {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; Compose cleanup failed: {cleanup}"),
        }
        .into()),
    }
}

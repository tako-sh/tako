use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
) -> Result<(tokio::process::Child, Option<OwnedFd>), Box<dyn std::error::Error + Send + Sync>> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let mut child = match runtime.start_development(port).await {
        Ok(child) => child,
        Err(error) => {
            return Err(match runtime.stop().await {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; Compose cleanup failed: {cleanup}"),
            }
            .into());
        }
    };
    let ready = async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|e| e.to_string())?;
        let url = format!("http://127.0.0.1:{port}{}", runtime.config.health_path);
        for _ in 0..60 {
            if let Ok(response) = client
                .get(&url)
                .header(
                    "Host",
                    runtime.config.health_host.as_deref().unwrap_or("localhost"),
                )
                .send()
                .await
                && (response.status().is_success() || response.status().is_redirection())
            {
                let (read, write) =
                    crate::process::create_readiness_pipe().map_err(|e| e.to_string())?;
                writeln!(std::fs::File::from(write), "{port}").map_err(|e| e.to_string())?;
                return Ok(read);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err("Compose HTTP service did not become healthy".to_string())
    }
    .await;
    match ready {
        Ok(read) => Ok((child, Some(read))),
        Err(error) => {
            let cleanup = runtime.stop().await;
            let _ = child.kill().await;
            Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; Compose cleanup failed: {cleanup}"),
            }
            .into())
        }
    }
}

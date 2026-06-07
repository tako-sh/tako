use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::{SIGNAL_PARENT_ON_READY_ENV, ServerState};
#[cfg(unix)]
use async_trait::async_trait;
#[cfg(unix)]
use pingora_core::server::{ShutdownSignal, ShutdownSignalWatch};
use tokio::runtime::Runtime;

pub(super) fn spawn_reload_signal_handlers(rt: &Runtime, startup_exe: Option<PathBuf>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        rt.spawn(async move {
            let mut sighup = match signal(SignalKind::hangup()) {
                Ok(signal) => signal,
                Err(err) => {
                    tracing::error!("Failed to register SIGHUP handler: {err}");
                    return;
                }
            };
            sighup.recv().await;
            tracing::info!(
                "SIGHUP received — spawning new server process for zero-downtime reload"
            );
            let exe = match &startup_exe {
                Some(p) => p.clone(),
                None => match std::env::current_exe() {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!("Failed to get current exe: {e}");
                        return;
                    }
                },
            };
            let args: Vec<String> = std::env::args().skip(1).collect();
            match std::process::Command::new(&exe)
                .args(&args)
                .env(SIGNAL_PARENT_ON_READY_ENV, "1")
                .spawn()
            {
                Ok(child) => tracing::info!(pid = child.id(), "New server process spawned"),
                Err(e) => tracing::error!("Failed to spawn new server: {e}"),
            }
        });

        rt.spawn(async move {
            let mut sigusr1 = match signal(SignalKind::user_defined1()) {
                Ok(signal) => signal,
                Err(err) => {
                    tracing::error!("Failed to register SIGUSR1 handler: {err}");
                    return;
                }
            };
            sigusr1.recv().await;
            tracing::info!("SIGUSR1 received — new process ready, starting graceful drain");
            unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
        });
    }
}

#[cfg(unix)]
pub(super) struct TakoShutdownSignalWatch {
    state: Arc<ServerState>,
    workflow_drain_timeout: Duration,
}

#[cfg(unix)]
impl TakoShutdownSignalWatch {
    pub(super) fn new(state: Arc<ServerState>, workflow_drain_timeout: Duration) -> Self {
        Self {
            state,
            workflow_drain_timeout,
        }
    }
}

#[cfg(unix)]
#[async_trait]
impl ShutdownSignalWatch for TakoShutdownSignalWatch {
    async fn recv(&self) -> ShutdownSignal {
        use tokio::signal::unix::{SignalKind, signal};

        let mut graceful_upgrade = match signal(SignalKind::quit()) {
            Ok(signal) => signal,
            Err(err) => {
                tracing::error!("Failed to register SIGQUIT handler: {err}");
                return ShutdownSignal::FastShutdown;
            }
        };
        let mut graceful_terminate = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(err) => {
                tracing::error!("Failed to register SIGTERM handler: {err}");
                return ShutdownSignal::FastShutdown;
            }
        };
        let mut fast_shutdown = match signal(SignalKind::interrupt()) {
            Ok(signal) => signal,
            Err(err) => {
                tracing::error!("Failed to register SIGINT handler: {err}");
                return ShutdownSignal::FastShutdown;
            }
        };

        tokio::select! {
            _ = graceful_upgrade.recv() => ShutdownSignal::GracefulUpgrade,
            _ = fast_shutdown.recv() => ShutdownSignal::FastShutdown,
            _ = graceful_terminate.recv() => {
                let drain = self.state.shutdown_runtime(self.workflow_drain_timeout);
                tokio::pin!(drain);
                tokio::select! {
                    _ = &mut drain => ShutdownSignal::GracefulTerminate,
                    _ = fast_shutdown.recv() => ShutdownSignal::FastShutdown,
                    _ = graceful_terminate.recv() => ShutdownSignal::FastShutdown,
                }
            }
        }
    }
}

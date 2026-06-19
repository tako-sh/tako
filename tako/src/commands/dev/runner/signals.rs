use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::watch;

pub(super) fn spawn_signal_handlers(
    should_exit_tx: watch::Sender<bool>,
    terminate_requested: Arc<AtomicBool>,
    verbose: bool,
) {
    let should_exit_tx_ctrlc = should_exit_tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = should_exit_tx_ctrlc.send(true);
            if verbose {
                crate::output::stream_line("\nShutting down…");
            }
        }
    });

    spawn_unix_signal_handlers(should_exit_tx, terminate_requested, verbose);
}

#[cfg(unix)]
fn spawn_unix_signal_handlers(
    should_exit_tx: watch::Sender<bool>,
    terminate_requested: Arc<AtomicBool>,
    verbose: bool,
) {
    let should_exit_tx_term = should_exit_tx.clone();
    tokio::spawn(async move {
        if let Ok(mut sigterm) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            let _ = sigterm.recv().await;
            terminate_requested.store(true, Ordering::Relaxed);
            let _ = should_exit_tx_term.send(true);
            if verbose {
                crate::output::stream_line("\nTerminating…");
            }
        }
    });

    let should_exit_tx_hup = should_exit_tx.clone();
    tokio::spawn(async move {
        if let Ok(mut sighup) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        {
            let _ = sighup.recv().await;
            let _ = should_exit_tx_hup.send(true);
            if verbose {
                crate::output::stream_line("\nDisconnected from terminal.");
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_unix_signal_handlers(
    _should_exit_tx: watch::Sender<bool>,
    _terminate_requested: Arc<AtomicBool>,
    _verbose: bool,
) {
}

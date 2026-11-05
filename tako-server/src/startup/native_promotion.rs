use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tako_core::{AppState, InstanceState, UpgradeMode};
use tokio::time::Instant;

use crate::{SIGNAL_PARENT_ON_READY_ENV, ServerState};

pub(super) const PROMOTION_CHILD_ENV: &str = "TAKO_NATIVE_PROMOTION";
const RETIREMENT_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) fn is_promotion_child() -> bool {
    std::env::var_os(PROMOTION_CHILD_ENV).as_deref() == Some(OsStr::new("1"))
}

pub(super) fn guard_promotion_startup(data_dir: &Path) -> Result<(), String> {
    if is_promotion_child() {
        check_promotion_startup(data_dir)?;
    }
    Ok(())
}

pub(super) async fn run(state: Arc<ServerState>) {
    loop {
        if let Err(error) = promote(state.clone(), None).await {
            tracing::warn!(%error, "Native promotion deferred before local retirement");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// Call under PrimaryHandoff, before ServerState construction, and skip marker consumption.
pub(super) fn check_promotion_startup(data_dir: &Path) -> Result<(), String> {
    reject_reload_marker(data_dir)?;
    let connection = rusqlite::Connection::open_with_flags(
        data_dir.join("state.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|error| format!("Cannot inspect promotion upgrade state: {error}"))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    let (mode, owned): (Option<String>, bool) = connection
        .query_row(
            "SELECT (SELECT server_mode FROM server_state WHERE id = 1),
                    EXISTS(SELECT 1 FROM upgrade_lock WHERE id = 1)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| format!("Cannot inspect promotion upgrade state: {error}"))?;
    if mode.as_deref() != Some("normal") || owned {
        return Err("Native promotion deferred while an upgrade is active".into());
    }
    reject_reload_marker(data_dir)
}

fn reject_reload_marker(data_dir: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(data_dir.join(tako_core::UPGRADE_RELOAD_MARKER_FILE)) {
        Ok(_) => Err("Native promotion deferred while an upgrade reload marker exists".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Cannot inspect upgrade reload marker: {error}")),
    }
}

// Run as an owned startup task. After local admission closes, only process exit cancels it.
async fn promote(state: Arc<ServerState>, startup_exe: Option<PathBuf>) -> Result<(), String> {
    let executable = match startup_exe {
        Some(path) => path,
        None => std::env::current_exe().map_err(|error| error.to_string())?,
    };
    let arguments = primary_arguments(std::env::args_os().skip(1));
    let _management = state.management_lock.clone().lock_owned().await;
    if state
        .state_store
        .server_mode()
        .map_err(|error| error.to_string())?
        != UpgradeMode::Normal
        || state
            .state_store
            .upgrade_lock_owner()
            .map_err(|error| error.to_string())?
            .is_some()
    {
        return Err("Native promotion deferred while an upgrade is active".into());
    }
    reject_reload_marker(&state.runtime_config().data_dir)?;

    state.drain_flag().store(true, Ordering::Release);
    state.proxy_requests.begin_drain();
    let deadline = Instant::now() + RETIREMENT_TIMEOUT;
    if let Err(error) = retire_local_runtime(&state, deadline).await {
        tracing::error!(%error, "Native promotion retirement incomplete; local runtime remains gated until restart");
        return std::future::pending().await;
    }

    let child = tokio::process::Command::new(executable)
        .args(arguments)
        .env(SIGNAL_PARENT_ON_READY_ENV, "1")
        .env(PROMOTION_CHILD_ENV, "1")
        .spawn();
    match child {
        Ok(mut child) => {
            tracing::info!(
                pid = child.id(),
                "Native promotion child started; waiting for its readiness signal"
            );
            match child.wait().await {
                Ok(status) => {
                    tracing::error!(%status, "Native promotion child exited; retired parent remains gated until restart")
                }
                Err(error) => {
                    tracing::error!(%error, "Native promotion child wait failed; retired parent remains gated until restart")
                }
            }
        }
        Err(error) => {
            tracing::error!(%error, "Native promotion child failed to start; retired parent remains gated until restart");
        }
    }
    // The observer must not reopen this stale state while the child starts or after failure.
    std::future::pending().await
}

async fn retire_local_runtime(state: &ServerState, deadline: Instant) -> Result<(), String> {
    state.workflows.begin_server_drain();
    let forced_transports = state.proxy_requests.force_drain();
    let (forced_workers, worker_errors) = state.workflows.force_server_drain();
    tracing::info!(
        forced_transports,
        forced_workers,
        "Retiring local runtime for native promotion"
    );
    if worker_errors != 0 {
        return Err(format!(
            "Failed to signal {worker_errors} local workflow workers"
        ));
    }

    for name in state.app_manager.list_apps() {
        let Some(app) = state.app_manager.get_app(&name) else {
            continue;
        };
        app.set_state(AppState::Stopped);
        for instance in app.get_instances() {
            app.set_instance_state(&instance, InstanceState::Draining);
            // Compose containers survive this process; the fresh primary adopts them.
            instance.own_compose_container(false);
            tokio::time::timeout_at(deadline, instance.kill())
                .await
                .map_err(|_| format!("Local instance retirement timed out: {name}"))?
                .map_err(|error| format!("Local instance retirement failed: {name}: {error}"))?;
            app.remove_instance(&instance.id);
            crate::metrics::remove_instance_metrics(&name, &instance.id);
        }
        crate::runtime_events::update_instance_count_metric(&name, &app);
    }
    loop {
        if state.proxy_requests.active_count() == 0
            && state.workflows.server_drain_status().is_complete()
        {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(
                "Local requests or workflow workers remain after retirement deadline".into(),
            );
        }
        tokio::time::sleep_until(deadline.min(now + Duration::from_millis(10))).await;
    }
    // Close the old internal socket before the child can publish its replacement.
    tokio::time::timeout_at(deadline, state.workflows.shutdown_all(Duration::ZERO))
        .await
        .map_err(|_| "Local workflow socket shutdown timed out".to_string())?;
    state.drain_activity.lock().take();
    Ok(())
}

fn primary_arguments(arguments: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut options = true;
    arguments
        .into_iter()
        .filter(|argument| {
            if argument == OsStr::new("--") {
                options = false;
            }
            !options || argument != OsStr::new("--standby")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn native_promotion_preserves_os_arguments_and_removes_only_standby_flag() {
        let path = OsString::from_vec(b"/tmp/native-\xff".to_vec());
        assert_eq!(
            primary_arguments([
                "--standby".into(),
                "--data-dir".into(),
                path.clone(),
                "--https-port=8443".into(),
                "--".into(),
                "--standby".into(),
            ]),
            vec![
                OsString::from("--data-dir"),
                path,
                "--https-port=8443".into(),
                "--".into(),
                "--standby".into(),
            ]
        );
    }

    fn upgrade_state(path: &Path) -> rusqlite::Connection {
        let connection = rusqlite::Connection::open(path.join("state.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE server_state(id INTEGER PRIMARY KEY, server_mode TEXT);
                 INSERT INTO server_state VALUES(1, 'normal');
                 CREATE TABLE upgrade_lock(id INTEGER PRIMARY KEY, owner TEXT);",
            )
            .unwrap();
        connection
    }

    #[test]
    fn native_promotion_checks_mode_and_owner_without_clearing_either() {
        let temp = tempfile::tempdir().unwrap();
        let connection = upgrade_state(temp.path());
        assert!(check_promotion_startup(temp.path()).is_ok());
        connection
            .execute("INSERT INTO upgrade_lock VALUES(1, 'upgrade-owner')", [])
            .unwrap();
        assert!(check_promotion_startup(temp.path()).is_err());
        let owner: String = connection
            .query_row("SELECT owner FROM upgrade_lock", [], |row| row.get(0))
            .unwrap();
        assert_eq!(owner, "upgrade-owner");
        connection
            .execute_batch(
                "DELETE FROM upgrade_lock; UPDATE server_state SET server_mode='upgrading'",
            )
            .unwrap();
        assert!(check_promotion_startup(temp.path()).is_err());
    }

    #[test]
    fn native_promotion_never_consumes_reload_marker() {
        let temp = tempfile::tempdir().unwrap();
        let _connection = upgrade_state(temp.path());
        let marker = temp.path().join(tako_core::UPGRADE_RELOAD_MARKER_FILE);
        std::fs::write(&marker, "existing-upgrade-owner").unwrap();
        assert!(check_promotion_startup(temp.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            "existing-upgrade-owner"
        );
    }

    #[test]
    fn native_promotion_missing_database_does_not_create_one() {
        let temp = tempfile::tempdir().unwrap();
        assert!(check_promotion_startup(temp.path()).is_err());
        assert!(!temp.path().join("state.sqlite").exists());
    }
}

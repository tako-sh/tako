use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::control::State;

pub(crate) fn app_lock(state: &Arc<Mutex<State>>, key: &str) -> Arc<tokio::sync::Mutex<()>> {
    lifecycle_lock(state, format!("registration:{key}"))
}

pub(crate) fn runtime_name_lock(
    state: &Arc<Mutex<State>>,
    name: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    lifecycle_lock(state, format!("runtime-name:{name}"))
}

fn lifecycle_lock(state: &Arc<Mutex<State>>, key: String) -> Arc<tokio::sync::Mutex<()>> {
    state
        .lock()
        .unwrap()
        .app_locks
        .entry(key)
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

/// The development binding of a group's `sdk: true` services: the dev app
/// name and bootstrap token, and a pinned internal socket under the dev
/// data directory. The same contract as production; only the values differ.
pub(crate) struct DevelopmentSdk<'a> {
    pub(crate) app_name: &'a str,
    pub(crate) token: &'a str,
    pub(crate) env: &'a HashMap<String, String>,
    pub(crate) secrets: &'a HashMap<String, String>,
    pub(crate) storages: &'a HashMap<String, tako_core::StorageBinding>,
}

pub(crate) fn with_sdk(
    state: &Arc<Mutex<State>>,
    runtime: tako_compose::ComposeRuntime,
    sdk: DevelopmentSdk<'_>,
) -> Result<tako_compose::ComposeRuntime, String> {
    if !runtime.config.has_sdk_services() {
        // A reload clones the previous runtime, which may still carry the
        // SDK binding of services the Compose file no longer marks.
        return Ok(runtime.without_sdk());
    }
    let workflows = state.lock().unwrap().workflows.clone().ok_or(
        "Compose sdk: true services need the dev server's internal socket, which did not start",
    )?;
    let data_dir = crate::paths::tako_data_dir().map_err(|e| e.to_string())?;
    let socket_dir = prepare_socket_dir(&data_dir, &runtime.project)?;
    workflows
        .start_pinned_socket(
            sdk.app_name,
            &socket_dir.join(tako_compose::SDK_SOCKET_FILE),
            0o666,
        )
        .map_err(|e| format!("Could not start the Compose internal socket: {e}"))?;
    // Containers cannot reach local storage or databases served on the host.
    let storages: HashMap<_, _> = sdk
        .storages
        .iter()
        .filter(|(_, binding)| binding.provider == tako_core::StorageProvider::S3)
        .map(|(name, binding)| (name.clone(), binding.clone()))
        .collect();
    let envelope = tako_core::bootstrap::envelope_string(
        sdk.token,
        tako_core::bootstrap::Bindings {
            secrets: sdk.secrets,
            storages: &storages,
            databases: &HashMap::new(),
            kv: &HashMap::new(),
        },
    );
    let value = |key: &str, default: &str| {
        sdk.env
            .get(key)
            .cloned()
            .unwrap_or_else(|| default.to_string())
    };
    let data_dir = sdk
        .env
        .get("TAKO_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime.directory.join(".tako/data/app"));
    let base_name = sdk.app_name.split('/').next().unwrap_or(sdk.app_name);
    Ok(runtime.with_sdk(tako_compose::SdkRuntime {
        app_name: sdk.app_name.to_string(),
        token: sdk.token.to_string(),
        envelope,
        build: value("TAKO_BUILD", "dev"),
        environment: value("ENV", "development"),
        socket_dir,
        data_dir,
        health_host: format!("{base_name}.tako"),
    }))
}

/// `{dev data dir}/runtime/compose/<project>`. The root is 0700 and the
/// project directory 0711, so containers reach the socket through their mount
/// but other users of the machine cannot.
fn prepare_socket_dir(data_dir: &Path, project: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let root = data_dir.join("runtime").join("compose");
    let directory = root.join(project);
    std::fs::create_dir_all(&directory)
        .map_err(|e| format!("create Compose socket directory: {e}"))?;
    for (path, mode) in [(&root, 0o700), (&directory, 0o711)] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("protect Compose socket directory: {e}"))?;
    }
    Ok(directory)
}

/// A group with SDK services has a workflow runtime whose workers are its
/// own Compose services. Tako never spawns them.
pub(crate) async fn ensure_workflows(
    workflows: Option<&Arc<tako_workflows::WorkflowManager>>,
    runtime: Option<&tako_compose::ComposeRuntime>,
    retention_ms: Option<i64>,
) -> bool {
    let (Some(workflows), Some(sdk)) = (workflows, runtime.and_then(|r| r.sdk())) else {
        return false;
    };
    let app = sdk.app_name.clone();
    let build = sdk.build.clone();
    if let Err(error) = workflows
        .ensure(&app, retention_ms, |_| {
            tako_workflows::WorkerSpec::external(&app, &build)
        })
        .await
    {
        tracing::warn!(app = %app, %error, "failed to register Compose workflows");
    }
    true
}

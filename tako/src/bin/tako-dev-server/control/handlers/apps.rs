use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

use crate::process::{
    app_name_for, broadcast_app_status, broadcast_dev_event, forward_child_log_line,
    kill_app_process, monitor_handoff_pid, push_scoped_log, push_user_action,
    spawn_and_monitor_app,
};
use crate::protocol::{self, Response};
use crate::route_pattern::split_route_pattern;
use crate::state::{self, RuntimeApp};
use crate::{advertised_https_port, app_short_host, default_hosts};

use super::super::state::State;

pub(super) struct RegisterAppArgs {
    pub(super) config_path: String,
    pub(super) project_dir: String,
    pub(super) app_name: String,
    pub(super) variant: Option<String>,
    pub(super) hosts: Vec<String>,
    pub(super) command: Vec<String>,
    pub(super) env: HashMap<String, String>,
    pub(super) secrets: HashMap<String, String>,
    pub(super) images: Box<tako_images::ImagesConfig>,
    pub(super) storages: HashMap<String, tako_core::StorageBinding>,
    pub(super) client_pid: Option<u32>,
    pub(super) readiness_failure_hint: Option<String>,
    pub(super) worker_command: Option<Vec<String>>,
}

pub(super) async fn register_app(
    state: Arc<Mutex<State>>,
    args: RegisterAppArgs,
) -> Result<Response, Box<dyn std::error::Error + Send + Sync>> {
    let RegisterAppArgs {
        config_path,
        project_dir,
        app_name,
        variant,
        hosts,
        command,
        env,
        secrets,
        images,
        storages,
        client_pid,
        readiness_failure_hint,
        worker_command,
    } = args;

    let app_name = sanitize_app_name(&app_name);
    let route_id = format!("reg:{}", config_path);

    let existing_bootstrap = {
        let s = state.lock().unwrap();
        s.apps
            .get(&config_path)
            .map(|app| app.bootstrap_token.clone())
    };
    let bootstrap_token = match existing_bootstrap {
        Some(token) => token,
        None => generate_dev_secret()
            .map_err(|e| format!("failed to generate dev bootstrap token: {e}"))?,
    };

    let app_storages = storages.clone();
    let app_secrets = secrets.clone();

    {
        let s = state.lock().unwrap();
        if let Some(existing) = s.apps.get(&config_path)
            && let Some(pid) = existing.pid
        {
            kill_app_process(pid);
        }
    }

    let (url, workflows, internal_socket, worker_log_buffer, worker_app_root, old_app_name) = {
        let mut s = state.lock().unwrap();
        s.cancel_idle_exit();
        let old_app_name = s.apps.get(&config_path).map(|app| app.name.clone());
        let old_hosts = s
            .apps
            .get(&config_path)
            .map(|app| app.hosts.clone())
            .unwrap_or_default();

        let hosts = if hosts.is_empty() {
            default_hosts(&app_name)
        } else {
            hosts
        };

        if let Some(db) = &s.db {
            let _ = db.register(&config_path, &project_dir, &app_name, variant.as_deref());
        }

        let log_buffer = s
            .apps
            .get(&config_path)
            .map(|a| {
                a.log_buffer.clear();
                a.log_buffer.clone()
            })
            .unwrap_or_else(state::LogBuffer::new);

        // Preserve the previously-reported port across re-registration so the
        // proxy keeps routing correctly until the next readiness signal.
        let upstream_port = s
            .apps
            .get(&config_path)
            .map(|a| a.upstream_port)
            .unwrap_or(0);

        s.apps.insert(
            config_path.clone(),
            RuntimeApp {
                project_dir: project_dir.clone(),
                name: app_name.clone(),
                variant: variant.clone(),
                hosts: hosts.clone(),
                upstream_port,
                is_idle: false,
                command,
                worker_command: worker_command.clone(),
                env,
                log_buffer,
                pid: None,
                client_pid,
                tunnel: None,
                readiness_failure_hint,
                bootstrap_token,
                secrets,
                storages,
            },
        );

        s.routes.set_routes_with_images_and_channel_store_key(
            route_id,
            hosts.clone(),
            upstream_port,
            false,
            (*images).clone(),
            app_name.clone(),
        );
        if let Some(ref mut mdns) = s.mdns {
            for host in &old_hosts {
                mdns.unpublish(split_route_pattern(host).0);
            }
            for host in &hosts {
                mdns.publish(split_route_pattern(host).0);
            }
        }
        let host = hosts
            .first()
            .cloned()
            .unwrap_or_else(|| app_short_host(&app_name));
        let public_port = advertised_https_port(&s);
        let url = if public_port == 443 {
            format!("https://{}/", host)
        } else {
            format!("https://{}:{}/", host, public_port)
        };
        let worker_log_buffer = s.apps.get(&config_path).map(|a| a.log_buffer.clone());
        let worker_app_root = s
            .apps
            .get(&config_path)
            .and_then(|a| a.env.get("TAKO_APP_ROOT").cloned());
        (
            url,
            s.workflows.clone(),
            s.internal_socket.clone(),
            worker_log_buffer,
            worker_app_root,
            old_app_name,
        )
    };

    ensure_workflow_runtime(
        workflows.clone(),
        internal_socket,
        &app_name,
        &project_dir,
        worker_command.as_deref(),
        worker_app_root.as_deref(),
        app_storages,
        app_secrets,
        worker_log_buffer,
    )
    .await;
    if let (Some(workflows), Some(old_app_name)) = (workflows, old_app_name)
        && old_app_name != app_name
    {
        workflows.stop(&old_app_name, Duration::from_secs(1)).await;
    }

    spawn_registered_app_process(state, config_path.clone());

    Ok(Response::AppRegistered {
        app_name,
        config_path,
        project_dir,
        url,
    })
}

#[allow(clippy::too_many_arguments)]
async fn ensure_workflow_runtime(
    workflows: Option<Arc<tako_workflows::WorkflowManager>>,
    internal_socket: Option<PathBuf>,
    app_name: &str,
    project_dir: &str,
    worker_command: Option<&[String]>,
    worker_app_root: Option<&str>,
    storages: HashMap<String, tako_core::StorageBinding>,
    secrets: HashMap<String, String>,
    log_buffer: Option<state::LogBuffer>,
) {
    let Some(workflows) = workflows else {
        return;
    };
    let Some(worker_cmd) = worker_command else {
        workflows.stop(app_name, Duration::from_secs(1)).await;
        return;
    };
    if worker_cmd.is_empty() {
        workflows.stop(app_name, Duration::from_secs(1)).await;
        return;
    }

    // Register workflow workers before spawning the app process, so the first
    // SDK workflow enqueue/channel publish hits a known app on the internal socket.
    let Some(socket) = internal_socket else {
        return;
    };

    let app = app_name.to_string();
    let cwd = PathBuf::from(project_dir);
    let app_root = worker_app_root.map(str::to_string);
    let cmd_os: Vec<OsString> = worker_cmd.iter().map(OsString::from).collect();
    let log_sink: Option<tako_workflows::WorkerLogSink> = log_buffer.map(|buf| {
        std::sync::Arc::new(move |line: &str, is_stderr: bool| {
            let level = if is_stderr { "warn" } else { "info" };
            forward_child_log_line(&buf, line.to_string(), level, "worker");
        }) as tako_workflows::WorkerLogSink
    });
    let spec_app = app.clone();
    let spec_fn = move |_db_path: PathBuf| tako_workflows::WorkerSpec {
        env: build_worker_env(&spec_app, &cwd, &socket, app_root.as_deref()),
        app: spec_app.clone(),
        workers: 0,
        concurrency: 500,
        idle_timeout_ms: 3_000,
        command: cmd_os,
        cwd,
        secrets,
        storages,
        log_sink,
        isolation: None,
    };
    if let Err(e) = workflows.ensure(&app, spec_fn).await {
        tracing::warn!(
            app = %app,
            error = %e,
            "failed to register app with workflow manager; workflows / channel publish will not work",
        );
    }
}

pub(super) async fn unregister_app(state: &Arc<Mutex<State>>, config_path: String) -> Response {
    let (app_name, workflows) = {
        let mut s = state.lock().unwrap();

        if let Some(app) = s.apps.get(&config_path)
            && let Some(pid) = app.pid
        {
            kill_app_process(pid);
            state::remove_pid_file(&app.project_dir, &config_path);
        }

        let app_name = if let Some(mut app) = s.apps.remove(&config_path) {
            if let Some(ref mut mdns) = s.mdns {
                for host in &app.hosts {
                    mdns.unpublish(split_route_pattern(host).0);
                }
            }
            if let Some(tunnel) = app.tunnel.take() {
                tunnel.abort_handle.abort();
                s.events.broadcast(Response::Event {
                    event: protocol::DevEvent::TunnelModeChanged {
                        config_path: config_path.clone(),
                        app_name: app.name.clone(),
                        enabled: false,
                        url: None,
                        expires_at: None,
                        close_reason: Some(protocol::TunnelCloseReason::Shutdown),
                    },
                });
            }
            app.name
        } else {
            String::new()
        };

        let route_id = format!("reg:{}", config_path);
        s.routes.remove_app(&route_id);

        if !app_name.is_empty() {
            s.events.broadcast(Response::Event {
                event: protocol::DevEvent::AppStatusChanged {
                    config_path: config_path.clone(),
                    app_name: app_name.clone(),
                    status: "stopped".to_string(),
                },
            });
        }

        if s.apps.is_empty() {
            s.schedule_idle_exit();
        }

        (app_name, s.workflows.clone())
    };
    if let Some(workflows) = workflows
        && !app_name.is_empty()
    {
        workflows.stop(&app_name, Duration::from_secs(1)).await;
    }

    Response::AppUnregistered { config_path }
}

pub(super) async fn restart_app(state: &Arc<Mutex<State>>, config_path: String) -> Response {
    let restart = {
        let s = state.lock().unwrap();
        s.apps.get(&config_path).map(|app| {
            (
                s.workflows.clone(),
                s.internal_socket.clone(),
                app.name.clone(),
                app.project_dir.clone(),
                app.worker_command.clone(),
                app.env.get("TAKO_APP_ROOT").cloned(),
                app.storages.clone(),
                app.secrets.clone(),
                app.log_buffer.clone(),
            )
        })
    };

    {
        let mut s = state.lock().unwrap();
        if let Some(app) = s.apps.get_mut(&config_path) {
            if let Some(pid) = app.pid.take() {
                kill_app_process(pid);
                state::remove_pid_file(&app.project_dir, &config_path);
            }
            app.is_idle = true;
        }
    }

    if let Some((
        workflows,
        internal_socket,
        app_name,
        project_dir,
        worker_command,
        app_root,
        storages,
        secrets,
        log_buffer,
    )) = restart
    {
        ensure_workflow_runtime(
            workflows,
            internal_socket,
            &app_name,
            &project_dir,
            worker_command.as_deref(),
            app_root.as_deref(),
            storages,
            secrets,
            Some(log_buffer),
        )
        .await;
    }

    let log_buffer = {
        let s = state.lock().unwrap();
        s.apps.get(&config_path).map(|a| a.log_buffer.clone())
    };
    if let Some(ref buf) = log_buffer {
        push_user_action(buf, "restarted");
    }

    spawn_restarted_app_process(Arc::clone(state), config_path.clone());

    Response::AppRestarting { config_path }
}

pub(super) fn set_app_status(
    state: &Arc<Mutex<State>>,
    config_path: String,
    status: String,
) -> Result<Response, Response> {
    let is_idle = match status.as_str() {
        "idle" => true,
        "running" => false,
        _ => {
            return Err(Response::Error {
                message: format!("unknown status: {status}"),
            });
        }
    };

    let mut s = state.lock().unwrap();
    let route_id = format!("reg:{}", config_path);
    s.routes.set_active(&route_id, !is_idle);

    let app_name = if let Some(app) = s.apps.get_mut(&config_path) {
        app.is_idle = is_idle;
        app.name.clone()
    } else {
        String::new()
    };

    if !app_name.is_empty() {
        s.events.broadcast(Response::Event {
            event: protocol::DevEvent::AppStatusChanged {
                config_path: config_path.clone(),
                app_name,
                status: status.clone(),
            },
        });
    }

    Ok(Response::AppStatusUpdated {
        config_path,
        status,
    })
}

pub(super) fn handoff_app(state: &Arc<Mutex<State>>, config_path: String, pid: u32) -> Response {
    let mut s = state.lock().unwrap();
    let project_dir = if let Some(app) = s.apps.get_mut(&config_path) {
        app.pid = Some(pid);
        app.is_idle = false;
        app.project_dir.clone()
    } else {
        String::new()
    };
    if !project_dir.is_empty() {
        state::write_pid_file(&project_dir, &config_path, pid);
    }

    let state_for_monitor = Arc::clone(state);
    let config_for_monitor = config_path.clone();
    let dir_for_monitor = project_dir.clone();
    tokio::spawn(async move {
        monitor_handoff_pid(state_for_monitor, config_for_monitor, dir_for_monitor, pid).await;
    });

    Response::AppHandedOff { config_path }
}

fn sanitize_app_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if (c == '_' || c == '.' || c == '-') && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.starts_with('-') || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.remove(0);
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "app".to_string()
    } else {
        out
    }
}

fn generate_dev_secret() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(super) fn build_worker_env(
    app: &str,
    project_dir: &Path,
    internal_socket: &Path,
    app_root: Option<&str>,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert(
        tako_core::instance_env::TAKO_APP_NAME_ENV.into(),
        app.to_string(),
    );
    env.insert(
        tako_core::instance_env::TAKO_INTERNAL_SOCKET_ENV.into(),
        internal_socket.to_string_lossy().to_string(),
    );
    env.insert(
        "TAKO_DATA_DIR".into(),
        project_dir
            .join(".tako")
            .join("data")
            .join("app")
            .to_string_lossy()
            .to_string(),
    );
    if let Some(app_root) = app_root {
        env.insert("TAKO_APP_ROOT".into(), app_root.to_string());
    }
    env
}

fn spawn_registered_app_process(state: Arc<Mutex<State>>, config_path: String) {
    tokio::spawn(async move {
        match spawn_and_monitor_app(Arc::clone(&state), &config_path).await {
            Ok(pid) => {
                tracing::info!(config_path = %config_path, pid = pid, "spawned app process");
                broadcast_dev_event(
                    &state,
                    protocol::DevEvent::AppReady {
                        config_path: config_path.clone(),
                        app_name: app_name_for(&state, &config_path),
                    },
                );
                broadcast_app_status(&state, &config_path, "running");
            }
            Err(e) => {
                tracing::warn!(config_path = %config_path, error = %e, "failed to spawn app");
                let log_buffer = {
                    let s = state.lock().unwrap();
                    s.apps.get(&config_path).map(|a| a.log_buffer.clone())
                };
                broadcast_app_status(&state, &config_path, "idle");
                let msg = format!("failed to start app: {e}");
                if let Some(buf) = log_buffer {
                    push_scoped_log(&buf, "Error", "tako", &msg);
                }
                broadcast_dev_event(
                    &state,
                    protocol::DevEvent::AppError {
                        config_path: config_path.clone(),
                        app_name: app_name_for(&state, &config_path),
                        message: msg,
                    },
                );
            }
        }
    });
}

fn spawn_restarted_app_process(state: Arc<Mutex<State>>, config_path: String) {
    tokio::spawn(async move {
        match spawn_and_monitor_app(Arc::clone(&state), &config_path).await {
            Ok(pid) => {
                tracing::info!(config_path = %config_path, pid = pid, "restarted app process");
                broadcast_dev_event(
                    &state,
                    protocol::DevEvent::AppReady {
                        config_path: config_path.clone(),
                        app_name: app_name_for(&state, &config_path),
                    },
                );
                broadcast_app_status(&state, &config_path, "running");
            }
            Err(e) => {
                tracing::warn!(config_path = %config_path, error = %e, "failed to restart app");
                let log_buffer = {
                    let s = state.lock().unwrap();
                    s.apps.get(&config_path).map(|a| a.log_buffer.clone())
                };
                let msg = format!("restart failed: {e}");
                if let Some(buf) = log_buffer {
                    push_scoped_log(&buf, "Error", "tako", &msg);
                }
                broadcast_dev_event(
                    &state,
                    protocol::DevEvent::AppError {
                        config_path: config_path.clone(),
                        app_name: app_name_for(&state, &config_path),
                        message: msg,
                    },
                );
            }
        }
    });
}

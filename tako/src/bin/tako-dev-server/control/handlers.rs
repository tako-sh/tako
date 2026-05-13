use std::sync::{Arc, Mutex};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::UnixStream;

use crate::process::{
    app_name_for, broadcast_app_status, broadcast_dev_event, forward_child_log_line,
    kill_app_process, monitor_handoff_pid, push_scoped_log, push_user_action,
    spawn_and_monitor_app,
};
use crate::protocol::{self, AppInfo, Request, Response};
use crate::route_pattern::split_route_pattern;
use crate::state;
use crate::state::RuntimeApp;
use crate::{advertised_https_port, app_short_host, default_hosts};
use tako_socket::{read_json_line, write_json_line};

use super::lan::{handle_toggle_lan, write_lan_mode_log};
use super::state::{ControlClientSubscription, State};

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

/// Build the env map injected into a dev-mode workflow worker child process.
/// The supervisor runs with `env_clear()`, so anything the worker needs at
/// runtime must be named here — most notably `TAKO_DATA_DIR`, which the SDK
/// surfaces as `tako.dataDir` from `tako.sh` (and `process.env.TAKO_DATA_DIR`) and is required
/// whenever app code opens files under the per-app data directory.
fn build_worker_env(
    app: &str,
    project_dir: &std::path::Path,
    internal_socket: &std::path::Path,
    app_root: Option<&str>,
) -> std::collections::HashMap<String, String> {
    let mut env = std::collections::HashMap::new();
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

pub(crate) async fn handle_client(
    stream: UnixStream,
    state: Arc<Mutex<State>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    loop {
        let Some(req) = (match read_json_line::<_, Request>(&mut r).await {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                write_resp(
                    &mut w,
                    &Response::Error {
                        message: format!("invalid request: {}", e),
                    },
                )
                .await?;
                continue;
            }
            Err(e) => return Err(e.into()),
        }) else {
            break;
        };

        let resp = match req {
            Request::Ping => Response::Pong,
            Request::SubscribeEvents => {
                let rx = {
                    let s = state.lock().unwrap();
                    s.events.subscribe()
                };

                let _control_client = ControlClientSubscription::register(&state);
                let mut rx = rx;
                if write_resp(&mut w, &Response::Subscribed).await.is_err() {
                    return Ok(());
                }
                let mut disconnect_probe = [0_u8; 1];
                loop {
                    tokio::select! {
                        maybe_resp = rx.recv() => {
                            let Some(resp) = maybe_resp else {
                                break;
                            };
                            if write_resp(&mut w, &resp).await.is_err() {
                                break;
                            }
                        }
                        read_result = r.read(&mut disconnect_probe) => {
                            match read_result {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    }
                }
                return Ok(());
            }
            Request::SubscribeLogs { config_path, after } => {
                let log_buffer = {
                    let s = state.lock().unwrap();
                    s.apps.get(&config_path).map(|a| a.log_buffer.clone())
                };

                let Some(log_buffer) = log_buffer else {
                    write_resp(
                        &mut w,
                        &Response::Error {
                            message: format!("app not found: {config_path}"),
                        },
                    )
                    .await?;
                    continue;
                };

                let _control_client = ControlClientSubscription::register(&state);
                let (backlog, mut rx, truncated) = log_buffer.subscribe(after);

                if write_resp(&mut w, &Response::LogsSubscribed).await.is_err() {
                    return Ok(());
                }
                if truncated && write_resp(&mut w, &Response::LogsTruncated).await.is_err() {
                    return Ok(());
                }

                for entry in backlog {
                    if write_resp(
                        &mut w,
                        &Response::LogEntry {
                            id: entry.id,
                            line: entry.line,
                        },
                    )
                    .await
                    .is_err()
                    {
                        return Ok(());
                    }
                }

                let mut disconnect_probe = [0_u8; 1];
                loop {
                    tokio::select! {
                        maybe_entry = rx.recv() => {
                            let Some(entry) = maybe_entry else {
                                break;
                            };
                            if write_resp(
                                &mut w,
                                &Response::LogEntry {
                                    id: entry.id,
                                    line: entry.line,
                                },
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                        }
                        read_result = r.read(&mut disconnect_probe) => {
                            match read_result {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    }
                }
                return Ok(());
            }
            Request::RegisterApp {
                config_path,
                project_dir,
                app_name,
                variant,
                hosts,
                command,
                env,
                images,
                client_pid,
                readiness_failure_hint,
                worker_command,
            } => {
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

                {
                    let s = state.lock().unwrap();
                    if let Some(existing) = s.apps.get(&config_path)
                        && let Some(pid) = existing.pid
                    {
                        kill_app_process(pid);
                    }
                }

                // Everything before the await must happen under the
                // `std::sync::Mutex` guard; scope the guard so the
                // compiler sees it dropped before we `await` below.
                let (url, workflows, internal_socket, worker_log_buffer, worker_app_root) = {
                    let mut s = state.lock().unwrap();
                    s.cancel_idle_exit();
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
                        let _ =
                            db.register(&config_path, &project_dir, &app_name, variant.as_deref());
                    }

                    let log_buffer = s
                        .apps
                        .get(&config_path)
                        .map(|a| {
                            a.log_buffer.clear();
                            a.log_buffer.clone()
                        })
                        .unwrap_or_else(state::LogBuffer::new);
                    let lan_banner_buffer = log_buffer.clone();

                    // Preserve the previously-reported port across re-registration
                    // so the proxy keeps routing correctly until the next readiness
                    // signal. New registrations start at 0 and get filled in when
                    // the SDK writes its bound port to the readiness pipe.
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
                            env,
                            log_buffer,
                            pid: None,
                            client_pid,
                            readiness_failure_hint,
                            bootstrap_token,
                        },
                    );

                    s.routes.set_routes_with_images(
                        route_id,
                        hosts.clone(),
                        upstream_port,
                        false,
                        (*images).clone(),
                    );
                    if let Some(ref mut mdns) = s.mdns {
                        for host in &old_hosts {
                            mdns.unpublish(split_route_pattern(host).0);
                        }
                        for host in &hosts {
                            mdns.publish(split_route_pattern(host).0);
                        }
                    }
                    if s.lan_enabled {
                        write_lan_mode_log([lan_banner_buffer], true, s.lan_ip.as_deref());
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
                    )
                };

                // Register the app with the workflow manager before
                // spawning the app process, so the very first
                // workflow `.enqueue()` / channel `.publish()` from user
                // code lands on a known app (prevents
                // `unknown app: <name>` errors on the internal socket).
                //
                // Dev uses scale-to-zero (`workers: 0`) with a short idle
                // timeout: the worker subprocess is spawned on enqueue,
                // processes the queue, and exits after ~3s idle. Every
                // wake re-execs the worker, so source edits are picked up
                // with no watcher or `--hot` involved.
                if let (Some(workflows), Some(worker_cmd), Some(socket)) =
                    (workflows, worker_command, internal_socket)
                    && !worker_cmd.is_empty()
                {
                    let app = app_name.clone();
                    let cwd = std::path::PathBuf::from(&project_dir);
                    let app_root = worker_app_root.clone();
                    let cmd_os: Vec<std::ffi::OsString> =
                        worker_cmd.iter().map(std::ffi::OsString::from).collect();
                    let log_sink: Option<tako_workflows::WorkerLogSink> =
                        worker_log_buffer.map(|buf| {
                            std::sync::Arc::new(move |line: &str, is_stderr: bool| {
                                let level = if is_stderr { "warn" } else { "info" };
                                forward_child_log_line(&buf, line.to_string(), level, "worker");
                            }) as tako_workflows::WorkerLogSink
                        });
                    let spec_fn = move |_db_path: std::path::PathBuf| tako_workflows::WorkerSpec {
                        env: build_worker_env(&app, &cwd, &socket, app_root.as_deref()),
                        app: app.clone(),
                        workers: 0,
                        concurrency: 500,
                        idle_timeout_ms: 3_000,
                        command: cmd_os,
                        cwd,
                        secrets: std::collections::HashMap::new(),
                        log_sink,
                    };
                    if let Err(e) = workflows.ensure(&app_name, spec_fn).await {
                        tracing::warn!(
                            app = %app_name,
                            error = %e,
                            "failed to register app with workflow manager; workflows / channel publish will not work",
                        );
                    }
                }

                let spawn_state = state.clone();
                let spawn_config = config_path.clone();
                tokio::spawn(async move {
                    match spawn_and_monitor_app(spawn_state.clone(), &spawn_config).await {
                        Ok(pid) => {
                            tracing::info!(config_path = %spawn_config, pid = pid, "spawned app process");
                            broadcast_dev_event(
                                &spawn_state,
                                protocol::DevEvent::AppReady {
                                    config_path: spawn_config.clone(),
                                    app_name: app_name_for(&spawn_state, &spawn_config),
                                },
                            );
                            broadcast_app_status(&spawn_state, &spawn_config, "running");
                        }
                        Err(e) => {
                            tracing::warn!(config_path = %spawn_config, error = %e, "failed to spawn app");
                            let log_buffer = {
                                let s = spawn_state.lock().unwrap();
                                s.apps.get(&spawn_config).map(|a| a.log_buffer.clone())
                            };
                            broadcast_app_status(&spawn_state, &spawn_config, "idle");
                            let msg = format!("failed to start app: {e}");
                            if let Some(buf) = log_buffer {
                                push_scoped_log(&buf, "Error", "tako", &msg);
                            }
                            broadcast_dev_event(
                                &spawn_state,
                                protocol::DevEvent::AppError {
                                    config_path: spawn_config.clone(),
                                    app_name: app_name_for(&spawn_state, &spawn_config),
                                    message: msg,
                                },
                            );
                        }
                    }
                });

                Response::AppRegistered {
                    app_name,
                    config_path,
                    project_dir,
                    url,
                }
            }
            Request::UnregisterApp { config_path } => {
                let mut s = state.lock().unwrap();

                if let Some(app) = s.apps.get(&config_path)
                    && let Some(pid) = app.pid
                {
                    kill_app_process(pid);
                    state::remove_pid_file(&app.project_dir, &config_path);
                }

                let app_name = if let Some(app) = s.apps.remove(&config_path) {
                    if let Some(ref mut mdns) = s.mdns {
                        for host in &app.hosts {
                            mdns.unpublish(split_route_pattern(host).0);
                        }
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

                Response::AppUnregistered { config_path }
            }
            Request::RestartApp { config_path } => {
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

                let log_buffer = {
                    let s = state.lock().unwrap();
                    s.apps.get(&config_path).map(|a| a.log_buffer.clone())
                };
                if let Some(ref buf) = log_buffer {
                    push_user_action(buf, "restarted");
                }

                let spawn_state = state.clone();
                let spawn_config = config_path.clone();
                tokio::spawn(async move {
                    match spawn_and_monitor_app(spawn_state.clone(), &spawn_config).await {
                        Ok(pid) => {
                            tracing::info!(config_path = %spawn_config, pid = pid, "restarted app process");
                            broadcast_dev_event(
                                &spawn_state,
                                protocol::DevEvent::AppReady {
                                    config_path: spawn_config.clone(),
                                    app_name: app_name_for(&spawn_state, &spawn_config),
                                },
                            );
                            broadcast_app_status(&spawn_state, &spawn_config, "running");
                        }
                        Err(e) => {
                            tracing::warn!(config_path = %spawn_config, error = %e, "failed to restart app");
                            let log_buffer = {
                                let s = spawn_state.lock().unwrap();
                                s.apps.get(&spawn_config).map(|a| a.log_buffer.clone())
                            };
                            let msg = format!("restart failed: {e}");
                            if let Some(buf) = log_buffer {
                                push_scoped_log(&buf, "Error", "tako", &msg);
                            }
                            broadcast_dev_event(
                                &spawn_state,
                                protocol::DevEvent::AppError {
                                    config_path: spawn_config.clone(),
                                    app_name: app_name_for(&spawn_state, &spawn_config),
                                    message: msg,
                                },
                            );
                        }
                    }
                });

                Response::AppRestarting { config_path }
            }
            Request::SetAppStatus {
                config_path,
                status,
            } => {
                let is_idle = match status.as_str() {
                    "idle" => true,
                    "running" => false,
                    _ => {
                        write_resp(
                            &mut w,
                            &Response::Error {
                                message: format!("unknown status: {status}"),
                            },
                        )
                        .await?;
                        continue;
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

                Response::AppStatusUpdated {
                    config_path,
                    status,
                }
            }
            Request::HandoffApp { config_path, pid } => {
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

                let state_for_monitor = state.clone();
                let config_for_monitor = config_path.clone();
                let dir_for_monitor = project_dir.clone();
                tokio::spawn(async move {
                    monitor_handoff_pid(
                        state_for_monitor,
                        config_for_monitor,
                        dir_for_monitor,
                        pid,
                    )
                    .await;
                });

                Response::AppHandedOff { config_path }
            }
            Request::ConnectClient {
                config_path,
                client_id,
            } => {
                let app_name = {
                    let s = state.lock().unwrap();
                    let name = s
                        .apps
                        .get(&config_path)
                        .map(|a| a.name.clone())
                        .unwrap_or_default();
                    s.events.broadcast(Response::Event {
                        event: protocol::DevEvent::ClientConnected {
                            config_path: config_path.clone(),
                            app_name: name.clone(),
                            client_id,
                        },
                    });
                    name
                };

                if write_resp(&mut w, &Response::Pong).await.is_err() {
                    return Ok(());
                }

                let mut probe = [0_u8; 1];
                loop {
                    match r.read(&mut probe).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }

                {
                    let s = state.lock().unwrap();
                    s.events.broadcast(Response::Event {
                        event: protocol::DevEvent::ClientDisconnected {
                            config_path,
                            app_name,
                            client_id,
                        },
                    });
                }

                return Ok(());
            }
            Request::ListRegisteredApps => {
                let s = state.lock().unwrap();
                let apps = s
                    .apps
                    .iter()
                    .map(|(config_path, a)| protocol::RegisteredAppInfo {
                        config_path: config_path.clone(),
                        project_dir: a.project_dir.clone(),
                        app_name: a.name.clone(),
                        variant: a.variant.clone(),
                        hosts: a.hosts.clone(),
                        upstream_port: a.upstream_port,
                        status: if a.is_idle { "idle" } else { "running" }.to_string(),
                        pid: a.pid,
                        client_pid: a.client_pid,
                    })
                    .collect();
                Response::RegisteredApps { apps }
            }
            Request::ListApps => {
                let s = state.lock().unwrap();
                let apps = s
                    .apps
                    .values()
                    .map(|a| AppInfo {
                        app_name: a.name.clone(),
                        variant: a.variant.clone(),
                        hosts: a.hosts.clone(),
                        upstream_port: a.upstream_port,
                        pid: a.pid,
                    })
                    .collect();
                Response::Apps { apps }
            }
            Request::Info => {
                let s = state.lock().unwrap();
                Response::Info {
                    info: protocol::DevInfo {
                        listen: s.listen_addr.clone(),
                        port: advertised_https_port(&s),
                        advertised_ip: s.advertised_ip.clone(),
                        local_dns_enabled: s.local_dns_enabled,
                        local_dns_port: s.local_dns_port,
                        control_clients: s.control_clients,
                        lan_enabled: s.lan_enabled,
                        lan_ip: s.lan_ip.clone(),
                    },
                }
            }
            Request::ToggleLan { enabled } => handle_toggle_lan(&state, enabled).await,
            Request::StopServer => {
                let s = state.lock().unwrap();
                let _ = s.shutdown_tx.send(true);
                Response::Stopping
            }
        };

        write_resp(&mut w, &resp).await?;
    }

    Ok(())
}

async fn write_resp(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &Response,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    write_json_line(w, resp).await?;
    Ok(())
}

#[cfg(test)]
mod tests;

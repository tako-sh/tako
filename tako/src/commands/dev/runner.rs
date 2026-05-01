use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::sync::watch;

use super::prepare::{DevSession, PrepareOutcome, prepare};
use super::*;

pub(super) fn bootstrap_dev_events(status: &str, pid: Option<u32>) -> Vec<DevEvent> {
    match (status, pid) {
        ("running", Some(pid)) => vec![DevEvent::AppPid(pid), DevEvent::AppReady],
        ("idle" | "stopped", _) => vec![DevEvent::AppStopped],
        _ => Vec::new(),
    }
}

/// Run the dev server
pub async fn stop(
    name: Option<String>,
    all: bool,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let apps = crate::dev_server_client::list_registered_apps().await?;

    if all {
        if apps.is_empty() {
            crate::output::muted("No registered dev apps.");
            return Ok(());
        }
        for app in &apps {
            let _ = crate::dev_server_client::unregister_app(&app.config_path).await;
            crate::output::success(&format!("Stopped {}", crate::output::strong(&app.app_name)));
        }
        return Ok(());
    }

    let target_name = match name {
        Some(n) => n,
        None => {
            let context = crate::commands::project_context::resolve(config_path)?;
            let config_key = context.config_key();
            if let Some(app) = apps.iter().find(|a| a.config_path == config_key) {
                let _ = crate::dev_server_client::unregister_app(&app.config_path).await;
                crate::output::success(&format!(
                    "Stopped {}",
                    crate::output::strong(&app.app_name)
                ));
                return Ok(());
            }
            resolve_app_name_from_config_path(&context.config_path)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?
        }
    };

    let app = apps.iter().find(|a| a.app_name == target_name);
    match app {
        Some(a) => {
            let _ = crate::dev_server_client::unregister_app(&a.config_path).await;
            crate::output::success(&format!("Stopped {}", crate::output::strong(&a.app_name)));
        }
        None => {
            return Err(format!("No registered dev app named '{}'", target_name).into());
        }
    }
    Ok(())
}

pub async fn ls() -> Result<(), Box<dyn std::error::Error>> {
    let apps = match crate::dev_server_client::list_registered_apps().await {
        Ok(apps) => apps,
        Err(_) => {
            crate::output::muted("No dev server running.");
            return Ok(());
        }
    };

    if apps.is_empty() {
        crate::output::muted("No registered dev apps.");
        return Ok(());
    }

    println!("{:<20} {:<10} {:<30} CONFIG", "NAME", "STATUS", "URL");
    for app in &apps {
        let url = if let Some(host) = app.hosts.first() {
            format!("https://{}/", host)
        } else {
            String::new()
        };
        println!(
            "{:<20} {:<10} {:<30} {}",
            app.app_name, app.status, url, app.config_path
        );
    }
    Ok(())
}

pub async fn run(
    public_port: u16,
    variant: Option<String>,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let session = match prepare(public_port, variant, config_path).await? {
        PrepareOutcome::Ready(s) => *s,
        PrepareOutcome::AlreadyConnected => return Ok(()),
    };

    let DevSession {
        config_key,
        config_path,
        project_dir,
        app_name,
        variant,
        runtime_name,
        domain,
        base_domain,
        primary_host,
        public_port,
        public_url_port,
        cfg,
        cmd,
        readiness_failure_hint,
        worker_command,
        dev_hosts,
        env,
        interactive,
    } = session;

    let hosts_state = Arc::new(tokio::sync::Mutex::new(dev_hosts.clone()));
    let env_state = Arc::new(tokio::sync::Mutex::new(env));

    let (log_tx, log_rx) = mpsc::channel::<ScopedLog>(1000);
    let (event_tx, event_rx) = mpsc::channel::<DevEvent>(100);
    let (control_tx, mut control_rx) = mpsc::channel::<output::ControlCmd>(32);
    let (should_exit_tx, mut should_exit_rx) = watch::channel(false);
    let terminate_requested = Arc::new(AtomicBool::new(false));

    let mut log_rx_opt = Some(log_rx);
    let mut event_rx_opt = Some(event_rx);
    let mut output_handle: Option<tokio::task::JoinHandle<Result<output::DevOutputExit, String>>> =
        None;

    {
        let config_key = config_key.clone();
        let event_tx = event_tx.clone();
        let should_exit_tx = should_exit_tx.clone();
        let log_tx = log_tx.clone();

        let mut ev_rx = match crate::dev_server_client::subscribe_events().await {
            Ok(rx) => Some(rx),
            Err(e) => {
                let _ = log_tx
                    .send(ScopedLog::warn(
                        "tako",
                        format!("failed to subscribe to dev server events: {}", e),
                    ))
                    .await;
                None
            }
        };

        if let Some(mut ev_rx) = ev_rx.take() {
            tokio::spawn(async move {
                use crate::dev_server_client::DevServerEvent;
                while let Some(ev) = ev_rx.recv().await {
                    match ev {
                        DevServerEvent::AppStatusChanged {
                            ref config_path,
                            ref status,
                            ..
                        } => {
                            if config_path == &config_key && status == "stopped" {
                                let _ = event_tx
                                    .send(DevEvent::ExitWithMessage(
                                        "stopped by another client".to_string(),
                                    ))
                                    .await;
                                let _ = should_exit_tx.send(true);
                                break;
                            }
                        }
                        DevServerEvent::ClientConnected {
                            ref config_path,
                            client_id,
                            ..
                        } => {
                            if config_path == &config_key {
                                let _ = event_tx
                                    .send(DevEvent::ClientConnected {
                                        is_self: false,
                                        client_id,
                                    })
                                    .await;
                            }
                        }
                        DevServerEvent::ClientDisconnected {
                            ref config_path,
                            client_id,
                            ..
                        } => {
                            if config_path == &config_key {
                                let _ = event_tx
                                    .send(DevEvent::ClientDisconnected { client_id })
                                    .await;
                            }
                        }
                        DevServerEvent::AppLaunching {
                            ref config_path, ..
                        } if config_path == &config_key => {
                            let _ = event_tx.send(DevEvent::AppLaunching).await;
                        }
                        DevServerEvent::AppStarted {
                            ref config_path, ..
                        } if config_path == &config_key => {
                            let _ = event_tx.send(DevEvent::AppStarted).await;
                        }
                        DevServerEvent::AppReady {
                            ref config_path, ..
                        } if config_path == &config_key => {
                            let _ = event_tx.send(DevEvent::AppReady).await;
                        }
                        DevServerEvent::AppPid {
                            ref config_path,
                            pid,
                            ..
                        } if config_path == &config_key => {
                            let _ = event_tx.send(DevEvent::AppPid(pid)).await;
                        }
                        DevServerEvent::AppProcessExited {
                            ref config_path,
                            ref message,
                            ..
                        } if config_path == &config_key => {
                            let _ = event_tx
                                .send(DevEvent::AppProcessExited(message.clone()))
                                .await;
                        }
                        DevServerEvent::AppError {
                            ref config_path,
                            ref message,
                            ..
                        } if config_path == &config_key => {
                            let _ = event_tx.send(DevEvent::AppError(message.clone())).await;
                        }
                        DevServerEvent::LanModeChanged {
                            enabled,
                            ref lan_ip,
                            ref ca_url,
                        } => {
                            let _ = event_tx
                                .send(DevEvent::LanModeChanged {
                                    enabled,
                                    lan_ip: lan_ip.clone(),
                                    ca_url: ca_url.clone(),
                                })
                                .await;
                        }
                        _ => {}
                    }
                }
            });
        }
    }

    let reg_hosts = hosts_state.lock().await.clone();
    let env_snapshot = env_state.lock().await.clone();
    let project_dir_display = project_dir.to_string_lossy();
    let reg_url =
        crate::dev_server_client::register_app(crate::dev_server_client::RegisterAppRequest {
            config_path: &config_key,
            project_dir: &project_dir_display,
            app_name: &app_name,
            variant: variant.as_deref(),
            hosts: &reg_hosts,
            command: &cmd,
            env: &env_snapshot,
            readiness_failure_hint: readiness_failure_hint.as_deref(),
            worker_command: worker_command.as_deref(),
        })
        .await?;
    let initial_lan_enabled = crate::dev_server_client::info()
        .await
        .ok()
        .and_then(|info| {
            info.get("info")
                .and_then(|i| i.get("lan_enabled"))
                .and_then(|v| v.as_bool())
        })
        .unwrap_or(false);

    {
        let log_tx = log_tx.clone();
        let config_key = config_key.clone();
        tokio::spawn(async move {
            let Ok(mut rx) = crate::dev_server_client::subscribe_logs(&config_key, None).await
            else {
                return;
            };
            while let Some(entry) = rx.recv().await {
                match entry {
                    crate::dev_server_client::LogStreamEntry::Entry { line, .. } => {
                        if let Some(log) = parse_log_line(&line) {
                            let _ = log_tx.send(log).await;
                        }
                    }
                    crate::dev_server_client::LogStreamEntry::Truncated => {
                        let _ = log_tx
                            .send(ScopedLog::info("tako", "earlier logs trimmed"))
                            .await;
                    }
                }
            }
        });
    }

    if reg_hosts.iter().any(|h| {
        let host = h.split('/').next().unwrap_or(h);
        host.ends_with(&format!(".{}", crate::dev::TAKO_DEV_DOMAIN))
    }) && let Ok(info) = crate::dev_server_client::info().await
    {
        let local_dns_enabled = info
            .get("info")
            .and_then(|i| i.get("local_dns_enabled"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        if !local_dns_enabled {
            crate::output::warning("Local DNS is unavailable; .test hostnames may not resolve.");
            crate::output::muted("Run `tako doctor` for diagnostics.");
        }
    }

    if interactive {
        let public_port_for_output = public_url_port;
        let hosts = compute_display_routes(&cfg, &domain, base_domain.as_deref());
        let app_name_for_output = app_name.clone();
        let adapter_name_for_output = runtime_name.clone();
        let control_tx_for_output = control_tx.clone();

        let log_rx = log_rx_opt.take().unwrap();
        let event_rx = event_rx_opt.take().unwrap();
        output_handle = Some(tokio::spawn(async move {
            output::run_dev_output(
                app_name_for_output,
                adapter_name_for_output,
                hosts,
                public_port_for_output,
                log_rx,
                event_rx,
                control_tx_for_output,
            )
            .await
            .map_err(|e| e.to_string())
        }));
    }

    let verbose = crate::output::is_verbose();
    let url = preferred_public_url(&primary_host, &reg_url, public_port, public_url_port);
    if !interactive {
        for line in dev_startup_lines(
            verbose,
            &app_name,
            &runtime_name,
            &project_dir.join(&cmd[0]),
            &url,
        ) {
            println!("{}", line);
        }
    }

    let (cfg_tx, cfg_rx) = mpsc::channel::<watcher::WatchChange>(8);
    let _cfg_handle =
        watcher::ConfigWatcher::new(project_dir.clone(), config_path.clone(), cfg_tx)?.start()?;

    if verbose && !interactive {
        println!(
            "Starting server at {}…",
            dev_url(&primary_host, public_url_port)
        );
        println!("Press Ctrl+c or q to stop");
        println!();
    }

    {
        let config_key = config_key.clone();
        let log_tx = log_tx.clone();
        let should_exit_tx = should_exit_tx.clone();
        let terminate_requested = terminate_requested.clone();

        tokio::spawn(async move {
            let mut lan_enabled = initial_lan_enabled;
            while let Some(cmd_in) = control_rx.recv().await {
                match cmd_in {
                    output::ControlCmd::Restart => {
                        let result = crate::dev_server_client::restart_app(&config_key)
                            .await
                            .map_err(|e| e.to_string());
                        if let Err(msg) = result {
                            let _ = log_tx
                                .send(ScopedLog::error("tako", format!("restart failed: {}", msg)))
                                .await;
                        }
                    }
                    output::ControlCmd::Terminate => {
                        terminate_requested.store(true, Ordering::Relaxed);
                        let _ = crate::dev_server_client::unregister_app(&config_key).await;
                        let _ = should_exit_tx.send(true);
                        break;
                    }
                    output::ControlCmd::ToggleLan => {
                        let target = !lan_enabled;
                        let result = crate::dev_server_client::toggle_lan(target)
                            .await
                            .map_err(|e| e.to_string());
                        match result {
                            Ok((enabled, _, _)) => {
                                lan_enabled = enabled;
                            }
                            Err(msg) => {
                                let _ = log_tx
                                    .send(ScopedLog::error(
                                        "tako",
                                        format!("LAN toggle failed: {}", msg),
                                    ))
                                    .await;
                            }
                        }
                    }
                }
            }
        });
    }

    {
        let project_dir = project_dir.clone();
        let config_path = config_path.clone();
        let config_key = config_key.clone();
        let app_name = app_name.clone();
        let variant = variant.clone();
        let domain = domain.clone();
        let base_domain = base_domain.clone();
        let env_state = env_state.clone();
        let hosts_state = hosts_state.clone();
        let cmd = cmd.clone();
        let log_tx = log_tx.clone();
        let should_exit_tx = should_exit_tx.clone();
        let mut cfg_rx = cfg_rx;
        tokio::spawn(async move {
            while let Some(change) = cfg_rx.recv().await {
                if !config_path.exists() {
                    let _ = log_tx
                        .send(ScopedLog::error(
                            "tako",
                            format!(
                                "{} was removed — stopping dev server",
                                config_path.display()
                            ),
                        ))
                        .await;
                    let _ = should_exit_tx.send(true);
                    return;
                }
                let cfg = match load_dev_tako_toml(&config_path) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = log_tx
                            .send(ScopedLog::error("tako", format!("tako.toml error: {}", e)))
                            .await;
                        continue;
                    }
                };

                let mut new_env = compute_dev_env(&cfg);
                for warning in cfg.ignored_reserved_var_warnings() {
                    let _ = log_tx
                        .send(ScopedLog::warn("tako", format!("Validation: {}", warning)))
                        .await;
                }
                if let Err(msg) = inject_dev_data_dir(&project_dir, &mut new_env) {
                    let _ = log_tx
                        .send(ScopedLog::error(
                            "tako",
                            format!("Failed to prepare TAKO_DATA_DIR: {msg}"),
                        ))
                        .await;
                    continue;
                }

                if let Err(msg) =
                    inject_dev_secrets(&project_dir, &mut new_env).map_err(|e| e.to_string())
                {
                    let _ = log_tx
                        .send(ScopedLog::warn(
                            "tako",
                            format!("Failed to reload secrets: {}", msg),
                        ))
                        .await;
                }

                let _ = crate::build::js::write_typegen_support_files(&project_dir);

                *env_state.lock().await = new_env.clone();

                let new_hosts =
                    match compute_dev_hosts(&app_name, &cfg, &domain, base_domain.as_deref()) {
                        Ok(hosts) => hosts,
                        Err(msg) => {
                            let _ = log_tx
                                .send(ScopedLog::error(
                                    "tako",
                                    format!("tako.toml invalid routes: {}", msg),
                                ))
                                .await;
                            continue;
                        }
                    };
                let hosts_changed = {
                    let mut cur = hosts_state.lock().await;
                    let changed = *cur != new_hosts;
                    *cur = new_hosts.clone();
                    changed
                };

                if hosts_changed {
                    let project_dir_display = project_dir.to_string_lossy();
                    let reg_result = crate::dev_server_client::register_app(
                        crate::dev_server_client::RegisterAppRequest {
                            config_path: &config_key,
                            project_dir: &project_dir_display,
                            app_name: &app_name,
                            variant: variant.as_deref(),
                            hosts: &new_hosts,
                            command: &cmd,
                            env: &new_env,
                            readiness_failure_hint: readiness_failure_hint.as_deref(),
                            worker_command: worker_command.as_deref(),
                        },
                    )
                    .await
                    .map_err(|e| e.to_string());
                    if let Err(msg) = reg_result {
                        let _ = log_tx
                            .send(ScopedLog::warn(
                                "tako",
                                format!("failed to update routing: {}", msg),
                            ))
                            .await;
                    }
                } else {
                    let restart_reason = match change {
                        watcher::WatchChange::Config => "tako.toml changed, restarting…",
                        watcher::WatchChange::Secrets => "Secrets changed, restarting…",
                        watcher::WatchChange::Channels => "channels/ changed, restarting…",
                        watcher::WatchChange::Workflows => "workflows/ changed, restarting…",
                    };
                    let _ = log_tx.send(ScopedLog::info("tako", restart_reason)).await;
                    let _ = crate::dev_server_client::restart_app(&config_key).await;
                }
            }
        });
    }

    {
        let should_exit_tx_ctrlc = should_exit_tx.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = should_exit_tx_ctrlc.send(true);
                if verbose {
                    println!("\nShutting down…");
                }
            }
        });
    }
    #[cfg(unix)]
    {
        let should_exit_tx_term = should_exit_tx.clone();
        let terminate_requested = terminate_requested.clone();
        tokio::spawn(async move {
            if let Ok(mut sigterm) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                let _ = sigterm.recv().await;
                terminate_requested.store(true, Ordering::Relaxed);
                let _ = should_exit_tx_term.send(true);
                if verbose {
                    println!("\nTerminating…");
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
                    println!("\nDisconnected from terminal.");
                }
            }
        });
    }

    if interactive {
        if let Some(mut handle) = output_handle.take() {
            let mut dev_exit: Option<output::DevOutputExit> = None;
            tokio::select! {
                r = &mut handle => {
                    match r {
                        Ok(Ok(exit)) => dev_exit = Some(exit),
                        Ok(Err(msg)) => return Err(msg.into()),
                        Err(e) => return Err(format!("dev output task failed: {}", e).into()),
                    }
                }
                _ = async {
                    while should_exit_rx.changed().await.is_ok() {
                        if *should_exit_rx.borrow() {
                            break;
                        }
                    }
                } => {
                    match tokio::time::timeout(Duration::from_millis(500), &mut handle).await {
                        Ok(Ok(Ok(exit))) => dev_exit = Some(exit),
                        _ => {
                            handle.abort();
                            let _ = handle.await;
                        }
                    }
                }
            }

            if let Some(output::DevOutputExit::Disconnect { .. }) = dev_exit {
                return Ok(());
            }
        }
    } else {
        let mut log_rx = log_rx_opt
            .take()
            .expect("non-interactive should have log rx");
        let mut event_rx = event_rx_opt
            .take()
            .expect("non-interactive should have event rx");
        tokio::select! {
            _ = async {
                loop {
                    tokio::select! {
                        Some(log) = log_rx.recv() => {
                            println!(
                                "{} {:<5} [{}] {}",
                                log.timestamp, log.level, log.scope, log.message
                            );
                        }
                        Some(event) = event_rx.recv() => {
                            match event {
                                DevEvent::AppStarted => {}
                                DevEvent::AppReady => {
                                    println!("App started");
                                }
                                DevEvent::AppLaunching => {
                                    println!("Starting app…");
                                }
                                DevEvent::AppStopped => {
                                    println!("○ App stopped (idle)");
                                }
                                DevEvent::AppPid(pid) => {
                                    println!("App pid {}", pid);
                                }
                                DevEvent::AppProcessExited(_) => {}
                                DevEvent::AppError(e) => {
                                    eprintln!("App error: {}", e);
                                }
                                DevEvent::ClientConnected { is_self, client_id } => {
                                    if !is_self {
                                        println!("Client {} connected", client_id);
                                    }
                                }
                                DevEvent::ClientDisconnected { client_id } => {
                                    println!("Client {} disconnected", client_id);
                                }
                                DevEvent::LanModeChanged { enabled, lan_ip, .. } => {
                                    if enabled {
                                        if let Some(ip) = lan_ip {
                                            println!("LAN mode enabled — {}", ip);
                                        }
                                    } else {
                                        println!("LAN mode disabled");
                                    }
                                }
                                DevEvent::ExitWithMessage(msg) => {
                                    println!("{}", msg);
                                    break;
                                }
                            }
                        }
                    }
                }
            } => {}
            _ = async {
                while should_exit_rx.changed().await.is_ok() {
                    if *should_exit_rx.borrow() {
                        break;
                    }
                }
            } => {}
        }
    }

    let _ = crate::dev_server_client::unregister_app(&config_key).await;
    Ok(())
}

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::sync::watch;

mod control;
mod events;
mod reload;
mod signals;

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
        secrets,
        interactive,
    } = session;

    let app_root_dir = crate::build::js::js_app_root_dir(&project_dir, cfg.js_app_root());
    let hosts_state = Arc::new(tokio::sync::Mutex::new(dev_hosts.clone()));
    let env_state = Arc::new(tokio::sync::Mutex::new(env));

    let (log_tx, log_rx) = mpsc::channel::<ScopedLog>(1000);
    let (event_tx, event_rx) = mpsc::channel::<DevEvent>(100);
    let (control_tx, control_rx) = mpsc::channel::<output::ControlCmd>(32);
    let (should_exit_tx, mut should_exit_rx) = watch::channel(false);
    let terminate_requested = Arc::new(AtomicBool::new(false));

    let mut log_rx_opt = Some(log_rx);
    let mut event_rx_opt = Some(event_rx);
    let mut output_handle: Option<tokio::task::JoinHandle<Result<output::DevOutputExit, String>>> =
        None;

    events::spawn_dev_event_forwarder(
        config_key.clone(),
        event_tx.clone(),
        should_exit_tx.clone(),
        log_tx.clone(),
    )
    .await;

    let reg_hosts = hosts_state.lock().await.clone();
    let env_snapshot = env_state.lock().await.clone();
    let storages = load_dev_storages(&project_dir).unwrap_or_else(|error| {
        tracing::warn!("Failed to load development storages: {error}");
        std::collections::HashMap::new()
    });
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
            secrets: &secrets,
            images: &cfg.images,
            storages: &storages,
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

    events::spawn_log_forwarder(config_key.clone(), log_tx.clone());

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
    let _cfg_handle = watcher::ConfigWatcher::new(
        project_dir.clone(),
        app_root_dir.clone(),
        config_path.clone(),
        cfg_tx,
    )?
    .start()?;

    if verbose && !interactive {
        println!(
            "Starting server at {}…",
            dev_url(&primary_host, public_url_port)
        );
        println!("Press Ctrl+c or q to stop");
        println!();
    }

    control::spawn_control_loop(
        config_key.clone(),
        initial_lan_enabled,
        control_rx,
        log_tx.clone(),
        should_exit_tx.clone(),
        terminate_requested.clone(),
    );

    reload::spawn_config_reload_loop(
        reload::ConfigReloadLoop {
            project_dir: project_dir.clone(),
            config_path: config_path.clone(),
            config_key: config_key.clone(),
            app_name: app_name.clone(),
            variant: variant.clone(),
            domain: domain.clone(),
            base_domain: base_domain.clone(),
            env_state: env_state.clone(),
            hosts_state: hosts_state.clone(),
            command: cmd.clone(),
            log_tx: log_tx.clone(),
            should_exit_tx: should_exit_tx.clone(),
            readiness_failure_hint: readiness_failure_hint.clone(),
            worker_command: worker_command.clone(),
        },
        cfg_rx,
    );

    signals::spawn_signal_handlers(should_exit_tx.clone(), terminate_requested.clone(), verbose);

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
        let log_rx = log_rx_opt
            .take()
            .expect("non-interactive should have log rx");
        let event_rx = event_rx_opt
            .take()
            .expect("non-interactive should have event rx");
        events::run_non_interactive_output(log_rx, event_rx, should_exit_rx).await;
    }

    let _ = crate::dev_server_client::unregister_app(&config_key).await;
    Ok(())
}

fn load_dev_storages(
    project_dir: &std::path::Path,
) -> Result<std::collections::HashMap<String, tako_core::StorageBinding>, Box<dyn std::error::Error>>
{
    let config = crate::config::TakoToml::load_from_dir(project_dir)?;
    let secrets = crate::config::SecretsStore::load_from_dir(project_dir)?;
    crate::commands::storage::decrypt_storage_bindings(
        "development",
        &config,
        &secrets,
        Some(project_dir),
    )
}

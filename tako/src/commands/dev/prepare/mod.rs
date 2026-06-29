#[cfg(target_os = "linux")]
pub(super) mod linux;
pub(super) mod local;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
pub(super) mod tls;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;

/// All resolved state needed to start a dev session.
pub(super) struct DevSession {
    pub config_key: String,
    pub config_path: PathBuf,
    pub project_dir: PathBuf,
    pub app_name: String,
    pub variant: Option<String>,
    pub runtime_name: String,
    pub domain: String,
    pub base_domain: Option<String>,
    pub primary_host: String,
    pub public_port: u16,
    pub public_url_port: u16,
    pub cfg: crate::config::TakoToml,
    pub cmd: Vec<String>,
    pub readiness_failure_hint: Option<String>,
    /// Command to spawn the workflow worker subprocess on demand. `None`
    /// when the project ships no configured workflows directory or the
    /// runtime doesn't support workflows.
    pub worker_command: Option<Vec<String>>,
    pub dev_hosts: Vec<String>,
    pub env: HashMap<String, String>,
    pub secrets: HashMap<String, String>,
    pub interactive: bool,
}

pub(super) enum PrepareOutcome {
    Ready(Box<DevSession>),
    AlreadyConnected,
}

pub(super) async fn prepare(
    public_port: u16,
    variant: Option<String>,
    tunnel: bool,
    command_override: Option<Vec<String>>,
    config_path: Option<&Path>,
) -> Result<PrepareOutcome, Box<dyn std::error::Error>> {
    let context = crate::commands::project_context::resolve_existing(config_path)?;
    let config_key = context.config_key();
    let project_dir = context.project_dir.clone();
    let config_path = context.config_path.clone();
    let cfg = load_dev_tako_toml(&config_path)?;
    for warning in cfg.ignored_reserved_var_warnings() {
        crate::output::warning(&format!("Validation: {}", warning));
    }
    let eff_app_dir = project_dir.clone();
    let preset_ref = resolve_dev_preset_ref(&eff_app_dir, &cfg)?;
    let runtime_adapter = resolve_effective_dev_build_adapter(&eff_app_dir, &cfg, &preset_ref)
        .map_err(|e| format!("Failed to resolve runtime adapter: {}", e))?;
    let (mut build_preset, _) = crate::build::load_dev_build_preset(&eff_app_dir, &preset_ref)
        .await
        .map_err(|e| format!("Failed to resolve build preset '{}': {}", preset_ref, e))?;
    let plugin_ctx = tako_runtime::PluginContext {
        project_dir: &eff_app_dir,
        package_manager: cfg.package_manager.as_deref(),
    };
    apply_adapter_base_runtime_defaults(&mut build_preset, runtime_adapter, Some(&plugin_ctx))
        .map_err(|e| format!("Failed to apply runtime defaults to preset: {}", e))?;
    let main = crate::commands::deploy::resolve_deploy_main(
        &eff_app_dir,
        runtime_adapter,
        &cfg,
        build_preset.main.as_deref(),
    )
    .map_err(|e| format!("Failed to resolve deploy entrypoint: {}", e))?;

    if runtime_adapter.preset_group() == PresetGroup::Js {
        let _ = js::write_generated_files_for_adapter_and_app_root(
            &project_dir,
            runtime_adapter,
            cfg.js_app_root(),
        );
    }

    let runtime_name = build_preset.name.clone();

    let base_name = resolve_app_name_from_config_path(&config_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let app_name = if let Some(ref v) = variant {
        format!("{base_name}-{v}")
    } else {
        base_name.clone()
    };

    let existing_apps = try_list_registered_app_names().await;
    let app_name = disambiguate_app_name(&app_name, &config_key, &existing_apps);

    #[cfg(target_os = "macos")]
    macos::explain_pending_sudo_setup(LOCAL_DNS_PORT)?;
    #[cfg(target_os = "linux")]
    linux::explain_pending_sudo_setup()?;

    let local_ca = setup_local_ca().await?;
    let tls_material_updated = ensure_dev_server_tls_material(&local_ca, &app_name)?;
    let short_domain_active = ensure_local_dns_resolver_configured(LOCAL_DNS_PORT)?;

    let domain = if short_domain_active {
        LocalCA::app_short_domain(&app_name)
    } else {
        LocalCA::app_domain(&app_name)
    };
    let base_domain = if variant.is_some() {
        if short_domain_active {
            Some(LocalCA::app_short_domain(&base_name))
        } else {
            Some(LocalCA::app_domain(&base_name))
        }
    } else {
        None
    };

    #[cfg(target_os = "macos")]
    macos::ensure_installed()?;
    #[cfg(target_os = "linux")]
    linux::ensure_installed()?;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let public_url_port: u16 = 443;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let mut public_url_port: u16 = public_port;

    let daemon_dns_ip = if public_url_port == 443 {
        DEV_LOOPBACK_ADDR
    } else {
        "127.0.0.1"
    };
    let listen_addr = format!("127.0.0.1:{}", public_port);

    // Check if a dev server is already running and needs a restart.
    let existing_info = crate::dev_server_client::info().await.ok();
    let existing_listen = existing_info.as_ref().and_then(|v| {
        v.get("info")
            .and_then(|i| i.get("listen"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
    });
    let existing_advertised_ip = existing_info.as_ref().and_then(|v| {
        v.get("info")
            .and_then(|i| i.get("advertised_ip"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
    });
    let restart_for_listen =
        restart_required_for_requested_listen(existing_listen.as_deref(), &listen_addr);
    let restart_for_dns = existing_advertised_ip
        .as_deref()
        .map(|ip| ip != daemon_dns_ip)
        .unwrap_or(false);
    let restart_for_tls = tls_material_updated && existing_info.is_some();

    if restart_for_listen || restart_for_dns || restart_for_tls {
        crate::dev_server_client::stop_server().await?;
        wait_for_dev_server_stopped(&listen_addr).await;
    }

    let dev_hosts = compute_dev_hosts(&app_name, &cfg, &domain, base_domain.as_deref())
        .map_err(|e| format!("invalid development routes: {}", e))?;
    let primary_host = dev_hosts
        .iter()
        .map(|h| h.split('/').next().unwrap_or(h))
        .find(|h| !h.starts_with("*."))
        .map(|h| h.to_string())
        .unwrap_or_else(|| domain.clone());

    let mut env = compute_dev_env(&cfg);
    if runtime_adapter.preset_group() == PresetGroup::Js {
        env.insert("TAKO_APP_ROOT".to_string(), cfg.js_app_root().to_string());
    }
    inject_dev_allowed_hosts(&dev_hosts, &mut env);
    inject_dev_data_dir(&project_dir, &mut env).map_err(|e| e.to_string())?;
    let secrets = inject_dev_secrets(&project_dir, &mut env).map_err(|e| e.to_string())?;

    if runtime_adapter.preset_group() == PresetGroup::Js {
        let _ = crate::build::js::write_generated_files_for_adapter_and_app_root(
            &project_dir,
            runtime_adapter,
            cfg.js_app_root(),
        );
    }

    let cmd = resolve_dev_run_command(
        &cfg,
        &build_preset,
        &main,
        runtime_adapter,
        has_explicit_dev_preset(&cfg),
        &project_dir,
        command_override.as_deref(),
    )
    .map_err(|e| format!("Invalid dev start command: {}", e))?;
    let readiness_failure_hint = readiness_failure_hint_for_dev_command(&cmd);
    let worker_command =
        resolve_dev_worker_command(&project_dir, cfg.js_app_root(), runtime_adapter);

    // Start (or connect to) the dev server daemon.
    if let Err(e) = crate::dev_server_client::ensure_running(&listen_addr, daemon_dns_ip).await {
        return Err(format!("dev server failed to start: {}", e).into());
    }

    // Probe the HTTPS endpoint; auto-repair the dev proxy on failure.
    if public_url_port == 443 {
        let Ok(loopback_ip) = DEV_LOOPBACK_ADDR.parse::<std::net::Ipv4Addr>() else {
            return Err(format!("Invalid loopback address: {DEV_LOOPBACK_ADDR}").into());
        };
        let probe_host = local_https_probe_host(&primary_host);
        let mut probe_result = wait_for_https_host_reachable_via_ip(
            probe_host,
            loopback_ip,
            443,
            LOCALHOST_443_HTTPS_PROBE_ATTEMPTS,
            LOCALHOST_443_HTTPS_PROBE_TIMEOUT_MS,
            LOCALHOST_443_HTTPS_PROBE_RETRY_DELAY_MS,
        )
        .await;

        if probe_result.is_err() {
            probe_result =
                repair_https_probe(&listen_addr, daemon_dns_ip, probe_host, loopback_ip).await;
        }

        if let Err(ref loopback_error) = probe_result {
            crate::output::error(&format!(
                "Local HTTPS endpoint unreachable at https://{probe_host}/ ({loopback_error})"
            ));
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                crate::output::muted("Continuing with explicit dev port URL.");
                public_url_port = public_port;
            }
        }
    }

    // Reconcile DNS IP if the HTTPS probe downgraded public_url_port.
    let final_dns_ip = if public_url_port == 443 {
        DEV_LOOPBACK_ADDR
    } else {
        "127.0.0.1"
    };
    if final_dns_ip != daemon_dns_ip {
        crate::dev_server_client::stop_server().await?;
        for _ in 0..40 {
            if crate::dev_server_client::info().await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        crate::dev_server_client::ensure_running(&listen_addr, final_dns_ip)
            .await
            .map_err(|e| format!("dev server failed to start: {}", e))?;
    }

    // If the app is already running under this config, connect as a client.
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if let Ok(apps) = crate::dev_server_client::list_registered_apps().await
        && let Some(existing) = apps.iter().find(|a| a.config_path == config_key)
        && existing.status.as_str() == "running"
    {
        let url = if let Some(host) = existing.hosts.first() {
            let port = if public_url_port == 443 {
                String::new()
            } else {
                format!(":{}", public_url_port)
            };
            format!("https://{}{}/", host, port)
        } else {
            dev_url(&primary_host, public_url_port)
        };
        let session = ConnectedDevClient {
            config_key: config_key.clone(),
            config_path: config_path.clone(),
            project_dir: project_dir.clone(),
            url,
            pid: existing.pid,
            tunnel_enabled: existing.tunnel_url.is_some(),
            tunnel_url: existing.tunnel_url.clone(),
        };
        let display_hosts = compute_display_routes(&cfg, &domain, base_domain.as_deref());
        run_connected_dev_client(&app_name, interactive, tunnel, session, display_hosts).await?;
        return Ok(PrepareOutcome::AlreadyConnected);
    }

    Ok(PrepareOutcome::Ready(Box::new(DevSession {
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
    })))
}

pub(super) async fn wait_for_dev_server_stopped(listen_addr: &str) {
    let socket_path = crate::paths::tako_data_dir()
        .ok()
        .map(|dir| dir.join("dev-server.sock"));
    wait_for_dev_server_stopped_with_socket_path(listen_addr, socket_path.as_deref()).await;
}

pub(super) async fn wait_for_dev_server_stopped_with_socket_path(
    listen_addr: &str,
    socket_path: Option<&std::path::Path>,
) {
    for _ in 0..40 {
        let info_available = crate::dev_server_client::info().await.is_ok();
        let socket_still_exists = socket_path.is_some_and(|path| path.exists());
        if !info_available && !socket_still_exists {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if let Some(port_str) = listen_addr.rsplit(':').next()
        && let Ok(port) = port_str.parse::<u16>()
    {
        for _ in 0..20 {
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Diagnose why the HTTPS probe failed, fix what's broken, and retry.
///
/// Checklist:
///   1. Dev proxy installed & listening? → ensure_installed
///   2. Dev server responding?           → restart dev server
async fn repair_https_probe(
    listen_addr: &str,
    dns_ip: &str,
    probe_host: &str,
    loopback_ip: std::net::Ipv4Addr,
) -> Result<(), String> {
    // 1. Dev proxy
    #[cfg(target_os = "macos")]
    {
        let status = macos::status();
        if !status.https_ready || !status.http_ready || !status.installed {
            let _ = macos::ensure_installed();
        }
    }
    #[cfg(target_os = "linux")]
    let _ = linux::ensure_installed();

    // 2. Dev server — restart to pick up any config/cert changes
    let _ = crate::dev_server_client::stop_server().await;
    wait_for_dev_server_stopped(listen_addr).await;
    let _ = crate::dev_server_client::ensure_running(listen_addr, dns_ip).await;

    // Retry
    wait_for_https_host_reachable_via_ip(
        probe_host,
        loopback_ip,
        443,
        LOCALHOST_443_HTTPS_PROBE_ATTEMPTS,
        LOCALHOST_443_HTTPS_PROBE_TIMEOUT_MS,
        LOCALHOST_443_HTTPS_PROBE_RETRY_DELAY_MS,
    )
    .await
}

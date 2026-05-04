use crate::boot::{
    PrimaryStatus, certificate_renewal_task, probe_primary_socket, read_server_config,
    sd_notify_ready,
};
use crate::instances::{HealthChecker, HealthConfig};
use crate::metrics;
use crate::proxy::{self, ProxyConfig};
use crate::runtime_events::{handle_health_event, handle_idle_event, handle_instance_event};
use crate::scaling::{IdleConfig, IdleMonitor};
use crate::socket::SocketServer;
use crate::tls::{AcmeClient, AcmeConfig, CertManager, CertManagerConfig, ChallengeTokens};
use crate::{Args, ServerRuntimeConfig, ServerState};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

/// Permissions for the tako data directory (typically `/opt/tako`).
///
/// `0o710` = `rwx--x---`: owner (`tako`) gets full access; group (`tako`,
/// which `tako-app` is a member of) gets traverse-only so app processes
/// spawned under `tako-app` can descend into `runtimes/` and
/// `apps/{name}/{env}/releases/{ver}/` to exec binaries and read
/// release files; world gets nothing.
///
/// Do not weaken to `0o700` — the kernel denies `tako-app` directory
/// traversal without the group `x` bit, and `execve` of any nested
/// binary returns `ENOENT`, which manifests as
/// `cold start spawn failed: No such file or directory`.
#[cfg(unix)]
const DATA_DIR_MODE: u32 = 0o710;

/// Create the tako data directory (idempotent) and set its permissions
/// so the `tako-app` sandbox user can traverse into release and runtime
/// subdirectories. See [`DATA_DIR_MODE`] for rationale.
#[cfg(unix)]
pub(crate) fn prepare_data_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(DATA_DIR_MODE))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn prepare_data_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

struct StandbyPromotionConfig {
    socket_path: String,
    state: Arc<ServerState>,
    cert_manager: Arc<CertManager>,
    acme_staging: bool,
    acme_email: Option<String>,
    dns_provider: Option<String>,
    no_acme: bool,
    renewal_interval_hours: u64,
    data_dir: PathBuf,
    challenge_tokens: ChallengeTokens,
}

struct AcmeInitConfig {
    standby: bool,
    acme_staging: bool,
    acme_email: Option<String>,
    dns_provider: Option<String>,
    no_acme: bool,
    account_dir: PathBuf,
    data_dir: PathBuf,
    cert_manager: Arc<CertManager>,
    challenge_tokens: ChallengeTokens,
}

pub(crate) fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let rt = Runtime::new()?;
    let exe = std::env::current_exe().ok();

    let socket = args.socket.clone().unwrap_or_else(|| {
        if cfg!(debug_assertions)
            && let Some(exe) = &exe
            && let Some(p) = crate::paths::debug_default_socket_from_exe(exe)
        {
            return p.to_string_lossy().to_string();
        }
        "/var/run/tako/tako.sock".to_string()
    });

    let data_dir_str = args.data_dir.clone().unwrap_or_else(|| {
        if cfg!(debug_assertions)
            && let Some(exe) = &exe
            && let Some(p) = crate::paths::debug_default_data_dir_from_exe(exe)
        {
            return p.to_string_lossy().to_string();
        }
        "/var/lib/tako".to_string()
    });

    let standby = args.standby;

    tracing::info!("Tako Server v{}", crate::server_version());
    if standby {
        tracing::info!("Mode: standby");
    }
    tracing::info!("Socket: {}", socket);
    tracing::info!("HTTP port: {}", args.port);
    tracing::info!("HTTPS port: {}", args.tls_port);
    tracing::info!("Data directory: {}", data_dir_str);

    let data_dir = PathBuf::from(&data_dir_str);
    prepare_data_dir(&data_dir)?;

    if let Some(parent) = PathBuf::from(&socket).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let cert_dir = data_dir.join("certs");
    let acme_dir = data_dir.join("acme");
    std::fs::create_dir_all(&cert_dir)?;
    std::fs::create_dir_all(&acme_dir)?;

    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: cert_dir.clone(),
        ..Default::default()
    }));
    if let Err(e) = cert_manager.init() {
        tracing::warn!("Failed to initialize certificate manager: {}", e);
    }

    let (_socket_server, socket_listener) = if standby {
        (None, None)
    } else {
        let server = SocketServer::new(&socket);
        let listener = server
            .bind()
            .map_err(|e| format!("Failed to bind management socket: {e}"))?;
        (Some(server), Some(listener))
    };

    let server_config = read_server_config(&data_dir);
    let config_dns_provider = server_config.dns.as_ref().map(|d| d.provider.clone());
    let challenge_tokens: ChallengeTokens = Arc::new(parking_lot::RwLock::new(HashMap::new()));

    let acme_client = init_acme_client(
        &rt,
        AcmeInitConfig {
            standby,
            acme_staging: args.acme_staging,
            acme_email: server_config.acme_email.clone(),
            dns_provider: config_dns_provider.clone(),
            no_acme: args.no_acme,
            account_dir: acme_dir,
            data_dir: data_dir.clone(),
            cert_manager: cert_manager.clone(),
            challenge_tokens: challenge_tokens.clone(),
        },
    );

    let runtime = ServerRuntimeConfig {
        pid: std::process::id(),
        socket: socket.clone(),
        data_dir: data_dir.clone(),
        http_port: args.port,
        https_port: args.tls_port,
        no_acme: args.no_acme,
        acme_staging: args.acme_staging,
        renewal_interval_hours: args.renewal_interval_hours,
        dns_provider: config_dns_provider.clone(),
        standby,
        metrics_port: if args.metrics_port == 0 {
            None
        } else {
            Some(args.metrics_port)
        },
        server_name: server_config.server_name.or_else(|| {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .filter(|h| !h.is_empty())
        }),
    };

    let challenge_tokens_for_promote = challenge_tokens.clone();
    let state = Arc::new(ServerState::new_with_runtime(
        data_dir.clone(),
        cert_manager.clone(),
        acme_client.clone(),
        challenge_tokens,
        runtime,
    )?);
    rt.block_on(async { state.ensure_internal_socket_started() })?;

    if let Err(e) = rt.block_on(state.restore_from_state_store()) {
        tracing::error!("Failed to restore server state from SQLite: {}", e);
        return Err(e.into());
    }

    spawn_instance_event_bridge(&rt, state.clone());
    spawn_health_monitoring(&rt, state.clone());
    spawn_idle_monitoring(&rt, state.clone());
    spawn_certificate_renewals(&rt, &acme_client, args.renewal_interval_hours);
    spawn_management_socket(&rt, state.clone(), socket_listener);

    if standby {
        spawn_standby_monitor(
            &rt,
            StandbyPromotionConfig {
                socket_path: socket.clone(),
                state: state.clone(),
                cert_manager: cert_manager.clone(),
                acme_staging: args.acme_staging,
                acme_email: server_config.acme_email.clone(),
                dns_provider: config_dns_provider,
                no_acme: args.no_acme,
                renewal_interval_hours: args.renewal_interval_hours,
                data_dir: data_dir.clone(),
                challenge_tokens: challenge_tokens_for_promote.clone(),
            },
        );
    }

    let proxy_config = ProxyConfig {
        http_port: args.port,
        https_port: args.tls_port,
        enable_https: true,
        dev_mode: cfg!(debug_assertions),
        cert_dir,
        redirect_http_to_https: true,
        response_cache: Some(proxy::ResponseCacheConfig::default()),
        metrics_port: if args.metrics_port == 0 {
            None
        } else {
            Some(args.metrics_port)
        },
    };

    tracing::info!("Starting HTTP proxy on port {}", args.port);
    if proxy_config.enable_https {
        tracing::info!("HTTPS enabled on port {}", args.tls_port);
    }

    spawn_reload_signal_handlers(&rt, exe, state.clone());

    metrics::init(state.runtime_config().server_name.as_deref());

    let server = proxy::build_server_with_acme(
        state.load_balancer(),
        state.routes(),
        proxy_config,
        Some(challenge_tokens_for_promote),
        Some(cert_manager),
        state.cold_start(),
    )?;

    sd_notify_ready();
    server.run_forever();

    #[allow(unreachable_code)]
    Ok(())
}

fn init_acme_client(rt: &Runtime, config: AcmeInitConfig) -> Option<Arc<AcmeClient>> {
    if config.no_acme || config.standby {
        if config.standby {
            tracing::info!("ACME disabled (standby mode)");
        } else {
            tracing::info!("ACME disabled, using manual certificate management");
        }
        return None;
    }

    let client = Arc::new(AcmeClient::with_tokens(
        AcmeConfig {
            staging: config.acme_staging,
            email: config.acme_email,
            account_dir: config.account_dir,
            dns_provider: config.dns_provider,
            data_dir: config.data_dir,
            ..Default::default()
        },
        config.cert_manager,
        config.challenge_tokens,
    ));

    if let Err(e) = rt.block_on(client.init()) {
        tracing::error!("Failed to initialize ACME client: {}", e);
        tracing::warn!("Continuing without ACME - certificates must be managed manually");
        None
    } else {
        if config.acme_staging {
            tracing::warn!(
                "Using Let's Encrypt STAGING environment - certificates will NOT be trusted!"
            );
        } else {
            tracing::info!("ACME client initialized with Let's Encrypt production");
        }
        Some(client)
    }
}

fn spawn_instance_event_bridge(rt: &Runtime, state: Arc<ServerState>) {
    if let Some(mut event_rx) = state.app_manager().take_event_receiver() {
        let state_clone = state.clone();
        rt.spawn(async move {
            while let Some(event) = event_rx.recv().await {
                handle_instance_event(&state_clone, event).await;
            }
        });
    }
}

fn spawn_health_monitoring(rt: &Runtime, state: Arc<ServerState>) {
    let (health_event_tx, mut health_event_rx) = mpsc::channel(256);
    let health_checker = Arc::new(HealthChecker::new(HealthConfig::default(), health_event_tx));
    let app_manager = state.app_manager();
    let health_checker_clone = health_checker.clone();
    rt.spawn(async move {
        let mut app_tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

        loop {
            let app_set: HashSet<_> = app_manager.list_apps().into_iter().collect();

            for app_name in &app_set {
                if !app_tasks.contains_key(app_name)
                    && let Some(app) = app_manager.get_app(app_name)
                {
                    let checker = health_checker_clone.clone();
                    let task = tokio::spawn(async move {
                        checker.monitor_app(app).await;
                    });
                    app_tasks.insert(app_name.clone(), task);
                }
            }

            app_tasks.retain(|app_name, task| {
                if !app_set.contains(app_name) {
                    task.abort();
                    false
                } else {
                    true
                }
            });

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    let health_state = state.clone();
    rt.spawn(async move {
        while let Some(event) = health_event_rx.recv().await {
            handle_health_event(&health_state, event).await;
        }
    });
}

fn spawn_idle_monitoring(rt: &Runtime, state: Arc<ServerState>) {
    let (idle_event_tx, mut idle_event_rx) = mpsc::channel(256);
    let idle_monitor = Arc::new(IdleMonitor::new(IdleConfig::default(), idle_event_tx));
    let app_manager = state.app_manager();
    let idle_monitor_clone = idle_monitor.clone();
    rt.spawn(async move {
        let mut app_tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

        loop {
            let app_set: HashSet<_> = app_manager.list_apps().into_iter().collect();

            for app_name in &app_set {
                if !app_tasks.contains_key(app_name)
                    && let Some(app) = app_manager.get_app(app_name)
                {
                    let monitor = idle_monitor_clone.clone();
                    let task = tokio::spawn(async move {
                        monitor.monitor_app(app).await;
                    });
                    app_tasks.insert(app_name.clone(), task);
                }
            }

            app_tasks.retain(|app_name, task| {
                if !app_set.contains(app_name) {
                    task.abort();
                    false
                } else {
                    true
                }
            });

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    let idle_state = state.clone();
    rt.spawn(async move {
        while let Some(event) = idle_event_rx.recv().await {
            handle_idle_event(&idle_state, event).await;
        }
    });
}

fn spawn_certificate_renewals(
    rt: &Runtime,
    acme_client: &Option<Arc<AcmeClient>>,
    renewal_interval_hours: u64,
) {
    if let Some(acme) = acme_client {
        rt.spawn(certificate_renewal_task(
            acme.clone(),
            Duration::from_secs(renewal_interval_hours * 3600),
        ));
    }
}

fn spawn_management_socket(
    rt: &Runtime,
    state: Arc<ServerState>,
    socket_listener: Option<std::os::unix::net::UnixListener>,
) {
    if let Some(socket_listener) = socket_listener {
        rt.spawn(async move {
            if let Err(e) = SocketServer::serve(socket_listener, move |cmd| {
                let state = state.clone();
                async move { state.handle_command(cmd).await }
            })
            .await
            {
                tracing::error!("Socket server error: {}", e);
            }
        });
    }
}

fn spawn_standby_monitor(rt: &Runtime, config: StandbyPromotionConfig) {
    rt.spawn(async move {
        const PROBE_INTERVAL: Duration = Duration::from_secs(5);
        const FAILURE_THRESHOLD: u32 = 3;
        let mut consecutive_failures: u32 = 0;
        let mut promoted = false;
        let our_pid = std::process::id();

        loop {
            tokio::time::sleep(PROBE_INTERVAL).await;

            match probe_primary_socket(&config.socket_path, our_pid).await {
                PrimaryStatus::Alive => {
                    if promoted {
                        tracing::info!("Primary server is back — standby shutting down");
                        #[cfg(unix)]
                        unsafe {
                            libc::kill(libc::getpid(), libc::SIGTERM);
                        }
                        break;
                    }
                    if consecutive_failures > 0 {
                        tracing::debug!("Primary socket is alive again");
                    }
                    consecutive_failures = 0;
                }
                PrimaryStatus::IsUs => {
                    consecutive_failures = 0;
                }
                PrimaryStatus::Down => {
                    consecutive_failures += 1;
                    tracing::warn!(
                        failures = consecutive_failures,
                        "Primary management socket not responding"
                    );

                    if consecutive_failures >= FAILURE_THRESHOLD && !promoted {
                        tracing::info!("Promoting standby to full mode");

                        let server = SocketServer::new(&config.socket_path);
                        match server.bind() {
                            Ok(listener) => {
                                let socket_state = config.state.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = SocketServer::serve(listener, move |cmd| {
                                        let state = socket_state.clone();
                                        async move { state.handle_command(cmd).await }
                                    })
                                    .await
                                    {
                                        tracing::error!("Socket server error after promotion: {e}");
                                    }
                                });
                                std::mem::forget(server);
                                tracing::info!("Management socket bound after promotion");
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to bind management socket on promotion: {e}"
                                );
                                consecutive_failures = 0;
                                continue;
                            }
                        }

                        if !config.no_acme {
                            let client = Arc::new(AcmeClient::with_tokens(
                                AcmeConfig {
                                    staging: config.acme_staging,
                                    email: config.acme_email.clone(),
                                    account_dir: config.data_dir.join("acme"),
                                    dns_provider: config.dns_provider.clone(),
                                    data_dir: config.data_dir.clone(),
                                    ..Default::default()
                                },
                                config.cert_manager.clone(),
                                config.challenge_tokens.clone(),
                            ));
                            match client.init().await {
                                Ok(()) => {
                                    tracing::info!("ACME initialized after promotion");
                                    tokio::spawn(certificate_renewal_task(
                                        client.clone(),
                                        Duration::from_secs(config.renewal_interval_hours * 3600),
                                    ));
                                    config.state.set_acme_client(client).await;
                                }
                                Err(e) => {
                                    tracing::error!("Failed to init ACME after promotion: {e}");
                                }
                            }
                        }

                        promoted = true;
                        consecutive_failures = 0;
                        tracing::info!("Promotion complete — standby now running as full server");
                    }
                }
            }
        }
    });
}

fn spawn_reload_signal_handlers(
    rt: &Runtime,
    startup_exe: Option<PathBuf>,
    state: Arc<ServerState>,
) {
    #[cfg(unix)]
    {
        use crate::SIGNAL_PARENT_ON_READY_ENV;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::signal::unix::{SignalKind, signal};

        let shutdown_started = Arc::new(AtomicBool::new(false));

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

        let shutdown_state = state.clone();
        let shutdown_started = shutdown_started.clone();
        rt.spawn(async move {
            let mut terminate = match signal(SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(err) => {
                    tracing::error!("Failed to register SIGTERM handler: {err}");
                    return;
                }
            };
            terminate.recv().await;
            if shutdown_started.swap(true, Ordering::SeqCst) {
                return;
            }
            shutdown_state
                .shutdown_runtime(Duration::from_secs(120))
                .await;
        });
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn prepare_data_dir_creates_dir_with_group_traverse_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tako-data");

        prepare_data_dir(&dir).expect("prepare_data_dir");

        assert!(dir.is_dir());
        assert_eq!(
            mode_of(&dir),
            0o710,
            "data dir must grant group traverse so tako-app can descend into \
             runtimes/ and releases/ to exec app binaries; 0o700 triggers \
             ENOENT on execve because the kernel denies directory traversal"
        );
    }

    #[test]
    fn prepare_data_dir_upgrades_legacy_0o700_dir_to_0o710() {
        // Regression: older installers left /opt/tako at mode 0o700, which
        // blocks tako-app (a group-tako member) from traversing in. On the
        // next server boot, prepare_data_dir must fix the mode in place.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tako-data");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        prepare_data_dir(&dir).expect("prepare_data_dir");

        assert_eq!(mode_of(&dir), 0o710);
    }

    #[test]
    fn prepare_data_dir_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tako-data");

        prepare_data_dir(&dir).expect("prepare_data_dir first call");
        prepare_data_dir(&dir).expect("prepare_data_dir second call");

        assert_eq!(mode_of(&dir), 0o710);
    }
}

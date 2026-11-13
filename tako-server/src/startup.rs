use crate::boot::{read_server_config, sd_notify_ready, should_signal_parent_on_ready};
use crate::identity::load_or_create_server_identity;
use crate::metrics;
use crate::proxy::{self, ProxyConfig};
use crate::socket::SocketServer;
use crate::tls::{CertManager, CertManagerConfig, ChallengeTokens};
use crate::{Args, ServerRuntimeConfig, ServerState};
#[cfg(unix)]
use pingora_core::server::RunArgs;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;

mod acme;
mod cloudflare;
mod data_dir;
#[cfg(target_os = "linux")]
mod native_promotion;
mod signals;
mod standby;
mod tasks;

use acme::{AcmeInitConfig, init_acme_client, spawn_certificate_renewals};
use cloudflare::{should_skip_cloudflare_ip_refresh, spawn_cloudflare_ip_refresh};
pub(crate) use data_dir::prepare_data_dir;
use signals::{TakoShutdownSignalWatch, spawn_reload_signal_handlers};
use standby::{StandbyPromotionConfig, spawn_standby_monitor};
use tasks::{
    spawn_health_monitoring, spawn_idle_monitoring, spawn_instance_event_bridge,
    spawn_instance_reconcile, spawn_management_http, spawn_management_socket,
};

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
    tracing::info!("HTTP port: {}", args.http_port);
    tracing::info!("HTTPS port: {}", args.https_port);
    tracing::info!("Data directory: {}", data_dir_str);

    let data_dir = PathBuf::from(&data_dir_str);
    prepare_data_dir(&data_dir)?;
    #[cfg(target_os = "linux")]
    native_promotion::guard_promotion_startup(&data_dir)?;
    let server_identity = if standby {
        None
    } else {
        Some(
            load_or_create_server_identity(&data_dir)
                .map_err(|e| format!("Failed to load server identity: {e}"))?
                .fingerprint,
        )
    };

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
    let native_control =
        crate::native_control::NativeControl::load(&data_dir, &cert_manager)?.map(Arc::new);
    let native_channels = crate::server_state::native_channel_client(&data_dir)?;
    let native_config = native_control
        .as_ref()
        .map(|_| tako_network::NodeConfig::load(&data_dir))
        .transpose()?;
    if let Some(config) = &native_config {
        crate::compose_runtime::configure_native_network(&config.network.id)?;
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
    let trusted_proxy_config = match server_config.trusted_proxy.as_ref() {
        Some(config) => proxy::TrustedProxyConfig::from_raw(
            config.proxy_protocol,
            &config.trusted_cidrs,
            &config.client_ip_headers,
        )?,
        None => proxy::TrustedProxyConfig::default(),
    };
    let challenge_tokens: ChallengeTokens = Arc::new(parking_lot::RwLock::new(HashMap::new()));

    let acme_client = init_acme_client(
        &rt,
        AcmeInitConfig {
            standby,
            acme_staging: args.acme_staging,
            acme_email: server_config.acme_email.clone(),
            no_acme: args.no_acme,
            account_dir: acme_dir,
            cert_manager: cert_manager.clone(),
            challenge_tokens: challenge_tokens.clone(),
        },
    );

    let runtime = ServerRuntimeConfig {
        pid: std::process::id(),
        process_started_at_unix_secs: Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or_default(),
        ),
        socket: socket.clone(),
        data_dir: data_dir.clone(),
        http_port: args.http_port,
        https_port: args.https_port,
        no_acme: args.no_acme,
        acme_staging: args.acme_staging,
        renewal_interval_hours: args.renewal_interval_hours,
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
        server_identity,
    };

    #[cfg(target_os = "linux")]
    let native_handoff = if native_config.is_some() && !standby {
        Some(
            rt.block_on(crate::native_management_ownership::PrimaryHandoff::claim(
                &data_dir,
                Duration::from_secs(120),
            ))?,
        )
    } else {
        None
    };

    #[cfg(target_os = "linux")]
    native_promotion::guard_promotion_startup(&data_dir)?;
    let challenge_tokens_for_promote = challenge_tokens.clone();
    #[cfg(target_os = "linux")]
    let promotion_child = native_promotion::is_promotion_child();
    #[cfg(not(target_os = "linux"))]
    let promotion_child = false;
    let reload_owner = if promotion_child {
        None
    } else {
        take_upgrade_reload_owner(&data_dir)?
    };
    let reload_owner = if should_signal_parent_on_ready() {
        reload_owner.as_deref()
    } else {
        None
    };
    // Before any app or workflow store opens app data: at a normal start, a
    // restore that the previous server's death interrupted is rolled back to
    // the original data. A standby, an upgrade reload, a promotion child, and
    // a normal start while another server process (such as a hot standby)
    // serves this data directory only report unfinished restores.
    let (recovery_mode, recovery_lease) = startup_recovery_mode(
        &data_dir,
        if standby {
            crate::operations::RecoveryMode::Standby
        } else if promotion_child || reload_owner.is_some() {
            crate::operations::RecoveryMode::Successor
        } else {
            crate::operations::RecoveryMode::Primary
        },
    );
    crate::operations::recover_interrupted_restores(&data_dir, recovery_mode);
    if recovery_mode == crate::operations::RecoveryMode::Primary {
        // Only this process can be using backup work directories now.
        crate::operations::sweep_backup_work_dirs(&data_dir);
    }
    // This process takes its own shared lease below.
    drop(recovery_lease);
    let state = Arc::new(if let Some(reload_owner) = reload_owner {
        ServerState::new_with_runtime_for_reload(
            data_dir.clone(),
            cert_manager.clone(),
            acme_client.clone(),
            challenge_tokens,
            runtime,
            reload_owner,
        )?
    } else {
        ServerState::new_with_runtime(
            data_dir.clone(),
            cert_manager.clone(),
            acme_client.clone(),
            challenge_tokens,
            runtime,
        )?
    });
    if let Err(e) = rt.block_on(async {
        let _guard = state.management_lock.lock().await;
        crate::drain_observer::spawn(&rt, state.clone());
        state.ensure_internal_socket_started()?;
        if native_config.is_some() && !standby {
            if state.is_draining() {
                Ok(())
            } else {
                state.restore_apps_from_state_store(true).await
            }
        } else {
            state.restore_from_state_store().await
        }
    }) {
        tracing::error!("Failed to restore server state from SQLite: {}", e);
        return Err(e.into());
    }
    let native_services = native_config
        .as_ref()
        .map(|config| {
            crate::native_proxy::NativeProxy::start(&rt, state.clone(), config, state.drain_flag())
        })
        .transpose()?;

    let cloudflare_ips = proxy::CloudflareIpRanges::default();
    tracing::info!(
        cidrs = cloudflare_ips.count(),
        "Loaded static Cloudflare IP ranges"
    );
    let cloudflare_ip_cache_path = data_dir.join("cloudflare-ips.json");
    if cloudflare_ip_cache_path.exists() {
        match cloudflare_ips.load_cache_file(&cloudflare_ip_cache_path) {
            Ok(()) => {
                tracing::info!(
                    cidrs = cloudflare_ips.count(),
                    path = %cloudflare_ip_cache_path.display(),
                    "Loaded cached Cloudflare IP ranges"
                );
            }
            Err(error) => {
                tracing::warn!(
                    path = %cloudflare_ip_cache_path.display(),
                    "Cloudflare IP range cache ignored: {error}"
                );
            }
        }
    }
    spawn_instance_event_bridge(&rt, state.clone());
    spawn_health_monitoring(&rt, state.clone());
    spawn_instance_reconcile(&rt, state.clone());
    spawn_idle_monitoring(&rt, state.clone());
    spawn_certificate_renewals(&rt, state.clone(), args.renewal_interval_hours);
    spawn_management_socket(&rt, state.clone(), socket_listener);
    if !standby {
        state.clone().start_backup_scheduler(rt.handle());
        spawn_management_http(&rt, state.clone(), args.management_host.clone());
        #[cfg(target_os = "linux")]
        if let Some(handoff) = native_handoff {
            let management = rt.block_on(crate::native_management_listener::prepare(
                data_dir.clone(),
                state.clone(),
                handoff,
            ))?;
            rt.spawn(async move {
                if let Err(error) = management.run().await {
                    tracing::error!(%error, "Native management listener stopped");
                }
            });
        }
    }

    if standby {
        state.clone().start_unresolved_restore_watch(rt.handle());
        spawn_standby_monitor(
            &rt,
            StandbyPromotionConfig {
                socket_path: socket.clone(),
                state: state.clone(),
                cert_manager: cert_manager.clone(),
                acme_staging: args.acme_staging,
                acme_email: server_config.acme_email.clone(),
                no_acme: args.no_acme,
                renewal_interval_hours: args.renewal_interval_hours,
                data_dir: data_dir.clone(),
                challenge_tokens: challenge_tokens_for_promote.clone(),
            },
        );
    }

    let proxy_config = ProxyConfig {
        http_port: args.http_port,
        https_port: args.https_port,
        enable_https: true,
        dev_mode: cfg!(debug_assertions),
        cert_dir,
        redirect_http_to_https: true,
        response_cache: None,
        metrics_port: if args.metrics_port == 0 {
            None
        } else {
            Some(args.metrics_port)
        },
        trusted_proxy: trusted_proxy_config,
    };

    tracing::info!("Starting HTTP proxy on port {}", args.http_port);
    if proxy_config.enable_https {
        tracing::info!("HTTPS enabled on port {}", args.https_port);
    }

    spawn_reload_signal_handlers(&rt, exe);

    metrics::init(state.runtime_config().server_name.as_deref());
    if args.metrics_port != 0 {
        tako_workflows::monitoring::register(
            &state.workflows,
            metrics::server(),
            prometheus::default_registry(),
            rt.handle(),
        )?;
    }
    if !should_skip_cloudflare_ip_refresh() {
        spawn_cloudflare_ip_refresh(
            &rt,
            cloudflare_ips.clone(),
            cloudflare_ip_cache_path,
            state.routes(),
        );
    }

    let server = proxy::build_server_with_acme(proxy::ServerBuildConfig {
        draining: state.drain_flag(),
        spawn_gate: state.spawn_gate.clone(),
        proxy_requests: state.proxy_requests.clone(),
        native_control,
        native_channels,
        native_services,
        lb: state.load_balancer(),
        routes: state.routes(),
        config: proxy_config,
        acme_tokens: Some(challenge_tokens_for_promote),
        cert_manager: Some(cert_manager),
        cold_start: state.cold_start(),
        cloudflare_ips,
        channel_postgres_url: Some({
            let state = state.clone();
            Arc::new(move |app: &str| state.runtime_postgres_url(app))
        }),
        channel_credentials_generation: state.channel_credentials_generation.clone(),
        channel_registry: state.channel_registry.clone(),
    })?;

    sd_notify_ready();
    server.run(RunArgs {
        shutdown_signal: Box::new(TakoShutdownSignalWatch::new(
            state,
            Duration::from_secs(120),
        )),
    });

    Ok(())
}

/// What startup recovery of backup restores may do. A normal start rolls
/// back only while no other server process serves this data directory:
/// each holds a shared drain activity lease, a hot standby included, whose
/// app instances use the same app data. The returned exclusive lease keeps
/// any from starting to serve until recovery is done.
fn startup_recovery_mode(
    data_dir: &std::path::Path,
    mode: crate::operations::RecoveryMode,
) -> (
    crate::operations::RecoveryMode,
    Option<crate::state_store::DrainLease>,
) {
    use crate::operations::RecoveryMode;
    if mode != RecoveryMode::Primary {
        return (mode, None);
    }
    match crate::state_store::DrainLease::try_exclusive_in(data_dir) {
        Ok(Some(lease)) => (mode, Some(lease)),
        Ok(None) => {
            tracing::warn!(
                "Another server process serves this data directory; this start only reports unfinished backup restores"
            );
            (RecoveryMode::Successor, None)
        }
        Err(error) => {
            tracing::error!(
                "Cannot tell whether another server process serves this data directory ({error}); this start only reports unfinished backup restores"
            );
            (RecoveryMode::Successor, None)
        }
    }
}

fn take_upgrade_reload_owner(data_dir: &std::path::Path) -> std::io::Result<Option<String>> {
    let marker_path = data_dir.join(tako_core::UPGRADE_RELOAD_MARKER_FILE);
    let owner = match std::fs::read_to_string(&marker_path) {
        Ok(owner) => owner,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    std::fs::remove_file(marker_path)?;
    let owner = owner.trim();
    if owner.is_empty() {
        Ok(None)
    } else {
        Ok(Some(owner.to_string()))
    }
}

#[cfg(all(test, unix))]
mod tests;

use crate::boot::{read_server_config, sd_notify_ready};
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
    spawn_management_http, spawn_management_socket,
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
    spawn_idle_monitoring(&rt, state.clone());
    spawn_certificate_renewals(&rt, state.clone(), args.renewal_interval_hours);
    spawn_management_socket(&rt, state.clone(), socket_listener);
    if !standby {
        state.clone().start_backup_scheduler(rt.handle());
        spawn_management_http(&rt, state.clone(), args.management_host.clone());
    }

    if standby {
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
    if !should_skip_cloudflare_ip_refresh() {
        spawn_cloudflare_ip_refresh(
            &rt,
            cloudflare_ips.clone(),
            cloudflare_ip_cache_path,
            state.routes(),
        );
    }

    let server = proxy::build_server_with_acme(
        state.load_balancer(),
        state.routes(),
        proxy_config,
        Some(challenge_tokens_for_promote),
        Some(cert_manager),
        state.cold_start(),
        cloudflare_ips,
        Some({
            let state = state.clone();
            Arc::new(move |app: &str| state.runtime_postgres_url(app))
        }),
    )?;

    sd_notify_ready();
    server.run(RunArgs {
        shutdown_signal: Box::new(TakoShutdownSignalWatch::new(
            state,
            Duration::from_secs(120),
        )),
    });

    Ok(())
}

#[cfg(all(test, unix))]
mod tests;

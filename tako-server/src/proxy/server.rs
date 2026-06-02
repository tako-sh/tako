use crate::lb::LoadBalancer;
use crate::proxy::proxy_protocol_service::{ProxyProtocolService, ProxyProtocolTlsAcceptor};
use crate::proxy::{CloudflareIpRanges, ProxyConfig, RouteTable, TakoProxy};
use crate::scaling::ColdStartManager;
use crate::tls::{CertManager, ChallengeTokens, SelfSignedGenerator, create_sni_callbacks};
use parking_lot::RwLock;
use pingora_core::listeners::TcpSocketOptions;
use pingora_core::listeners::tls::TlsSettings;
use pingora_core::prelude::*;
use pingora_core::server::configuration::ServerConf;
use pingora_core::services::listening::Service as ListeningService;
use std::sync::Arc;

/// Build and start the Pingora server with ACME and SNI support
pub fn build_server_with_acme(
    lb: Arc<LoadBalancer>,
    routes: Arc<RwLock<RouteTable>>,
    config: ProxyConfig,
    acme_tokens: Option<ChallengeTokens>,
    cert_manager: Option<Arc<CertManager>>,
    cold_start: Arc<ColdStartManager>,
    cloudflare_ips: CloudflareIpRanges,
) -> Result<Server> {
    let mut server = Server::new_with_opt_and_conf(None, proxy_server_conf()?);
    server.bootstrap();

    let proxy = if let Some(tokens) = acme_tokens {
        TakoProxy::with_acme(
            lb,
            routes.clone(),
            config.clone(),
            tokens,
            cold_start,
            cloudflare_ips,
        )
    } else {
        TakoProxy::new(
            lb,
            routes.clone(),
            config.clone(),
            cold_start,
            cloudflare_ips,
        )
    };

    if config.trusted_proxy.proxy_protocol {
        let mut proxy_service = ProxyProtocolService::new(
            "Tako PROXY Protocol HTTP Proxy Service",
            &server.configuration,
            proxy,
            config.trusted_proxy.clone(),
        );

        let listener_options = listener_socket_options();
        proxy_service.add_tcp_with_settings(
            &format!("0.0.0.0:{}", config.http_port),
            listener_options.clone(),
        );

        if config.enable_https {
            if let Some(tls_acceptor) =
                create_proxy_protocol_tls_acceptor(&config, cert_manager.clone())?
            {
                proxy_service.add_tls_with_settings(
                    &format!("0.0.0.0:{}", config.https_port),
                    Some(listener_options),
                    Arc::new(tls_acceptor),
                );
                tracing::info!(
                    port = config.https_port,
                    "HTTPS listener enabled with PROXY protocol"
                );
            } else {
                tracing::warn!("HTTPS enabled but no certificates available");
            }
        }

        tracing::info!("PROXY protocol enabled on public listeners");
        server.add_service(proxy_service);
    } else {
        let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, proxy);

        if let Some(app) = proxy_service.app_logic_mut() {
            let mut opts = pingora_core::apps::HttpServerOptions::default();
            // Pingora keeps per-connection memory while downstream keepalive is
            // open. This frees it periodically without disabling keepalive.
            opts.keepalive_request_limit = Some(1000);
            app.server_options = Some(opts);
        }

        let listener_options = listener_socket_options();
        proxy_service.add_tcp_with_settings(
            &format!("0.0.0.0:{}", config.http_port),
            listener_options.clone(),
        );

        if config.enable_https {
            if let Some(tls_settings) = create_tls_settings(&config, cert_manager)? {
                proxy_service.add_tls_with_settings(
                    &format!("0.0.0.0:{}", config.https_port),
                    Some(listener_options),
                    tls_settings,
                );
                tracing::info!(port = config.https_port, "HTTPS listener enabled");
            } else {
                tracing::warn!("HTTPS enabled but no certificates available");
            }
        }

        server.add_service(proxy_service);
    }

    if let Some(metrics_port) = config.metrics_port {
        let mut metrics_service = ListeningService::prometheus_http_service();
        metrics_service.add_tcp(&format!("127.0.0.1:{}", metrics_port));
        server.add_service(metrics_service);
        tracing::info!(port = metrics_port, "Prometheus metrics listener enabled");
    }

    Ok(server)
}

fn proxy_server_conf() -> Result<ServerConf> {
    let mut conf = ServerConf::new().ok_or_else(|| {
        Error::explain(
            ErrorType::ReadError,
            "Failed to create default Pingora server configuration",
        )
    })?;
    // Pingora defaults to one worker. Use the VM's available CPUs so the proxy
    // baseline is comparable to nginx/Caddy under sustained load.
    conf.threads = proxy_service_threads();
    conf.upstream_keepalive_pool_size = upstream_keepalive_pool_size_for_threads(conf.threads);
    conf.max_retries = 1;
    conf.grace_period_seconds = Some(0);
    conf.graceful_shutdown_timeout_seconds = Some(5);
    Ok(conf)
}

fn proxy_service_threads() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

fn upstream_keepalive_pool_size_for_threads(threads: usize) -> usize {
    256 * threads.max(1)
}

pub(crate) fn listener_socket_options() -> TcpSocketOptions {
    let mut options = TcpSocketOptions::default();
    options.so_reuseport = Some(true);
    options
}

pub(crate) fn create_proxy_protocol_tls_acceptor(
    config: &ProxyConfig,
    cert_manager: Option<Arc<CertManager>>,
) -> Result<Option<ProxyProtocolTlsAcceptor>> {
    std::fs::create_dir_all(&config.cert_dir).map_err(|e| {
        Error::explain(
            ErrorType::InternalError,
            format!("Failed to create cert directory: {}", e),
        )
    })?;

    if config.dev_mode {
        let generator = SelfSignedGenerator::new(&config.cert_dir);
        let cert = generator.get_or_create_localhost().map_err(|e| {
            Error::explain(
                ErrorType::InternalError,
                format!("Failed to generate self-signed cert: {}", e),
            )
        })?;

        let cert_path_str = cert.cert_path.to_string_lossy().to_string();
        let key_path_str = cert.key_path.to_string_lossy().to_string();

        let acceptor = ProxyProtocolTlsAcceptor::with_cert_files(&cert_path_str, &key_path_str)
            .map_err(|e| {
                Error::explain(
                    ErrorType::InternalError,
                    format!("Failed to create TLS acceptor: {}", e),
                )
            })?;

        tracing::info!(
            cert_path = %cert.cert_path.display(),
            "Loaded self-signed TLS certificate (dev mode)"
        );

        Ok(Some(acceptor))
    } else if let Some(cm) = cert_manager {
        let callbacks = create_sni_callbacks(cm);

        let acceptor = ProxyProtocolTlsAcceptor::with_callbacks(callbacks).map_err(|e| {
            Error::explain(
                ErrorType::InternalError,
                format!("Failed to create TLS acceptor with SNI callbacks: {}", e),
            )
        })?;

        tracing::info!("TLS enabled with SNI-based certificate selection");

        Ok(Some(acceptor))
    } else {
        let default_cert = config.cert_dir.join("default/fullchain.pem");
        let default_key = config.cert_dir.join("default/privkey.pem");

        if !default_cert.exists() || !default_key.exists() {
            tracing::warn!(
                "No certificate manager and no default certificate found. HTTPS disabled."
            );
            return Ok(None);
        }

        let cert_path_str = default_cert.to_string_lossy().to_string();
        let key_path_str = default_key.to_string_lossy().to_string();

        let acceptor = ProxyProtocolTlsAcceptor::with_cert_files(&cert_path_str, &key_path_str)
            .map_err(|e| {
                Error::explain(
                    ErrorType::InternalError,
                    format!("Failed to create TLS acceptor: {}", e),
                )
            })?;

        tracing::info!("Loaded default TLS certificate");

        Ok(Some(acceptor))
    }
}

pub(crate) fn create_tls_settings(
    config: &ProxyConfig,
    cert_manager: Option<Arc<CertManager>>,
) -> Result<Option<TlsSettings>> {
    std::fs::create_dir_all(&config.cert_dir).map_err(|e| {
        Error::explain(
            ErrorType::InternalError,
            format!("Failed to create cert directory: {}", e),
        )
    })?;

    if config.dev_mode {
        let generator = SelfSignedGenerator::new(&config.cert_dir);
        let cert = generator.get_or_create_localhost().map_err(|e| {
            Error::explain(
                ErrorType::InternalError,
                format!("Failed to generate self-signed cert: {}", e),
            )
        })?;

        let cert_path_str = cert.cert_path.to_string_lossy().to_string();
        let key_path_str = cert.key_path.to_string_lossy().to_string();

        let mut tls_settings =
            TlsSettings::intermediate(&cert_path_str, &key_path_str).map_err(|e| {
                Error::explain(
                    ErrorType::InternalError,
                    format!("Failed to create TLS settings: {}", e),
                )
            })?;

        tls_settings.enable_h2();

        tracing::info!(
            cert_path = %cert.cert_path.display(),
            "Loaded self-signed TLS certificate (dev mode)"
        );

        Ok(Some(tls_settings))
    } else if let Some(cm) = cert_manager {
        let callbacks = create_sni_callbacks(cm);

        let mut tls_settings = TlsSettings::with_callbacks(callbacks).map_err(|e| {
            Error::explain(
                ErrorType::InternalError,
                format!("Failed to create TLS settings with SNI callbacks: {}", e),
            )
        })?;

        tls_settings.enable_h2();

        tracing::info!("TLS enabled with SNI-based certificate selection");

        Ok(Some(tls_settings))
    } else {
        let default_cert = config.cert_dir.join("default/fullchain.pem");
        let default_key = config.cert_dir.join("default/privkey.pem");

        if !default_cert.exists() || !default_key.exists() {
            tracing::warn!(
                "No certificate manager and no default certificate found. HTTPS disabled."
            );
            return Ok(None);
        }

        let cert_path_str = default_cert.to_string_lossy().to_string();
        let key_path_str = default_key.to_string_lossy().to_string();

        let mut tls_settings =
            TlsSettings::intermediate(&cert_path_str, &key_path_str).map_err(|e| {
                Error::explain(
                    ErrorType::InternalError,
                    format!("Failed to create TLS settings: {}", e),
                )
            })?;

        tls_settings.enable_h2();

        tracing::info!(
            cert_path = %default_cert.display(),
            "Loaded default TLS certificate"
        );

        Ok(Some(tls_settings))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_server_conf_uses_available_parallelism() {
        let conf = proxy_server_conf().expect("proxy server config");

        assert_eq!(conf.threads, proxy_service_threads());
        assert!(conf.threads >= 1);
    }

    #[test]
    fn upstream_keepalive_pool_scales_with_proxy_threads() {
        assert_eq!(upstream_keepalive_pool_size_for_threads(1), 256);
        assert_eq!(upstream_keepalive_pool_size_for_threads(2), 512);
        assert_eq!(upstream_keepalive_pool_size_for_threads(8), 2048);
    }

    #[test]
    fn proxy_server_conf_scales_upstream_keepalive_pool_with_threads() {
        let conf = proxy_server_conf().expect("proxy server config");

        assert_eq!(
            conf.upstream_keepalive_pool_size,
            upstream_keepalive_pool_size_for_threads(conf.threads)
        );
    }

    #[test]
    fn proxy_server_conf_uses_single_upstream_attempt() {
        let conf = proxy_server_conf().expect("proxy server config");

        assert_eq!(conf.max_retries, 1);
    }
}

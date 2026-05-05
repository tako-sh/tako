use crate::lb::LoadBalancer;
use crate::proxy::{ProxyConfig, RouteTable, TakoProxy};
use crate::scaling::ColdStartManager;
use crate::tls::{
    CertInfo, CertManager, ChallengeTokens, SelfSignedGenerator, create_sni_callbacks,
};
use pingora_core::listeners::TcpSocketOptions;
use pingora_core::listeners::tls::TlsSettings;
use pingora_core::prelude::*;
use pingora_core::server::configuration::ServerConf;
use pingora_core::services::listening::Service as ListeningService;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// TLS configuration for the proxy
pub struct TlsConfig {
    /// Certificate manager
    cert_manager: Arc<CertManager>,
    /// Self-signed generator for dev mode
    self_signed: Option<SelfSignedGenerator>,
}

impl TlsConfig {
    /// Create TLS config with certificate manager
    pub fn new(cert_manager: Arc<CertManager>) -> Self {
        Self {
            cert_manager,
            self_signed: None,
        }
    }

    /// Create TLS config for development with self-signed certs
    pub fn development(cert_dir: PathBuf) -> Self {
        Self {
            cert_manager: Arc::new(CertManager::new(crate::tls::CertManagerConfig {
                cert_dir: cert_dir.clone(),
                ..Default::default()
            })),
            self_signed: Some(SelfSignedGenerator::new(cert_dir)),
        }
    }

    /// Get or create certificate for a domain
    pub fn get_cert(&self, domain: &str) -> Option<CertInfo> {
        if let Some(cert) = self.cert_manager.get_cert_for_host(domain) {
            return Some(cert);
        }

        if let Some(ref generator) = self.self_signed
            && (domain == "localhost" || domain.ends_with(".localhost"))
            && let Ok(self_signed) = generator.get_or_create_localhost()
        {
            return Some(CertInfo {
                domain: domain.to_string(),
                cert_path: self_signed.cert_path,
                key_path: self_signed.key_path,
                expires_at: None,
                is_wildcard: false,
                is_self_signed: true,
            });
        }

        None
    }

    /// Get default certificate (for SNI fallback)
    pub fn get_default_cert(&self) -> Option<CertInfo> {
        if let Some(cert) = self.get_cert("localhost") {
            return Some(cert);
        }

        self.cert_manager.list_certs().into_iter().next()
    }
}

/// Build and start the Pingora server
pub fn build_server(
    lb: Arc<LoadBalancer>,
    config: ProxyConfig,
    cold_start: Arc<ColdStartManager>,
) -> Result<Server> {
    build_server_with_acme(
        lb,
        Arc::new(RwLock::new(RouteTable::default())),
        config,
        None,
        None,
        cold_start,
    )
}

/// Build and start the Pingora server with ACME and SNI support
pub fn build_server_with_acme(
    lb: Arc<LoadBalancer>,
    routes: Arc<RwLock<RouteTable>>,
    config: ProxyConfig,
    acme_tokens: Option<ChallengeTokens>,
    cert_manager: Option<Arc<CertManager>>,
    cold_start: Arc<ColdStartManager>,
) -> Result<Server> {
    let mut server = Server::new_with_opt_and_conf(None, proxy_server_conf()?);
    server.bootstrap();

    let proxy = if let Some(tokens) = acme_tokens {
        TakoProxy::with_acme(lb, routes.clone(), config.clone(), tokens, cold_start)
    } else {
        TakoProxy::new(lb, routes.clone(), config.clone(), cold_start)
    };

    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, proxy);

    if let Some(app) = proxy_service.app_logic_mut() {
        let mut opts = pingora_core::apps::HttpServerOptions::default();
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
    conf.grace_period_seconds = Some(0);
    conf.graceful_shutdown_timeout_seconds = Some(5);
    Ok(conf)
}

pub(crate) fn listener_socket_options() -> TcpSocketOptions {
    let mut options = TcpSocketOptions::default();
    options.so_reuseport = Some(true);
    options
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

/// Builder for configuring the proxy server
pub struct ProxyBuilder {
    pub(super) lb: Arc<LoadBalancer>,
    pub(super) routes: Arc<RwLock<RouteTable>>,
    pub(super) config: ProxyConfig,
    pub(super) tls_config: Option<TlsConfig>,
    pub(super) acme_tokens: Option<ChallengeTokens>,
    pub(super) cert_manager: Option<Arc<CertManager>>,
}

impl ProxyBuilder {
    pub fn new(lb: Arc<LoadBalancer>) -> Self {
        Self {
            lb,
            routes: Arc::new(RwLock::new(RouteTable::default())),
            config: ProxyConfig::default(),
            tls_config: None,
            acme_tokens: None,
            cert_manager: None,
        }
    }

    pub fn routes(mut self, routes: Arc<RwLock<RouteTable>>) -> Self {
        self.routes = routes;
        self
    }

    pub fn config(mut self, config: ProxyConfig) -> Self {
        self.config = config;
        self
    }

    pub fn http_port(mut self, port: u16) -> Self {
        self.config.http_port = port;
        self
    }

    pub fn https_port(mut self, port: u16) -> Self {
        self.config.https_port = port;
        self
    }

    pub fn dev_mode(mut self) -> Self {
        self.config.dev_mode = true;
        self
    }

    pub fn cert_dir(mut self, dir: PathBuf) -> Self {
        self.config.cert_dir = dir;
        self
    }

    pub fn tls(mut self, tls_config: TlsConfig) -> Self {
        self.tls_config = Some(tls_config);
        self
    }

    pub fn acme_tokens(mut self, tokens: ChallengeTokens) -> Self {
        self.acme_tokens = Some(tokens);
        self
    }

    pub fn cert_manager(mut self, cm: Arc<CertManager>) -> Self {
        self.cert_manager = Some(cm);
        self
    }

    pub fn build(self) -> Result<Server> {
        build_server_with_acme(
            self.lb,
            self.routes,
            self.config,
            self.acme_tokens,
            self.cert_manager,
            Arc::new(ColdStartManager::new(
                crate::scaling::ColdStartConfig::default(),
            )),
        )
    }
}

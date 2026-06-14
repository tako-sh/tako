use super::*;

#[test]
fn test_tako_proxy_creation() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager));
    let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(
        crate::scaling::ColdStartConfig::default(),
    ));
    let proxy = TakoProxy::new(
        lb,
        routes,
        ProxyConfig::default(),
        cold_start,
        CloudflareIpRanges::default(),
    );

    // Just verify creation works
    let ctx = proxy.new_ctx();
    assert!(ctx.backend.is_none());
    assert!(!ctx.is_https);
    assert!(ctx.matched_route_path.is_none());
}

#[tokio::test]
async fn proxy_context_finishes_only_requests_started_upstream() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager.clone()));

    let app = manager.register_app(AppConfig {
        name: "test-app".to_string(),
        ..Default::default()
    });
    lb.register_app(app.clone());

    let instance = app.allocate_instance();
    app.set_instance_state(&instance, InstanceState::Healthy);

    let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(
        crate::scaling::ColdStartConfig::default(),
    ));
    let proxy = TakoProxy::new(
        lb.clone(),
        routes,
        ProxyConfig::default(),
        cold_start,
        CloudflareIpRanges::default(),
    );

    let mut ctx = proxy.new_ctx();
    ctx.backend = lb.get_backend("test-app");
    ctx.finish_backend_request();
    assert_eq!(instance.in_flight(), 0);

    ctx.mark_backend_request_started();
    assert_eq!(instance.in_flight(), 1);
    ctx.mark_backend_request_started();
    assert_eq!(instance.in_flight(), 1);
    ctx.finish_backend_request();
    assert_eq!(instance.in_flight(), 0);
    ctx.finish_backend_request();
    assert_eq!(instance.in_flight(), 0);
}

#[test]
fn test_tako_proxy_with_acme() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager));
    let tokens: ChallengeTokens = Arc::new(RwLock::new(HashMap::new()));

    let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(
        crate::scaling::ColdStartConfig::default(),
    ));
    let proxy = TakoProxy::with_acme(
        lb,
        routes,
        ProxyConfig::default(),
        tokens,
        cold_start,
        CloudflareIpRanges::default(),
    );
    assert!(proxy.challenge_handler.is_some());
}

#[test]
fn test_proxy_config_default() {
    let config = ProxyConfig::default();
    assert_eq!(config.http_port, 80);
    assert_eq!(config.https_port, 443);
    assert!(config.enable_https);
    assert!(!config.dev_mode);
    assert!(config.redirect_http_to_https);
    assert!(config.response_cache.is_none());
    assert!(!config.trusted_proxy.proxy_protocol);
    assert!(config.trusted_proxy.trusted_cidrs.is_empty());
    assert!(config.trusted_proxy.client_ip_headers.is_empty());
}

#[test]
fn proxy_metrics_enabled_follows_metrics_port() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager));
    let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig::default()));

    let enabled = TakoProxy::new(
        lb.clone(),
        routes.clone(),
        ProxyConfig {
            metrics_port: Some(9898),
            ..Default::default()
        },
        cold_start.clone(),
        CloudflareIpRanges::default(),
    );
    assert!(enabled.metrics_enabled());

    let disabled = TakoProxy::new(
        lb,
        routes,
        ProxyConfig {
            metrics_port: None,
            ..Default::default()
        },
        cold_start,
        CloudflareIpRanges::default(),
    );
    assert!(!disabled.metrics_enabled());
}

#[test]
fn proxy_does_not_install_default_downstream_compression_module() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager));
    let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig::default()));
    let proxy = TakoProxy::new(
        lb,
        routes,
        ProxyConfig::default(),
        cold_start,
        CloudflareIpRanges::default(),
    );
    let mut modules = pingora_core::modules::http::HttpModules::new();

    proxy.init_downstream_modules(&mut modules);
    let module_ctx = modules.build_ctx();

    assert!(
        module_ctx
            .get::<pingora_core::modules::http::compression::ResponseCompression>()
            .is_none()
    );
}

#[test]
fn request_context_skips_metric_timers_when_metrics_are_disabled() {
    let mut ctx = service::RequestCtx {
        backend: None,
        backend_request_started: false,
        is_https: false,
        matched_route_path: None,
        request_timer: None,
        client_ip: None,
        body_bytes_received: 0,
        upstream_start: None,
        compression: super::compression::ResponseCompression::new(),
        observation: super::observe::RequestObservation::new(),
    };

    ctx.start_request_metrics("test-app", false);
    ctx.start_upstream_metrics(false);

    assert!(ctx.request_timer.is_none());
    assert!(ctx.upstream_start.is_none());
}

#[test]
fn listener_socket_options_enable_reuseport() {
    let options = listener_socket_options(None);
    assert_eq!(options.so_reuseport, Some(true));
    assert_eq!(options.ipv6_only, None);
}

#[test]
fn public_listener_endpoints_bind_ipv4_and_ipv6_only() {
    let endpoints = public_listener_endpoints(443);

    assert_eq!(endpoints[0].addr, "0.0.0.0:443");
    assert_eq!(endpoints[0].options.so_reuseport, Some(true));
    assert_eq!(endpoints[0].options.ipv6_only, None);
    assert_eq!(endpoints[1].addr, "[::]:443");
    assert_eq!(endpoints[1].options.so_reuseport, Some(true));
    assert_eq!(endpoints[1].options.ipv6_only, Some(true));
}

#[test]
fn test_create_tls_settings_dev_mode() {
    let temp = TempDir::new().unwrap();
    let config = ProxyConfig {
        cert_dir: temp.path().to_path_buf(),
        dev_mode: true,
        ..Default::default()
    };

    let settings = create_tls_settings(&config, None).unwrap();
    assert!(settings.is_some());
}

#[test]
fn test_create_tls_settings_no_cert() {
    let temp = TempDir::new().unwrap();
    let config = ProxyConfig {
        cert_dir: temp.path().to_path_buf(),
        dev_mode: false, // Not dev mode, requires real certs
        ..Default::default()
    };

    let settings = create_tls_settings(&config, None).unwrap();
    assert!(settings.is_none()); // No default cert exists
}

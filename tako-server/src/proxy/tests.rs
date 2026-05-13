use super::request::{
    forwarded_header_has_proto, forwarded_header_proto_is_https, https_redirect_host,
    is_request_forwarded_https, strip_route_prefix_for_static_lookup, x_forwarded_proto_is_https,
};
use super::server::{create_tls_settings, listener_socket_options};
use super::*;
use crate::instances::{AppConfig, AppManager};
use crate::scaling::ColdStartConfig;
use crate::socket::InstanceState;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

#[test]
fn test_tako_proxy_creation() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager));
    let routes = Arc::new(tokio::sync::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(
        crate::scaling::ColdStartConfig::default(),
    ));
    let proxy = TakoProxy::new(lb, routes, ProxyConfig::default(), cold_start);

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
    instance.set_state(InstanceState::Healthy);

    let routes = Arc::new(tokio::sync::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(
        crate::scaling::ColdStartConfig::default(),
    ));
    let proxy = TakoProxy::new(lb.clone(), routes, ProxyConfig::default(), cold_start);

    let mut ctx = proxy.new_ctx();
    ctx.backend = lb.get_backend("test-app");
    ctx.finish_backend_request();
    assert_eq!(instance.in_flight(), 0);

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

    let routes = Arc::new(tokio::sync::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(
        crate::scaling::ColdStartConfig::default(),
    ));
    let proxy = TakoProxy::with_acme(lb, routes, ProxyConfig::default(), tokens, cold_start);
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
    assert!(config.response_cache.is_some());
}

#[test]
fn listener_socket_options_enable_reuseport() {
    let options = listener_socket_options();
    assert_eq!(options.so_reuseport, Some(true));
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

#[test]
fn test_should_redirect_http_request_when_http_and_enabled() {
    assert!(should_redirect_http_request(false, true));
}

#[test]
fn https_redirect_host_replaces_public_http_port() {
    assert_eq!(
        https_redirect_host("example.com:8080", 8443),
        "example.com:8443"
    );
    assert_eq!(https_redirect_host("example.com:8080", 443), "example.com");
    assert_eq!(
        https_redirect_host("[fd7a:115c:a1e0::1]:8080", 8443),
        "[fd7a:115c:a1e0::1]:8443"
    );
}

#[test]
fn test_should_not_redirect_http_request_when_already_https() {
    assert!(!should_redirect_http_request(true, true));
}

#[test]
fn test_should_not_redirect_http_request_when_disabled() {
    assert!(!should_redirect_http_request(false, false));
}

#[test]
fn test_should_not_redirect_http_request_when_forwarded_proto_is_https() {
    assert!(is_request_forwarded_https(Some("https"), None));
    assert!(!should_redirect_http_request(true, true));
}

#[test]
fn test_should_not_redirect_http_request_when_forwarded_header_proto_is_https() {
    assert!(is_request_forwarded_https(
        None,
        Some("for=192.0.2.60;proto=https;by=203.0.113.43")
    ));
    assert!(!should_redirect_http_request(true, true));
}

#[test]
fn request_is_cacheable_for_get_and_head_without_upgrade() {
    let get = RequestHeader::build("GET", b"/assets/app.js", None).expect("build request");
    let head = RequestHeader::build("HEAD", b"/assets/app.js", None).expect("build request");

    assert!(request_is_proxy_cacheable(&get));
    assert!(request_is_proxy_cacheable(&head));
}

#[test]
fn request_is_not_cacheable_for_upgrade_or_non_get_head_methods() {
    let mut post = RequestHeader::build("POST", b"/assets/app.js", None).expect("build request");
    let mut get_upgrade = RequestHeader::build("GET", b"/socket", None).expect("build request");
    get_upgrade
        .insert_header("Upgrade", "websocket")
        .expect("insert upgrade");
    post.insert_header("Content-Type", "application/json")
        .expect("insert content type");

    assert!(!request_is_proxy_cacheable(&post));
    assert!(!request_is_proxy_cacheable(&get_upgrade));
}

#[test]
fn cache_key_includes_host_and_uri() {
    let a = build_proxy_cache_key("app-a.example.com", "/assets/app.js?v=1");
    let b = build_proxy_cache_key("app-b.example.com", "/assets/app.js?v=1");
    let c = build_proxy_cache_key("app-a.example.com", "/assets/app.js?v=2");

    assert_ne!(a.to_compact().primary, b.to_compact().primary);
    assert_ne!(a.to_compact().primary, c.to_compact().primary);
}

#[test]
fn response_cacheability_requires_explicit_cache_directives() {
    let mut without_directive = ResponseHeader::build(200, Some(1)).expect("build response header");
    without_directive
        .insert_header("Content-Type", "text/plain")
        .expect("insert content type");

    let mut with_max_age = ResponseHeader::build(200, Some(2)).expect("build response header");
    with_max_age
        .insert_header("Content-Type", "text/plain")
        .expect("insert content type");
    with_max_age
        .insert_header("Cache-Control", "public, max-age=60")
        .expect("insert cache control");

    assert!(matches!(
        response_cacheability(&without_directive, false),
        pingora_cache::RespCacheable::Uncacheable(_)
    ));
    assert!(matches!(
        response_cacheability(&with_max_age, false),
        pingora_cache::RespCacheable::Cacheable(_)
    ));
}

#[test]
fn production_error_bodies_are_generic_reason_phrases() {
    assert_eq!(production_error_body(500), "Internal Server Error");
    assert_eq!(production_error_body(502), "Bad Gateway");
    assert_eq!(production_error_body(503), "Service Unavailable");
    assert_eq!(production_error_body(504), "Gateway Timeout");
}

#[test]
fn test_effective_request_https_prefers_transport_tls() {
    assert!(is_effective_request_https(true, None, None));
}

#[test]
fn test_effective_request_https_uses_forwarded_https_when_transport_is_http() {
    assert!(is_effective_request_https(false, Some("https"), None));
    assert!(is_effective_request_https(
        false,
        None,
        Some("for=192.0.2.60;proto=https")
    ));
    assert!(!is_effective_request_https(false, Some("http"), None));
}

#[test]
fn test_private_local_forwarded_request_without_proto_is_treated_as_https() {
    let inferred_https = should_assume_forwarded_private_request_https(
        "test-app.orb.local",
        Some("127.0.0.1"),
        None,
        None,
    );
    assert!(inferred_https);
}

#[test]
fn test_private_local_forwarded_request_with_proto_is_not_inferred() {
    assert!(!should_assume_forwarded_private_request_https(
        "test-app.orb.local",
        Some("127.0.0.1"),
        Some("http"),
        None,
    ));
    assert!(!should_assume_forwarded_private_request_https(
        "test-app.orb.local",
        None,
        None,
        Some("for=127.0.0.1;proto=https"),
    ));
}

#[test]
fn test_public_forwarded_request_without_proto_is_not_inferred() {
    assert!(!should_assume_forwarded_private_request_https(
        "api.example.com",
        Some("127.0.0.1"),
        None,
        None,
    ));
}

#[test]
fn test_forwarded_header_has_proto_detects_presence() {
    assert!(forwarded_header_has_proto("for=192.0.2.60;proto=https"));
    assert!(forwarded_header_has_proto(
        r#"for=192.0.2.60;proto="http";by=203.0.113.43"#
    ));
    assert!(!forwarded_header_has_proto(
        "for=192.0.2.60;by=203.0.113.43"
    ));
    assert!(!forwarded_header_has_proto(r#"for=192.0.2.60;proto="""#));
}

#[test]
fn test_x_forwarded_proto_parsing_handles_case_and_commas() {
    assert!(x_forwarded_proto_is_https("HTTPS"));
    assert!(x_forwarded_proto_is_https("https, http"));
    assert!(!x_forwarded_proto_is_https("http, https"));
}

#[test]
fn test_forwarded_header_parsing_handles_quotes_and_multiple_entries() {
    assert!(forwarded_header_proto_is_https(
        r#"for=192.0.2.60;proto="https";by=203.0.113.43"#
    ));
    assert!(forwarded_header_proto_is_https(
        "for=192.0.2.60;proto=http,for=198.51.100.17;proto=https"
    ));
    assert!(!forwarded_header_proto_is_https(
        "for=192.0.2.60;proto=http"
    ));
}

#[test]
fn body_headers_include_content_type_and_length() {
    let mut header = ResponseHeader::build(404, None).expect("build header");
    insert_body_headers(&mut header, "text/plain", "Not Found").expect("insert headers");

    assert_eq!(
        header
            .headers
            .get("Content-Type")
            .and_then(|v| v.to_str().ok()),
        Some("text/plain")
    );
    assert_eq!(
        header
            .headers
            .get("Content-Length")
            .and_then(|v| v.to_str().ok()),
        Some("9")
    );
}

#[test]
fn body_headers_use_utf8_byte_length() {
    let mut header = ResponseHeader::build(200, None).expect("build header");
    insert_body_headers(&mut header, "text/plain", "✓").expect("insert headers");

    assert_eq!(
        header
            .headers
            .get("Content-Length")
            .and_then(|v| v.to_str().ok()),
        Some("3")
    );
}

#[test]
fn test_path_looks_like_static_asset() {
    assert!(path_looks_like_static_asset("/assets/main.js"));
    assert!(path_looks_like_static_asset("/img/logo.123abc.svg"));
    assert!(!path_looks_like_static_asset("/"));
    assert!(!path_looks_like_static_asset("/dashboard/settings"));
    assert!(!path_looks_like_static_asset("/assets/main"));
}

#[test]
fn test_strip_route_prefix_for_static_lookup_with_path_wildcard() {
    let stripped =
        strip_route_prefix_for_static_lookup("/tanstack-start/assets/main.js", "/tanstack-start/*");
    assert_eq!(stripped, Some("/assets/main.js".to_string()));
}

#[test]
fn test_strip_route_prefix_for_static_lookup_with_prefix_star() {
    let stripped = strip_route_prefix_for_static_lookup("/apiv2/app.js", "/api*");
    assert_eq!(stripped, Some("/v2/app.js".to_string()));
}

#[test]
fn test_static_lookup_paths_includes_prefix_stripped_candidate() {
    let candidates =
        static_lookup_paths("/tanstack-start/assets/main.js", Some("/tanstack-start/*"));
    assert_eq!(
        candidates,
        vec![
            "/tanstack-start/assets/main.js".to_string(),
            "/assets/main.js".to_string()
        ]
    );
}

#[tokio::test]
async fn resolve_backend_waits_for_ready_on_on_demand_apps() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager.clone()));
    let app = manager.register_app(AppConfig {
        name: "test-app".to_string(),
        version: "v1".to_string(),
        min_instances: 0,
        ..Default::default()
    });
    lb.register_app(app.clone());

    let routes = Arc::new(tokio::sync::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig {
        startup_timeout: Duration::from_secs(1),
        max_queued_requests: 100,
    }));
    let proxy = TakoProxy::new(lb, routes, ProxyConfig::default(), cold_start.clone());

    let instance = app.allocate_instance();
    cold_start.begin("test-app");

    let ready_cold_start = cold_start.clone();
    let ready_instance = instance.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        ready_instance.set_state(InstanceState::Healthy);
        ready_cold_start.mark_ready("test-app");
    });

    let resolution = proxy.resolve_backend("test-app").await;
    assert!(matches!(resolution, BackendResolution::Ready(_)));
}

#[tokio::test]
async fn resolve_backend_returns_startup_timeout_after_wait_timeout() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager.clone()));
    let app = manager.register_app(AppConfig {
        name: "test-app".to_string(),
        version: "v1".to_string(),
        min_instances: 0,
        ..Default::default()
    });
    lb.register_app(app);

    let routes = Arc::new(tokio::sync::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig {
        startup_timeout: Duration::from_millis(25),
        max_queued_requests: 100,
    }));
    let proxy = TakoProxy::new(lb, routes, ProxyConfig::default(), cold_start.clone());

    cold_start.begin("test-app");

    let resolution = proxy.resolve_backend("test-app").await;
    assert!(matches!(resolution, BackendResolution::StartupTimeout));
}

#[tokio::test]
async fn resolve_backend_returns_startup_failed_when_cold_start_fails() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager.clone()));
    let app = manager.register_app(AppConfig {
        name: "test-app".to_string(),
        version: "v1".to_string(),
        min_instances: 0,
        ..Default::default()
    });
    lb.register_app(app);

    let routes = Arc::new(tokio::sync::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig {
        startup_timeout: Duration::from_secs(1),
        max_queued_requests: 100,
    }));
    let proxy = TakoProxy::new(lb, routes, ProxyConfig::default(), cold_start.clone());

    cold_start.begin("test-app");
    let failed_cold_start = cold_start.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        failed_cold_start.mark_failed("test-app", "spawn_failed");
    });

    let resolution = proxy.resolve_backend("test-app").await;
    assert!(matches!(resolution, BackendResolution::StartupFailed));
}

#[tokio::test]
async fn resolve_backend_returns_queue_full_when_cold_start_queue_is_full() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager.clone()));
    let app = manager.register_app(AppConfig {
        name: "test-app".to_string(),
        version: "v1".to_string(),
        min_instances: 0,
        ..Default::default()
    });
    lb.register_app(app);

    let routes = Arc::new(tokio::sync::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig {
        startup_timeout: Duration::from_secs(1),
        max_queued_requests: 1,
    }));
    let proxy = Arc::new(TakoProxy::new(
        lb,
        routes,
        ProxyConfig::default(),
        cold_start.clone(),
    ));

    cold_start.begin("test-app");

    let proxy_clone = proxy.clone();
    let first_request = tokio::spawn(async move { proxy_clone.resolve_backend("test-app").await });

    tokio::time::sleep(Duration::from_millis(25)).await;

    let second_request = proxy.resolve_backend("test-app").await;
    assert!(matches!(second_request, BackendResolution::QueueFull));

    cold_start.mark_failed("test-app", "spawn_failed");
    let _ = first_request.await.expect("first request should complete");
}

#[tokio::test]
async fn resolve_backend_returns_unavailable_for_non_on_demand_apps_without_backend() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager.clone()));
    let app = manager.register_app(AppConfig {
        name: "test-app".to_string(),
        version: "v1".to_string(),
        min_instances: 1,
        ..Default::default()
    });
    lb.register_app(app);

    let routes = Arc::new(tokio::sync::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig::default()));
    let proxy = TakoProxy::new(lb, routes, ProxyConfig::default(), cold_start);

    let resolution = proxy.resolve_backend("test-app").await;
    assert!(matches!(resolution, BackendResolution::Unavailable));
}

#[tokio::test]
async fn resolve_backend_returns_app_missing_when_app_not_registered() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager));

    let routes = Arc::new(tokio::sync::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig::default()));
    let proxy = TakoProxy::new(lb, routes, ProxyConfig::default(), cold_start);

    let resolution = proxy.resolve_backend("missing-app").await;
    assert!(matches!(resolution, BackendResolution::AppMissing));
}

#[tokio::test]
async fn load_balancer_cleanup_removes_stale_routes_for_app() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager));
    let routes = Arc::new(tokio::sync::RwLock::new(RouteTable::default()));
    {
        let mut table = routes.write().await;
        table.set_app_routes("test-app".to_string(), vec!["test.example.com".to_string()]);
        assert_eq!(
            table.select("test.example.com", "/"),
            Some("test-app".to_string())
        );
    }
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig::default()));
    let proxy = TakoProxy::new(lb, routes.clone(), ProxyConfig::default(), cold_start);

    proxy.load_balancer_cleanup("test-app").await;

    let table = routes.read().await;
    assert!(table.routes_for_app("test-app").is_empty());
    assert_eq!(table.select("test.example.com", "/"), None);
}

#[test]
fn static_server_for_app_reuses_cached_server_for_same_root() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager));
    let routes = Arc::new(tokio::sync::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig::default()));
    let proxy = TakoProxy::new(lb, routes, ProxyConfig::default(), cold_start);

    let root = TempDir::new().unwrap();
    let first = proxy.static_server_for_app("my-app", root.path());
    let second = proxy.static_server_for_app("my-app", root.path());

    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn static_server_for_app_replaces_cached_server_when_root_changes() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager));
    let routes = Arc::new(tokio::sync::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig::default()));
    let proxy = TakoProxy::new(lb, routes, ProxyConfig::default(), cold_start);

    let root_a = TempDir::new().unwrap();
    let root_b = TempDir::new().unwrap();
    let first = proxy.static_server_for_app("my-app", root_a.path());
    let second = proxy.static_server_for_app("my-app", root_b.path());

    assert!(!Arc::ptr_eq(&first, &second));
}

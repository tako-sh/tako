use super::*;

#[test]
fn test_path_looks_like_static_asset() {
    assert!(path_looks_like_static_asset("/assets/main.js"));
    assert!(path_looks_like_static_asset("/img/logo.123abc.svg"));
    assert!(!path_looks_like_static_asset("/"));
    assert!(!path_looks_like_static_asset("/dashboard/settings"));
    assert!(!path_looks_like_static_asset("/assets/main"));
}

#[test]
fn plain_proxy_paths_skip_tako_handler_probes() {
    assert!(!path_uses_tako_handler("/plaintext"));
    assert!(!path_uses_tako_handler("/api/users"));
    assert!(path_uses_tako_handler("/assets/main.js"));
    assert!(path_uses_tako_handler(tako_images::PUBLIC_IMAGE_BASE_PATH));
    assert!(path_uses_tako_handler(tako_channels::CHANNELS_BASE_PATH));
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

#[test]
fn static_server_for_app_reuses_cached_server_for_same_root() {
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

    let root = TempDir::new().unwrap();
    let first = proxy.static_server_for_app("my-app", root.path());
    let second = proxy.static_server_for_app("my-app", root.path());

    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn static_server_for_app_replaces_cached_server_when_root_changes() {
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

    let root_a = TempDir::new().unwrap();
    let root_b = TempDir::new().unwrap();
    let first = proxy.static_server_for_app("my-app", root_a.path());
    let second = proxy.static_server_for_app("my-app", root_b.path());

    assert!(!Arc::ptr_eq(&first, &second));
}

use super::*;

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

    let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig {
        startup_timeout: Duration::from_secs(1),
        max_queued_requests: 100,
    }));
    let proxy = TakoProxy::new(
        lb,
        routes,
        ProxyConfig::default(),
        cold_start.clone(),
        CloudflareIpRanges::default(),
    );

    let instance = app.allocate_instance();
    cold_start.begin("test-app");

    let ready_cold_start = cold_start.clone();
    let ready_app = app.clone();
    let ready_instance = instance.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        ready_app.set_instance_state(&ready_instance, InstanceState::Healthy);
        ready_cold_start.mark_ready("test-app");
    });

    let resolution = proxy.resolve_backend("test-app").await;
    assert!(matches!(resolution, BackendResolution::Ready { .. }));
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

    let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig {
        startup_timeout: Duration::from_millis(25),
        max_queued_requests: 100,
    }));
    let proxy = TakoProxy::new(
        lb,
        routes,
        ProxyConfig::default(),
        cold_start.clone(),
        CloudflareIpRanges::default(),
    );

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

    let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig {
        startup_timeout: Duration::from_secs(1),
        max_queued_requests: 100,
    }));
    let proxy = TakoProxy::new(
        lb,
        routes,
        ProxyConfig::default(),
        cold_start.clone(),
        CloudflareIpRanges::default(),
    );

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

    let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig {
        startup_timeout: Duration::from_secs(1),
        max_queued_requests: 1,
    }));
    let proxy = Arc::new(TakoProxy::new(
        lb,
        routes,
        ProxyConfig::default(),
        cold_start.clone(),
        CloudflareIpRanges::default(),
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

    let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig::default()));
    let proxy = TakoProxy::new(
        lb,
        routes,
        ProxyConfig::default(),
        cold_start,
        CloudflareIpRanges::default(),
    );

    let resolution = proxy.resolve_backend("test-app").await;
    assert!(matches!(resolution, BackendResolution::Unavailable));
}

#[tokio::test]
async fn resolve_backend_returns_app_missing_when_app_not_registered() {
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

    let resolution = proxy.resolve_backend("missing-app").await;
    assert!(matches!(resolution, BackendResolution::AppMissing));
}

#[tokio::test]
async fn load_balancer_cleanup_removes_stale_routes_for_app() {
    let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
    let lb = Arc::new(LoadBalancer::new(manager));
    let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
    {
        let mut table = routes.write();
        table.set_app_routes("test-app".to_string(), vec!["test.example.com".to_string()]);
        assert_eq!(
            table.select("test.example.com", "/"),
            Some("test-app".to_string())
        );
    }
    let cold_start = Arc::new(ColdStartManager::new(ColdStartConfig::default()));
    let proxy = TakoProxy::new(
        lb,
        routes.clone(),
        ProxyConfig::default(),
        cold_start,
        CloudflareIpRanges::default(),
    );

    proxy.load_balancer_cleanup("test-app").await;

    let table = routes.read();
    assert!(table.routes_for_app("test-app").is_empty());
    assert_eq!(table.select("test.example.com", "/"), None);
}

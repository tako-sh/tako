use super::*;

#[tokio::test]
async fn stopped_on_demand_app_cannot_be_cold_started() {
    let temp = TempDir::new().unwrap();
    let manager = Arc::new(AppManager::new(temp.path().to_path_buf()));
    let lb = Arc::new(LoadBalancer::new(manager.clone()));
    let app = manager.register_app(AppConfig {
        name: "stopped".into(),
        min_instances: 0,
        ..Default::default()
    });
    app.set_state(crate::socket::AppState::Idle);
    lb.register_app(app.clone());
    manager.stop_app("stopped").await.unwrap();
    let proxy = TakoProxy::new(
        lb,
        Arc::new(RwLock::new(RouteTable::default())),
        ProxyConfig::default(),
        Arc::new(ColdStartManager::new(ColdStartConfig {
            startup_timeout: Duration::from_secs(1),
            max_queued_requests: 1,
        })),
        CloudflareIpRanges::default(),
    );
    assert!(matches!(
        proxy.resolve_backend("stopped").await,
        BackendResolution::Unavailable
    ));
    assert_eq!(app.state(), crate::socket::AppState::Stopped);
    assert!(app.get_instances().is_empty());
    assert!(app.last_error().is_none());
}

#[tokio::test]
async fn lifecycle_mutation_blocks_routing_and_waits_for_admitted_spawns() {
    let temp = TempDir::new().unwrap();
    let manager = Arc::new(AppManager::new(temp.path().to_path_buf()));
    let lb = Arc::new(LoadBalancer::new(manager.clone()));
    let app = manager.register_app(AppConfig {
        name: "gated".into(),
        min_instances: 0,
        ..Default::default()
    });
    app.set_state(crate::socket::AppState::Idle);
    lb.register_app(app.clone());
    let proxy = TakoProxy::new(
        lb,
        Arc::new(RwLock::new(RouteTable::default())),
        ProxyConfig::default(),
        Arc::new(ColdStartManager::new(ColdStartConfig::default())),
        CloudflareIpRanges::default(),
    );
    let mutation = app.lifecycle_gate.write().await;
    assert!(matches!(
        proxy.resolve_backend("gated").await,
        BackendResolution::Unavailable
    ));
    drop(mutation);
    // The same request arriving at deployment cutover waits for the new
    // backend. Lifecycle stops above retain immediate refusal.
    app.set_state(crate::socket::AppState::Deploying);
    let cutover = app.lifecycle_gate.write().await;
    let request = proxy.resolve_backend("gated");
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut request)
            .await
            .is_err()
    );
    let candidate = app.allocate_instance();
    app.set_instance_state(&candidate, crate::socket::InstanceState::Healthy);
    drop(cutover);
    app.set_state(crate::socket::AppState::Running);
    let BackendResolution::Ready { backend, .. } = request.await else {
        panic!("cutover should release the request to the new backend");
    };
    assert_eq!(backend.instance_id(), candidate.id);

    let spawning = app.lifecycle_gate.read().await;
    let stopping = tokio::spawn(async move { manager.stop_app("gated").await });
    tokio::task::yield_now().await;
    assert!(!stopping.is_finished());
    app.allocate_instance();
    drop(spawning);
    stopping.await.unwrap().unwrap();
    assert!(app.get_instances().is_empty());
    assert!(matches!(
        proxy.resolve_backend("gated").await,
        BackendResolution::Unavailable
    ));
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
    app.set_state(crate::socket::AppState::Idle);
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
    app.set_state(crate::socket::AppState::Idle);
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
    app.set_state(crate::socket::AppState::Idle);
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
    app.set_state(crate::socket::AppState::Idle);
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
    app.set_state(crate::socket::AppState::Idle);
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

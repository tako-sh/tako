use super::logger::noop_log_handle;
use super::*;

#[test]
fn test_instance_state_transitions() {
    let instance = Instance::new("test-1".to_string(), "v1".to_string(), noop_log_handle());
    assert_eq!(instance.state(), InstanceState::Starting);

    instance.set_state(InstanceState::Ready);
    assert_eq!(instance.state(), InstanceState::Ready);

    instance.set_state(InstanceState::Healthy);
    assert_eq!(instance.state(), InstanceState::Healthy);
}

#[test]
fn instance_state_round_trips_all_variants() {
    let instance = Instance::new("test-1".to_string(), "v1".to_string(), noop_log_handle());

    for state in [
        InstanceState::Starting,
        InstanceState::Ready,
        InstanceState::Healthy,
        InstanceState::Unhealthy,
        InstanceState::Draining,
        InstanceState::Stopped,
    ] {
        instance.set_state(state);
        assert_eq!(instance.state(), state);
    }
}

#[test]
fn stop_error_display_names_stop_failure() {
    let error = InstanceError::StopError(std::io::Error::from_raw_os_error(1));

    assert!(error.to_string().starts_with("Failed to stop instance:"));
}

#[test]
fn host_instance_limit_allows_two_app_processes_per_cpu() {
    assert_eq!(host_instance_limit_for_parallelism(1), 2);
    assert_eq!(host_instance_limit_for_parallelism(2), 4);
    assert_eq!(host_instance_limit_for_parallelism(8), 16);
}

#[test]
fn effective_instance_limit_uses_lower_app_or_host_limit() {
    assert_eq!(
        effective_instance_limit(10),
        10.min(default_max_instances_for_host())
    );
    assert_eq!(effective_instance_limit(0), 0);
    assert_eq!(
        effective_instance_limit(u32::MAX),
        default_max_instances_for_host()
    );
}

#[test]
fn validate_requested_instances_rejects_above_effective_limit() {
    let limit = effective_instance_limit(4);

    assert!(validate_requested_instances(limit, 4).is_ok());
    let error = validate_requested_instances(limit.saturating_add(1), 4).unwrap_err();
    assert!(error.contains(&format!("at most {limit}")));
}

#[test]
fn test_instance_request_tracking() {
    let instance = Instance::new("test-1".to_string(), "v1".to_string(), noop_log_handle());
    assert_eq!(instance.requests_total(), 0);

    instance.request_started();
    instance.request_finished();
    instance.request_started();
    instance.request_finished();
    instance.request_started();
    instance.request_finished();

    assert_eq!(instance.requests_total(), 3);
}

#[test]
fn test_app_allocate_instances() {
    let (tx, _rx) = mpsc::channel(16);
    let config = AppConfig {
        name: "test-app".to_string(),
        version: "v1".to_string(),
        ..Default::default()
    };
    let app = App::new(config, tx, noop_log_handle());

    let i1 = app.allocate_instance();
    assert!(!i1.id.is_empty());

    let i2 = app.allocate_instance();
    assert_ne!(i1.id, i2.id);

    let i3 = app.allocate_instance();
    assert_ne!(i2.id, i3.id);
    assert!(i3.internal_token().len() >= 16);
}

#[test]
fn test_allocate_instance_tracks_build_version() {
    let (tx, _rx) = mpsc::channel(16);
    let config = AppConfig {
        name: "test-app".to_string(),
        version: "v1".to_string(),
        ..Default::default()
    };
    let app = App::new(config, tx, noop_log_handle());

    let v1_instance = app.allocate_instance();
    assert_eq!(v1_instance.build_version(), "v1");

    let mut next = app.config.read().clone();
    next.version = "v2".to_string();
    app.update_config(next);

    let v2_instance = app.allocate_instance();
    assert_eq!(v2_instance.build_version(), "v2");
}

#[test]
fn test_instance_internal_token_is_stable() {
    let instance = Instance::new("test-1".to_string(), "v1".to_string(), noop_log_handle());
    let token = instance.internal_token().to_string();
    assert!(!token.is_empty());
    assert_eq!(instance.internal_token(), token);
}

#[test]
fn app_config_defaults_internal_host_to_app_scoped_tako_host() {
    let (tx, _rx) = mpsc::channel(16);
    let app = App::new(
        AppConfig {
            name: "demo".to_string(),
            version: "v1".to_string(),
            ..Default::default()
        },
        tx,
        noop_log_handle(),
    );

    assert_eq!(app.config.read().health_check_host, "demo.tako");
}

#[test]
fn internal_app_host_for_deployment_id_uses_base_app_name() {
    assert_eq!(internal_app_host_for_app_id("demo/production"), "demo.tako");
}

#[test]
fn test_instance_port_round_trips() {
    let instance = Instance::new("test-1".to_string(), "v1".to_string(), noop_log_handle());
    assert_eq!(instance.port(), None);
    instance.set_port(48_123);
    assert_eq!(instance.port(), Some(48_123));
}

#[tokio::test]
async fn test_app_manager_register() {
    let manager = AppManager::new(PathBuf::from("/tmp/tako-test"));

    let config = AppConfig {
        name: "my-app".to_string(),
        version: "1.0.0".to_string(),
        ..Default::default()
    };

    manager.register_app(config);

    let app = manager.get_app("my-app").unwrap();
    assert_eq!(app.name(), "my-app");
    assert_eq!(app.version(), "1.0.0");

    let apps = manager.list_apps();
    assert_eq!(apps.len(), 1);
    assert!(apps.contains(&"my-app".to_string()));
}

#[tokio::test]
async fn app_manager_shutdown_all_stops_registered_instances() {
    let dir = tempfile::tempdir().unwrap();
    let manager = AppManager::new(dir.path().to_path_buf());

    let app = manager.register_app(AppConfig {
        name: "my-app".to_string(),
        version: "1.0.0".to_string(),
        ..Default::default()
    });

    let instance = app.allocate_instance();
    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn test child");
    instance.set_process(child);
    assert!(instance.is_alive().await);

    manager.shutdown_all().await;

    assert!(app.get_instances().is_empty());
    assert_eq!(app.state(), AppState::Stopped);
}

#[test]
fn test_get_healthy_instances() {
    let (tx, _rx) = mpsc::channel(16);
    let config = AppConfig {
        name: "test-app".to_string(),
        ..Default::default()
    };
    let app = App::new(config, tx, noop_log_handle());

    let i1 = app.allocate_instance();
    let i2 = app.allocate_instance();
    let i3 = app.allocate_instance();

    app.set_instance_state(&i1, InstanceState::Healthy);
    app.set_instance_state(&i2, InstanceState::Starting);
    app.set_instance_state(&i3, InstanceState::Healthy);

    let healthy = app.get_healthy_instances();
    assert_eq!(healthy.len(), 2);
}

#[test]
fn healthy_instance_count_ignores_non_healthy_instances() {
    let (tx, _rx) = mpsc::channel(16);
    let app = App::new(AppConfig::default(), tx, noop_log_handle());

    let healthy = app.allocate_instance();
    app.set_instance_state(&healthy, InstanceState::Healthy);
    let ready = app.allocate_instance();
    app.set_instance_state(&ready, InstanceState::Ready);
    let unhealthy = app.allocate_instance();
    app.set_instance_state(&unhealthy, InstanceState::Unhealthy);

    assert_eq!(app.healthy_instance_count(), 1);
}

#[test]
fn active_instance_list_tracks_state_changes_and_removal() {
    let (tx, _rx) = mpsc::channel(16);
    let app = App::new(AppConfig::default(), tx, noop_log_handle());

    let instance = app.allocate_instance();
    assert_eq!(app.healthy_instance_count(), 0);

    app.set_instance_state(&instance, InstanceState::Healthy);
    assert_eq!(app.healthy_instance_count(), 1);

    app.set_instance_state(&instance, InstanceState::Healthy);
    assert_eq!(app.healthy_instance_count(), 1);

    app.set_instance_state(&instance, InstanceState::Draining);
    assert_eq!(app.healthy_instance_count(), 0);

    app.set_instance_state(&instance, InstanceState::Healthy);
    assert_eq!(app.healthy_instance_count(), 1);

    app.remove_instance(&instance.id);
    assert_eq!(app.healthy_instance_count(), 0);
}

#[test]
fn healthy_instance_at_returns_only_healthy_instances() {
    let (tx, _rx) = mpsc::channel(16);
    let app = App::new(AppConfig::default(), tx, noop_log_handle());

    let ready = app.allocate_instance();
    app.set_instance_state(&ready, InstanceState::Ready);
    let healthy = app.allocate_instance();
    app.set_instance_state(&healthy, InstanceState::Healthy);
    let unhealthy = app.allocate_instance();
    app.set_instance_state(&unhealthy, InstanceState::Unhealthy);

    let selected = app
        .healthy_instance_at(0)
        .expect("one healthy instance should be selectable");

    assert_eq!(selected.id, healthy.id);
    assert!(app.healthy_instance_at(1).is_none());
}

#[test]
fn has_starting_instance_detects_startup_states_without_snapshotting() {
    let (tx, _rx) = mpsc::channel(16);
    let app = App::new(AppConfig::default(), tx, noop_log_handle());

    assert!(!app.has_starting_instance());

    app.allocate_instance().set_state(InstanceState::Healthy);
    assert!(!app.has_starting_instance());

    app.allocate_instance().set_state(InstanceState::Ready);
    assert!(app.has_starting_instance());
}

#[test]
fn app_last_error_roundtrip() {
    let (tx, _rx) = mpsc::channel(1);
    let app = App::new(AppConfig::default(), tx, noop_log_handle());
    assert_eq!(app.last_error(), None);

    app.set_last_error("boom");
    assert_eq!(app.last_error(), Some("boom".to_string()));

    app.clear_last_error();
    assert_eq!(app.last_error(), None);
}

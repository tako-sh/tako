use super::*;
use crate::instances::AppConfig;
use crate::isolation::fixture::TestDataDir as TempDir;
use crate::tls::{CertManager, CertManagerConfig};
use crate::{ServerRuntimeConfig, ServerState};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

fn open_state(path: &Path) -> Arc<ServerState> {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        Arc::new(
            ServerState::new_with_runtime(
                path.clone(),
                Arc::new(CertManager::new(CertManagerConfig {
                    cert_dir: path.join("certs"),
                    ..Default::default()
                })),
                None,
                Arc::new(parking_lot::RwLock::new(HashMap::new())),
                ServerRuntimeConfig::for_defaults(path),
            )
            .unwrap(),
        )
    })
    .join()
    .unwrap()
}

fn fixture() -> (TempDir, Arc<ServerState>) {
    let dir = TempDir::new().unwrap();
    let state = open_state(dir.path());
    (dir, state)
}

fn app(state: &ServerState, name: &str) -> Arc<crate::instances::App> {
    let app = state.app_manager.register_app(AppConfig {
        name: name.into(),
        environment: "production".into(),
        version: "v1".into(),
        min_instances: 0,
        ..Default::default()
    });
    state.load_balancer.register_app(app.clone());
    app
}

async fn wait_for_gate(state: &ServerState) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !state.is_draining() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn drain_withdraws_all_instances_before_waiting_for_requests() {
    let (_dir, state) = fixture();
    let first = app(&state, "first");
    let second = app(&state, "second");
    let a = first.allocate_instance();
    let b = second.allocate_instance();
    first.set_instance_state(&a, InstanceState::Healthy);
    second.set_instance_state(&b, InstanceState::Healthy);
    a.request_started();
    b.request_started();
    let task = tokio::spawn({
        let state = state.clone();
        async move {
            crate::management_http::handle_rpc_command(
                state,
                Command::DrainServer { timeout_seconds: 2 },
            )
            .await
        }
    });
    wait_for_gate(&state).await;
    assert!(state.state_store.drain_requested().unwrap());
    assert_eq!(a.state(), InstanceState::Draining);
    assert_eq!(b.state(), InstanceState::Draining);
    assert!(!task.is_finished());
    a.request_finished();
    b.request_finished();
    let response = task.await.unwrap();
    let report = response.data().unwrap();
    assert_eq!(report["completed"], true);
    assert_eq!(report["timed_out"], false);
    assert_eq!(report["forced_http_requests"], 0);
    assert!(first.get_instances().is_empty());
    assert!(second.get_instances().is_empty());
}

#[tokio::test]
async fn all_apps_share_one_deadline_and_force_is_reported() {
    let (_dir, state) = fixture();
    for index in 0..16 {
        app(&state, &format!("app{index}"))
            .allocate_instance()
            .request_started();
    }
    let report = tokio::time::timeout(
        Duration::from_millis(250),
        state.drain_runtime_until(Instant::now() + Duration::from_millis(25)),
    )
    .await
    .unwrap();
    assert!(report.completed);
    assert!(report.timed_out);
    assert!(!report.http_drained);
    assert_eq!(report.forced_instances, 16);
    assert_eq!(report.forced_http_requests, 16);
}

#[tokio::test]
async fn disconnect_cannot_cancel_drain_or_release_management_lock() {
    let (_dir, state) = fixture();
    let app = app(&state, "app");
    let instance = app.allocate_instance();
    instance.request_started();
    let client = tokio::spawn({
        let state = state.clone();
        async move {
            crate::management_http::handle_rpc_command(
                state,
                Command::DrainServer { timeout_seconds: 2 },
            )
            .await
        }
    });
    wait_for_gate(&state).await;
    client.abort();
    assert!(client.await.unwrap_err().is_cancelled());
    assert!(state.management_lock.try_lock().is_err());
    instance.request_finished();
    tokio::time::timeout(Duration::from_secs(2), async {
        let _guard = state.management_lock.lock().await;
    })
    .await
    .unwrap();
    assert!(app.get_instances().is_empty());
    assert!(state.is_draining());
}

#[tokio::test]
async fn failed_persistence_does_not_withdraw_instances() {
    let (_dir, state) = fixture();
    let app = app(&state, "app");
    let instance = app.allocate_instance();
    app.set_instance_state(&instance, InstanceState::Healthy);
    rusqlite::Connection::open(state.state_store.path())
        .unwrap()
        .execute_batch("DROP TABLE server_drain")
        .unwrap();
    let response = state.drain_server(0).await;
    assert!(!response.is_ok());
    assert!(!state.drain_flag().load(Ordering::Acquire));
    assert_eq!(instance.state(), InstanceState::Healthy);
    assert!(
        state.is_draining(),
        "unreadable shared intent must fail closed"
    );
}

#[tokio::test]
async fn restart_preserves_gate_and_resume_failure_stays_drained() {
    let (dir, state) = fixture();
    let app = app(&state, "missing-release");
    let config = app.config.read().clone();
    state
        .state_store
        .upsert_app(&config, &["app.test".into()])
        .unwrap();
    assert!(state.drain_server(0).await.is_ok());
    drop(state);
    let state = open_state(dir.path());
    assert!(state.is_draining());
    state.restore_from_state_store().await.unwrap();
    assert!(state.app_manager.list_apps().is_empty());
    assert!(!state.workflows.has(&config.deployment_id()));
    let response = state.resume_server().await;
    assert!(!response.is_ok());
    assert!(state.is_draining());
    assert!(state.state_store.drain_requested().unwrap());
}

#[tokio::test]
async fn empty_resume_is_durable_and_idempotent() {
    let (dir, state) = fixture();
    assert!(!state.drain_server(u64::MAX).await.is_ok());
    assert!(!state.is_draining());
    assert!(state.drain_server(0).await.is_ok());
    assert!(state.resume_server().await.is_ok());
    assert!(!state.is_draining());
    assert!(!state.state_store.drain_requested().unwrap());
    assert_eq!(
        state.resume_server().await.data().unwrap()["already_running"],
        true
    );
    drop(state);
    assert!(!open_state(dir.path()).is_draining());
}

#[tokio::test]
async fn drained_dispatch_rejects_new_work_but_allows_inspection_and_cleanup() {
    let (_dir, state) = fixture();
    state.drain_server(0).await;
    let response = state
        .handle_command(Command::Scale {
            app: "app/production".into(),
            instances: Some(0),
            service: None,
            reset: false,
        })
        .await;
    assert!(
        response
            .error_message()
            .unwrap()
            .contains("Server is drained")
    );
    assert!(state.handle_command(Command::ServerInfo).await.is_ok());
    assert!(state.handle_command(Command::List).await.is_ok());
    for cmd in [
        Command::Stop {
            app: "app/production".into(),
        },
        Command::Delete {
            app: "app/production".into(),
        },
        Command::ResumeServer,
    ] {
        assert!(!blocked_while_draining(&cmd));
    }
}

#[tokio::test]
async fn resume_restores_persisted_idle_config_routes_and_secrets() {
    let (dir, state) = fixture();
    let app = app(&state, "idle");
    let config = app.config.read().clone();
    let release = dir.path().join("apps/idle/production/releases/v1");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(
        release.join("app.json"),
        serde_json::to_vec(&serde_json::json!({
            "protocol_version":0,"runtime":"go","main":"app","start":["/bin/true"],"app_dir":"","idle_timeout":300
        }))
        .unwrap(),
    )
    .unwrap();
    state
        .state_store
        .upsert_app(&config, &["idle.test".into()])
        .unwrap();
    state
        .state_store
        .set_secrets(
            "idle/production",
            &HashMap::from([("TOKEN".into(), "stored".into())]),
        )
        .unwrap();
    state.drain_server(0).await;
    drop(state);
    let state = open_state(dir.path());
    state.restore_from_state_store().await.unwrap();
    assert!(state.app_manager.list_apps().is_empty());
    let response = state.resume_server().await;
    assert!(response.is_ok(), "{response:?}");
    let app = state.app_manager.get_app("idle/production").unwrap();
    assert_eq!(app.state(), AppState::Idle);
    assert!(app.get_instances().is_empty());
    assert_eq!(app.config.read().secrets["TOKEN"], "stored");
    assert_eq!(
        state.routes.read().routes_for_app("idle/production"),
        vec!["idle.test"]
    );
    assert!(!state.is_draining());
}

#[tokio::test]
async fn deadline_keeps_process_ownership_for_retry_cleanup() {
    let (_dir, state) = fixture();
    let app = app(&state, "native");
    let instance = app.allocate_instance();
    instance.set_process(
        tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .unwrap(),
    );
    instance.request_started();
    let response = state.drain_server(0).await;
    let report = response.data().unwrap();
    assert_eq!(report["forced_instances"], 1);
    assert_eq!(report["timed_out"], true);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let response = state.drain_server(0).await;
            if response.data().unwrap()["completed"] == true {
                break;
            }
            assert_eq!(instance.state(), InstanceState::Draining);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(app.get_instances().is_empty());
    assert!(instance.take_process().is_none());
}

#[tokio::test]
async fn resume_does_not_reactivate_uncommitted_app_configuration() {
    let (_dir, state) = fixture();
    let app = app(&state, "uncommitted");
    state.routes.write().set_app_routes_with_source_ip(
        app.name(),
        vec!["orphan.test".into()],
        tako_core::SourceIpMode::Auto,
    );
    state.drain_server(0).await;
    assert!(state.resume_server().await.is_ok());
    assert!(state.app_manager.list_apps().is_empty());
    assert!(
        state
            .routes
            .read()
            .routes_for_app("uncommitted/production")
            .is_empty()
    );
}

#[tokio::test]
async fn shared_drain_cannot_complete_while_another_runtime_holds_activity() {
    let (dir, first) = fixture();
    let second = open_state(dir.path());
    let app = app(&second, "standby");
    let instance = app.allocate_instance();
    app.set_instance_state(&instance, InstanceState::Healthy);
    let response = first.drain_server(0).await;
    let report = response.data().unwrap();
    assert_eq!(report["local_completed"], true);
    assert_eq!(report["completed"], false);
    assert_eq!(report["other_processes_quiescent"], false);
    assert_eq!(report["timed_out"], true);
    assert_eq!(report["counts_scope"], "local_process");
    assert!(second.is_draining());
    second.observe_drain_once().await.unwrap();
    assert!(app.get_instances().is_empty());
    let response = first.drain_server(0).await;
    assert_eq!(response.data().unwrap()["completed"], true);
    assert_eq!(response.data().unwrap()["other_processes_quiescent"], true);
}

#[tokio::test]
async fn shared_drain_waits_for_remote_requests_and_serializes_observer_with_deployment() {
    let (dir, first) = fixture();
    let second = open_state(dir.path());
    let management = second.management_lock.lock().await;
    let drain = tokio::spawn({
        let first = first.clone();
        async move {
            crate::management_http::handle_rpc_command(
                first,
                Command::DrainServer { timeout_seconds: 2 },
            )
            .await
        }
    });
    wait_for_gate(&first).await;
    let observer = tokio::spawn({
        let second = second.clone();
        async move { second.observe_drain_once().await }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !second.drain_flag().load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Simulate a deployment admitted before drain and still holding management_lock.
    let app = app(&second, "last-deployment");
    let instance = app.allocate_instance();
    app.set_instance_state(&instance, InstanceState::Healthy);
    instance.request_started();
    assert!(!observer.is_finished());
    assert!(!drain.is_finished());
    drop(management);
    tokio::time::timeout(Duration::from_secs(1), async {
        while instance.state() != InstanceState::Draining {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!drain.is_finished());
    instance.request_finished();
    observer.await.unwrap().unwrap();
    let response = drain.await.unwrap();
    assert_eq!(response.data().unwrap()["completed"], true);
    assert_eq!(response.data().unwrap()["timed_out"], false);
    assert!(app.get_instances().is_empty());
}

#[tokio::test]
async fn shared_resume_waits_for_quiescence_and_every_observer_reacquires_activity() {
    let (dir, first) = fixture();
    let second = open_state(dir.path());
    assert_eq!(
        first.drain_server(0).await.data().unwrap()["completed"],
        false
    );
    assert!(!first.resume_server().await.is_ok());
    assert!(first.state_store.drain_requested().unwrap());
    second.observe_drain_once().await.unwrap();
    assert!(first.resume_server().await.is_ok());
    assert!(!first.is_draining());
    assert!(second.drain_flag().load(Ordering::Acquire));
    second.observe_drain_once().await.unwrap();
    assert!(!second.is_draining());
    assert!(first.drain_activity.lock().is_some());
    assert!(second.drain_activity.lock().is_some());
    assert_eq!(
        first.drain_server(0).await.data().unwrap()["completed"],
        false
    );
    second.observe_drain_once().await.unwrap();
    assert_eq!(
        first.drain_server(0).await.data().unwrap()["completed"],
        true
    );
}

#[tokio::test]
async fn restart_during_shared_drain_is_gated_and_cannot_reopen_activity() {
    let (dir, first) = fixture();
    let second = open_state(dir.path());
    assert_eq!(
        first.drain_server(0).await.data().unwrap()["completed"],
        false
    );
    let restarted = open_state(dir.path());
    assert!(restarted.is_draining());
    assert!(restarted.drain_activity.lock().is_none());
    restarted.restore_from_state_store().await.unwrap();
    assert!(restarted.app_manager.list_apps().is_empty());
    drop(second);
    assert_eq!(
        first.drain_server(0).await.data().unwrap()["completed"],
        true
    );
    assert!(first.resume_server().await.is_ok());
    restarted.observe_drain_once().await.unwrap();
    assert!(!restarted.is_draining());
    assert!(restarted.drain_activity.lock().is_some());
}

#[tokio::test]
async fn competing_operation_returns_without_waiting_under_local_management_lock() {
    let (dir, first) = fixture();
    let second = open_state(dir.path());
    let operation = first.state_store.try_drain_operation().unwrap().unwrap();
    let response = tokio::time::timeout(
        Duration::from_millis(100),
        crate::management_http::handle_rpc_command(
            second.clone(),
            Command::DrainServer { timeout_seconds: 1 },
        ),
    )
    .await
    .unwrap();
    assert!(!response.is_ok());
    assert!(!first.state_store.drain_requested().unwrap());
    assert!(second.management_lock.try_lock().is_ok());
    drop(operation);
}

#[tokio::test]
async fn prelock_drain_persists_and_closes_gate_while_preparation_owns_management() {
    let (_dir, state) = fixture();
    let management = state.management_lock.lock().await;
    let response = tokio::time::timeout(
        Duration::from_millis(100),
        state.drain_server_before_management(0),
    )
    .await
    .unwrap();
    assert_eq!(response.data().unwrap()["completed"], false);
    assert_eq!(response.data().unwrap()["timed_out"], true);
    assert!(state.state_store.drain_requested().unwrap());
    assert!(state.drain_flag().load(Ordering::Acquire));
    assert!(state.drain_activity.lock().is_some());
    // A preparation already admitted can finish unwinding before observer cleanup.
    let app = app(&state, "finishing-preparation");
    app.allocate_instance();
    drop(management);
    state.observe_drain_once().await.unwrap();
    assert!(app.get_instances().is_empty());
    assert!(state.drain_activity.lock().is_none());
    assert_eq!(
        state
            .drain_server_before_management(0)
            .await
            .data()
            .unwrap()["completed"],
        true
    );
}

#[tokio::test]
async fn proxy_guard_without_transport_keeps_activity_after_force_until_it_drops() {
    let (_dir, state) = fixture();
    let request = state.proxy_requests.register(None).unwrap();
    let response = state.drain_server(0).await;
    let report = response.data().unwrap();
    assert_eq!(report["completed"], false);
    assert_eq!(report["forced_proxy_requests"], 1);
    assert_eq!(report["forced_proxy_transports"], 0);
    assert_eq!(report["remaining_proxy_requests"], 1);
    assert!(state.drain_activity.lock().is_some());
    assert!(state.proxy_requests.register(None).is_none());
    assert!(!state.resume_server().await.is_ok());
    assert!(state.state_store.drain_requested().unwrap());
    drop(request);
    state.observe_drain_once().await.unwrap();
    assert!(state.drain_activity.lock().is_none());
    assert!(state.resume_server().await.is_ok());
    assert!(state.proxy_requests.register(None).is_some());
}

#[tokio::test]
async fn remote_proxy_guard_prevents_global_completion_after_local_apps_stop() {
    let (dir, first) = fixture();
    let second = open_state(dir.path());
    let request = second.proxy_requests.register(None).unwrap();
    assert_eq!(
        first.drain_server(0).await.data().unwrap()["completed"],
        false
    );
    assert!(second.observe_drain_once().await.is_err());
    assert_eq!(second.proxy_requests.active_count(), 1);
    assert!(second.drain_activity.lock().is_some());
    assert_eq!(
        first.drain_server(0).await.data().unwrap()["completed"],
        false
    );
    drop(request);
    second.observe_drain_once().await.unwrap();
    assert_eq!(
        first.drain_server(0).await.data().unwrap()["completed"],
        true
    );
    let restarted = open_state(dir.path());
    assert!(restarted.proxy_requests.register(None).is_none());
}

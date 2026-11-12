use super::*;
use crate::instances::{AppConfig, HealthChecker, HealthConfig, Instance};
use crate::socket::{Command, Response};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Drains the fd-3 bootstrap, reports a port on fd 4, then stays up.
const READY: &str = "cat <&3 >/dev/null; echo 43210 >&4; exec sleep 30";

fn sh(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

fn test_state(dir: &Path) -> Arc<ServerState> {
    Arc::new(
        ServerState::new(
            dir.into(),
            Arc::new(crate::tls::CertManager::new(
                crate::tls::CertManagerConfig {
                    cert_dir: dir.join("certs"),
                    ..Default::default()
                },
            )),
            None,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
        )
        .unwrap(),
    )
}

fn running_app(
    state: &ServerState,
    dir: &Path,
    name: &str,
    min_instances: u32,
    command: Vec<String>,
) -> Arc<App> {
    let app = state.app_manager.register_app(AppConfig {
        name: name.into(),
        environment: "production".into(),
        version: "v1".into(),
        path: dir.into(),
        command,
        min_instances,
        ..Default::default()
    });
    app.set_state(AppState::Running);
    app
}

fn healthy_instance(app: &App) -> Arc<Instance> {
    let instance = app.allocate_instance();
    app.set_instance_state(&instance, InstanceState::Healthy);
    instance
}

/// What the health checker does when it finds an instance dead.
fn dead(app: &App, instance: &Arc<Instance>) -> HealthEvent {
    app.set_instance_health_state(instance, InstanceState::Stopped);
    HealthEvent::Dead {
        app: app.name(),
        instance_id: instance.id.clone(),
    }
}

fn live(app: &App) -> usize {
    app.get_instances()
        .iter()
        .filter(|instance| {
            matches!(
                instance.state(),
                InstanceState::Starting | InstanceState::Ready | InstanceState::Healthy
            )
        })
        .count()
}

/// One reconcile tick, awaited so assertions see its outcome.
async fn tick(state: &Arc<ServerState>, now: Instant) {
    for task in reconcile_instances(state, now) {
        tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("reconcile did not finish")
            .unwrap();
    }
}

/// A tick late enough that any backoff recorded so far has expired.
async fn tick_after_backoff(state: &Arc<ServerState>) {
    tick(state, Instant::now() + Duration::from_secs(31)).await;
}

fn restart_count(app: &App) -> u32 {
    app.recovery_status()
        .map_or(0, |status| status.restart_count)
}

#[tokio::test]
async fn failed_replacement_is_retried_until_the_app_is_back_at_its_minimum() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    let app = running_app(&state, dir.path(), "retry", 1, vec!["./missing-app".into()]);
    let instance = healthy_instance(&app);

    handle_health_event(&state, dead(&app, &instance)).await;
    // The replacement could not start and was removed. Nothing is left for
    // the health loop to notice.
    assert!(app.get_instances().is_empty());
    assert_eq!(app.state(), AppState::Running);

    // The cause goes away (think: a dependency came back). No operator acts.
    app.config.write().command = sh(READY);
    tick_after_backoff(&state).await;

    assert_eq!(live(&app), 1, "reconcile did not restore the minimum");
    assert_eq!(app.get_instances()[0].state(), InstanceState::Healthy);
    state.app_manager.stop_app(&app.name()).await.unwrap();
}

#[tokio::test]
async fn replacement_skipped_under_a_contended_gate_is_retried_later() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());

    for gate in ["spawn", "lifecycle", "recovery"] {
        let app = running_app(&state, dir.path(), &format!("gated-{gate}"), 1, sh(READY));
        let instance = healthy_instance(&app);

        let spawn_guard = (gate == "spawn").then(|| state.spawn_gate.try_write().unwrap());
        let lifecycle_guard =
            (gate == "lifecycle").then(|| app.lifecycle_gate.try_write().unwrap());
        let recovery_guard = (gate == "recovery").then(|| app.recovery_gate.try_lock().unwrap());

        handle_health_event(&state, dead(&app, &instance)).await;
        tick(&state, Instant::now()).await;
        assert!(
            app.get_instance(&instance.id).is_some() && live(&app) == 0,
            "{gate}: recovery must not act under a contended gate"
        );

        drop((spawn_guard, lifecycle_guard, recovery_guard));
        tick(&state, Instant::now()).await;
        assert!(
            app.get_instance(&instance.id).is_none(),
            "{gate}: the dead instance was never cleaned up"
        );
        assert_eq!(
            live(&app),
            1,
            "{gate}: the skipped replacement was never retried"
        );
        // The skipped loss is a first loss: replaced without delay, and reported.
        let status = app.recovery_status().unwrap();
        assert_eq!(status.restart_count, 1, "{gate}");
        assert!(status.last_exit.is_some(), "{gate}");
        assert_eq!(status.next_restart_at_unix_secs, None, "{gate}");
        state.app_manager.stop_app(&app.name()).await.unwrap();
    }
}

#[tokio::test]
async fn repeated_failures_back_off_and_a_stable_app_resets() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    let app = running_app(
        &state,
        dir.path(),
        "backoff",
        1,
        vec!["./missing-app".into()],
    );
    let instance = healthy_instance(&app);

    // First loss: replaced at once. That replacement fails to start.
    handle_health_event(&state, dead(&app, &instance)).await;
    assert_eq!(restart_count(&app), 1);
    let status = app.recovery_status().unwrap();
    assert!(status.next_restart_at_unix_secs.is_some());
    assert!(app.last_error().unwrap().contains("failed to start"));

    // Not due yet: neither the next tick nor anything else may spawn.
    tick(&state, Instant::now()).await;
    assert_eq!(restart_count(&app), 1);
    assert!(app.get_instances().is_empty());

    // Each due tick makes exactly one attempt, then waits again.
    for attempts in 2..=4 {
        tick_after_backoff(&state).await;
        assert_eq!(restart_count(&app), attempts);
        assert!(app.get_instances().is_empty());
        assert!(
            app.recovery_status()
                .unwrap()
                .next_restart_at_unix_secs
                .is_some()
        );
    }

    app.config.write().command = sh(READY);
    tick_after_backoff(&state).await;
    assert_eq!(live(&app), 1);
    let status = app.recovery_status().unwrap();
    assert_eq!(status.restart_count, 5);
    assert_eq!(status.next_restart_at_unix_secs, None);

    // The event-driven replacement honors the same backoff: this is far from
    // a first loss, so the handler leaves the restart to a later tick.
    let instance = app.get_instances().remove(0);
    handle_health_event(&state, dead(&app, &instance)).await;
    assert!(app.get_instances().is_empty());
    assert_eq!(restart_count(&app), 5);
    tick(&state, Instant::now()).await;
    assert!(app.get_instances().is_empty());
    tick_after_backoff(&state).await;
    assert_eq!(live(&app), 1);
    assert_eq!(restart_count(&app), 6);

    // Healthy at its minimum for the stable window: history is forgotten.
    let start = Instant::now();
    tick(&state, start).await;
    tick(&state, start + Duration::from_secs(59)).await;
    assert!(app.recovery_status().is_some());
    tick(&state, start + Duration::from_secs(60)).await;
    assert!(app.recovery_status().is_none());

    // So the next loss is a first loss again and is replaced promptly.
    let instance = app.get_instances().remove(0);
    handle_health_event(&state, dead(&app, &instance)).await;
    assert_eq!(live(&app), 1);
    assert_eq!(restart_count(&app), 1);
    state.app_manager.stop_app(&app.name()).await.unwrap();
}

#[tokio::test]
async fn reconcile_leaves_apps_alone_when_it_must_not_act() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    let app = running_app(&state, dir.path(), "quiet", 1, sh(READY));
    let name = app.name();
    let now = Instant::now();

    for blocked in [AppState::Deploying, AppState::Stopped, AppState::Error] {
        app.set_state(blocked);
        tick(&state, now).await;
        assert!(app.get_instances().is_empty(), "spawned while {blocked}");
    }
    app.set_state(AppState::Running);

    // Server drain.
    state
        .drain_requested
        .store(true, std::sync::atomic::Ordering::Release);
    tick(&state, now).await;
    assert!(app.get_instances().is_empty(), "spawned during a drain");
    state
        .drain_requested
        .store(false, std::sync::atomic::Ordering::Release);

    // Stop, restore and start hold the lifecycle gate; a drain holds the spawn
    // gate; deploy, scale and secrets hold the per-app deploy lock.
    {
        let _stopping = app.lifecycle_gate.write().await;
        tick(&state, now).await;
    }
    {
        let _draining = state.spawn_gate.write().await;
        tick(&state, now).await;
    }
    {
        let lock = state.get_deploy_lock(&name).await;
        let _deploying = lock.lock().await;
        tick(&state, now).await;
    }
    assert!(app.get_instances().is_empty(), "spawned under a held gate");

    // An instance that is only failing probes is still present, not missing.
    let flapping = app.allocate_instance();
    app.set_instance_state(&flapping, InstanceState::Unhealthy);
    tick(&state, now).await;
    assert_eq!(app.get_instances().len(), 1);
    app.remove_instance(&flapping.id);

    // On-demand apps are started by requests, never by the reconcile.
    let idle = running_app(&state, dir.path(), "on-demand", 0, sh(READY));
    for on_demand in [AppState::Idle, AppState::Running] {
        idle.set_state(on_demand);
        tick(&state, now).await;
        assert!(idle.get_instances().is_empty());
    }

    // With nothing in the way it fills the gap, and only the gap.
    tick(&state, now).await;
    assert_eq!(live(&app), 1);
    tick(&state, now).await;
    assert_eq!(app.get_instances().len(), 1);
    // The start counts as a restart, but no exit was observed, so none is reported.
    assert_eq!(restart_count(&app), 1);
    assert!(app.recovery_status().unwrap().last_exit.is_none());
    state.app_manager.stop_app(&name).await.unwrap();
}

#[tokio::test]
async fn reconcile_never_exceeds_the_instance_limit() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    let app = running_app(&state, dir.path(), "limited", 3, sh(READY));
    app.config.write().max_instances = 2;

    tick(&state, Instant::now()).await;
    assert_eq!(live(&app), 2);
    tick(&state, Instant::now()).await;
    assert_eq!(app.get_instances().len(), 2);
    state.app_manager.stop_app(&app.name()).await.unwrap();
}

#[tokio::test]
async fn status_reports_restarts_the_real_exit_and_the_next_attempt() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    // Becomes ready, then dies with a recognizable exit code once the test
    // releases it. A process that exits in the instant it reports ready can
    // lose the race with its own start and fail that start instead.
    let gate = dir.path().join("exit-gate");
    std::fs::create_dir_all(&gate).unwrap();
    let held = format!(
        "cat <&3 >/dev/null; echo 43210 >&4; while [ ! -e {}/$$ ]; do sleep 0.01; done",
        gate.display()
    );
    let release = |pid: Option<u32>| {
        std::fs::write(gate.join(pid.unwrap().to_string()), b"").unwrap();
    };
    let app = running_app(
        &state,
        dir.path(),
        "status",
        1,
        sh(&format!("{held}; exit 7")),
    );
    let name = app.name();
    let status = |state: Arc<ServerState>, name: String| async move {
        match state.handle_command(Command::Status { app: name }).await {
            Response::Ok { data } => data,
            // CodeQL[rust/cleartext-logging]: this isolated recovery fixture supplies no app secrets; assertions print test responses.
            Response::Error { message } => panic!("{message}"),
        }
    };
    assert!(
        status(state.clone(), name.clone())
            .await
            .get("recovery")
            .is_none(),
        "a quiet app must not report recovery"
    );

    state.app_manager.start_app(&name).await.unwrap();
    release(app.get_instances()[0].pid());
    // Run the real checker so the exit is observed where production observes it.
    // Nothing listens on the reported port, so failed probes must never count
    // here: only the process exit may report an instance dead.
    let (events, mut dead_events) = mpsc::channel(16);
    let checker = HealthChecker::new(
        HealthConfig {
            check_interval: Duration::from_millis(10),
            startup_check_interval: Duration::from_millis(10),
            unhealthy_threshold: u32::MAX,
            dead_threshold: u32::MAX,
            ..Default::default()
        },
        events,
    );
    let monitor = tokio::spawn({
        let app = app.clone();
        async move { checker.monitor_app(app).await }
    });
    let mut next_dead = async || loop {
        let event = tokio::time::timeout(Duration::from_secs(10), dead_events.recv())
            .await
            .expect("the checker never reported the exited instance")
            .unwrap();
        if matches!(event, HealthEvent::Dead { .. }) {
            break event;
        }
    };

    let before = (crate::drain_observer::unix_millis() / 1000) as i64;
    handle_health_event(&state, next_dead().await).await;
    let data = status(state.clone(), name.clone()).await;
    assert_eq!(data["recovery"]["restart_count"], 1);
    assert_eq!(data["recovery"]["last_exit"]["reason"], "crashed");
    assert_eq!(data["recovery"]["last_exit"]["exit_code"], 7);
    assert!(data["recovery"]["last_exit"].get("signal").is_none());
    let at = data["recovery"]["last_exit"]["at_unix_secs"]
        .as_i64()
        .unwrap();
    assert!((before..=before + 30).contains(&at), "{at} vs {before}");
    assert!(data["recovery"].get("next_restart_at_unix_secs").is_none());
    assert_eq!(data["last_error"], "Instance marked dead");

    // The replacement dies the same way. Now Tako waits, and says until when.
    release(app.get_instances()[0].pid());
    handle_health_event(&state, next_dead().await).await;
    monitor.abort();
    let data = status(state.clone(), name.clone()).await;
    assert_eq!(data["recovery"]["restart_count"], 1);
    let next = data["recovery"]["next_restart_at_unix_secs"]
        .as_i64()
        .unwrap();
    assert!((before..=before + 30).contains(&next), "{next} vs {before}");

    // Killed by a signal: reported as a signal, not an exit code.
    app.config.write().command = sh(&format!("{held}; kill -KILL $$"));
    tick_after_backoff(&state).await;
    let instance = app.get_instances().remove(0);
    release(instance.pid());
    tokio::time::timeout(Duration::from_secs(10), async {
        while instance.is_alive().await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    handle_health_event(&state, dead(&app, &instance)).await;
    let data = status(state.clone(), name.clone()).await;
    assert_eq!(data["recovery"]["last_exit"]["reason"], "crashed");
    assert_eq!(data["recovery"]["last_exit"]["signal"], 9);
    assert!(data["recovery"]["last_exit"].get("exit_code").is_none());

    // Never became healthy. The grandchild keeps the readiness pipe open so
    // the exit, not a closed pipe, is what the spawner sees.
    app.config.write().command = sh("sleep 0.3 & exit 3");
    tick_after_backoff(&state).await;
    let data = status(state.clone(), name.clone()).await;
    assert_eq!(data["recovery"]["restart_count"], 3);
    assert_eq!(data["recovery"]["last_exit"]["reason"], "failed_to_start");
    assert_eq!(data["recovery"]["last_exit"]["exit_code"], 3);
    // CodeQL[rust/cleartext-logging]: this isolated recovery fixture supplies no app secrets; assertions print test responses.
    assert!(
        data["last_error"]
            .as_str()
            .unwrap()
            .contains("failed to start"),
        "{data}"
    );

    // Probe-dead with a live process: no exit status to report.
    let hung = healthy_instance(&app);
    handle_health_event(&state, dead(&app, &hung)).await;
    let data = status(state.clone(), name.clone()).await;
    assert_eq!(data["recovery"]["last_exit"]["reason"], "unhealthy");
    assert!(data["recovery"]["last_exit"].get("exit_code").is_none());
    state.app_manager.stop_app(&name).await.unwrap();
}

#[tokio::test]
async fn operator_and_request_driven_starts_are_not_held_by_a_stale_backoff() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    let lost = |app: &App| {
        let instance = app.allocate_instance();
        let exit = instance.exit_report();
        app.remove_instance(&instance.id);
        for _ in 0..6 {
            app.record_instance_exit(exit.clone());
        }
        assert!(app.restart_backoff_remaining(Instant::now()).is_some());
    };

    // Scale: starts at once and clears the history it supersedes.
    let app = running_app(&state, dir.path(), "scaled", 1, sh(READY));
    lost(&app);
    let response = state
        .handle_command(Command::Scale {
            app: app.name(),
            instances: Some(2),
            service: None,
            reset: false,
        })
        .await;
    // CodeQL[rust/cleartext-logging]: this isolated recovery fixture supplies no app secrets; assertions print test responses.
    assert!(response.is_ok(), "{response:?}");
    assert_eq!(live(&app), 2);
    assert!(app.recovery_status().is_none());
    state.app_manager.stop_app(&app.name()).await.unwrap();

    // Cold start: a waiting request gets its attempt immediately.
    let on_demand = running_app(&state, dir.path(), "cold", 0, sh(READY));
    on_demand.set_state(AppState::Idle);
    state.load_balancer.register_app(on_demand.clone());
    lost(&on_demand);
    let mut instance_events = state.app_manager.take_event_receiver().unwrap();
    let bridge = tokio::spawn({
        let state = state.clone();
        async move {
            while let Some(event) = instance_events.recv().await {
                handle_instance_event(&state, event).await;
            }
        }
    });
    let resolution = tokio::time::timeout(
        Duration::from_secs(10),
        crate::proxy::resolve_app_backend(
            &state.load_balancer,
            &state.cold_start,
            &state.drain_requested,
            &state.spawn_gate,
            &on_demand.name(),
        ),
    )
    .await
    .expect("cold start waited on the backoff");
    assert!(matches!(
        resolution,
        crate::proxy::BackendResolution::Ready { .. }
    ));
    bridge.abort();
    state.app_manager.stop_app(&on_demand.name()).await.unwrap();
}

#[tokio::test]
async fn successful_deploy_resets_the_backoff() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    let name = "deployed/production";
    let release = |version: &str| {
        let path = dir
            .path()
            .join("apps")
            .join(name)
            .join("releases")
            .join(version);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("app.json"),
            serde_json::to_vec(&serde_json::json!({
                "protocol_version": tako_core::PROTOCOL_VERSION,
                "runtime": "node",
                "main": "index.js",
                "idle_timeout": 300,
                "app_dir": "",
                "start": sh(READY),
            }))
            .unwrap(),
        )
        .unwrap();
        path
    };
    let deploy = |version: &'static str, path: std::path::PathBuf| Command::Deploy {
        app: name.into(),
        version: version.into(),
        path: path.to_string_lossy().into_owned(),
        routes: vec!["deployed.example.com".into()],
        source_ip: tako_core::SourceIpMode::Auto,
        secrets: Some(HashMap::new()),
        runtime_credentials: None,
        storages: Some(HashMap::new()),
        ssl: tako_core::SslBinding::default(),
        backup: None,
        force: false,
    };

    let response = state.handle_command(deploy("v1", release("v1"))).await;
    // CodeQL[rust/cleartext-logging]: this isolated recovery fixture supplies no app secrets; assertions print test responses.
    assert!(response.is_ok(), "{response:?}");
    let app = state.app_manager.get_app(name).unwrap();
    let exit = app.get_instances()[0].exit_report();
    for _ in 0..6 {
        app.record_instance_exit(exit.clone());
    }
    assert!(app.restart_backoff_remaining(Instant::now()).is_some());

    // A failed deploy keeps the history of the release that keeps running.
    let broken = release("v2");
    std::fs::write(
        broken.join("app.json"),
        r#"{"protocol_version":0,"runtime":"node","main":"index.js","idle_timeout":300,"start":["/bin/sh","-c","exit 1"]}"#,
    )
    .unwrap();
    let response = state.handle_command(deploy("v2", broken)).await;
    assert!(!response.is_ok());
    assert!(app.recovery_status().is_some());

    // Deploy onto a stopped app so the test skips the rollout stability window.
    state.app_manager.stop_app(name).await.unwrap();
    let response = state.handle_command(deploy("v3", release("v3"))).await;
    // CodeQL[rust/cleartext-logging]: this isolated recovery fixture supplies no app secrets; assertions print test responses.
    assert!(response.is_ok(), "{response:?}");
    assert!(app.recovery_status().is_none());
    assert_eq!(app.restart_backoff_remaining(Instant::now()), None);
    state.app_manager.stop_app(name).await.unwrap();
}

#[tokio::test]
async fn an_app_in_error_with_live_instances_keeps_recovering_after_repeated_crashes() {
    // A server restart started one of two instances; the second failed, so
    // the app is in `Error` while the first keeps serving.
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    let app = running_app(&state, dir.path(), "error-live", 1, sh(READY));
    app.set_state(AppState::Error);
    let instance = healthy_instance(&app);

    // First crash: the health path replaces it at once.
    handle_health_event(&state, dead(&app, &instance)).await;
    assert_eq!(live(&app), 1);

    // The replacement crashes too: the replacement is now backed off and
    // left to the reconcile, which must not ignore the app for its state.
    let replacement = app.get_instances().remove(0);
    handle_health_event(&state, dead(&app, &replacement)).await;
    assert!(app.get_instances().is_empty());
    assert!(
        app.recovery_status()
            .unwrap()
            .next_restart_at_unix_secs
            .is_some()
    );
    tick_after_backoff(&state).await;
    assert_eq!(live(&app), 1, "an app in Error was left at zero instances");

    // A start that fails in `Error` is reported and backs off like any other.
    app.config.write().command = vec!["./missing-app".into()];
    let instance = app.get_instances().remove(0);
    handle_health_event(&state, dead(&app, &instance)).await;
    tick_after_backoff(&state).await;
    let status = app.recovery_status().unwrap();
    assert_eq!(
        status.last_exit.unwrap().reason,
        tako_core::InstanceExitReason::FailedToStart
    );
    assert!(status.next_restart_at_unix_secs.is_some());
    state.app_manager.stop_app(&app.name()).await.unwrap();
}

#[tokio::test]
async fn an_app_in_error_keeps_its_minimum_after_its_restart_history_settles() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    let app = running_app(&state, dir.path(), "error-settled", 1, sh(READY));
    app.set_state(AppState::Error);
    let instance = healthy_instance(&app);
    // Earlier losses were repaired: the app is back at its minimum, healthy,
    // and a stable minute later its history is forgotten.
    let lost = app.allocate_instance();
    app.record_instance_exit(lost.exit_report());
    app.remove_instance(&lost.id);
    let start = Instant::now();
    tick(&state, start).await;
    tick(&state, start + Duration::from_secs(60)).await;
    assert!(app.recovery_status().is_none());

    // The instance dies while the health path is skipped under a contended
    // gate, so nothing records the loss. The reconcile still sees it.
    {
        let _recovering = app.recovery_gate.try_lock().unwrap();
        handle_health_event(&state, dead(&app, &instance)).await;
    }
    tick(&state, start + Duration::from_secs(61)).await;
    assert!(app.get_instance(&instance.id).is_none());
    assert_eq!(live(&app), 1, "an app in Error was left below its minimum");
    assert_eq!(app.state(), AppState::Error);
    state.app_manager.stop_app(&app.name()).await.unwrap();
}

#[tokio::test]
async fn a_backup_holding_the_deploy_lock_does_not_hold_back_native_recovery() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    let app = running_app(&state, dir.path(), "backed-up", 1, sh(READY));
    let name = app.name();
    let instance = healthy_instance(&app);

    // A backup takes the deploy lock for a snapshot that can take minutes.
    let backup = state.lock_deploy_for_backup(&name).await;
    // Meanwhile the app crashes twice: once replaced at once, then backed off.
    handle_health_event(&state, dead(&app, &instance)).await;
    assert_eq!(live(&app), 1);
    let replacement = app.get_instances().remove(0);
    handle_health_event(&state, dead(&app, &replacement)).await;
    assert!(app.get_instances().is_empty());

    tick_after_backoff(&state).await;
    assert_eq!(
        live(&app),
        1,
        "recovery waited for the backup to release the deploy lock"
    );
    let started = app.get_instances().remove(0);

    // Any other holder of the deploy lock still keeps recovery out.
    drop(backup);
    handle_health_event(&state, dead(&app, &started)).await;
    {
        let lock = state.get_deploy_lock(&name).await;
        let _deploying = lock.lock().await;
        tick_after_backoff(&state).await;
        assert!(app.get_instances().is_empty(), "spawned under a deploy");
    }

    // A slot claimed during a backup is part of the instance set the next
    // operation sees: a scale-down that follows the backup retires it.
    let backup = state.lock_deploy_for_backup(&name).await;
    tick_after_backoff(&state).await;
    assert_eq!(live(&app), 1);
    drop(backup);
    let response = state
        .handle_command(Command::Scale {
            app: name.clone(),
            instances: Some(0),
            service: None,
            reset: false,
        })
        .await;
    // CodeQL[rust/cleartext-logging]: this isolated recovery fixture supplies no app secrets; assertions print test responses.
    assert!(response.is_ok(), "{response:?}");
    assert_eq!(
        app.get_instances().len(),
        0,
        "the scale-down missed an instance"
    );
    state.app_manager.stop_app(&name).await.unwrap();
}

#[tokio::test]
async fn operators_get_through_automatic_recovery_without_waiting_for_it() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    let app = running_app(&state, dir.path(), "crash-loop", 1, sh(READY));
    let name = app.name();
    let lock = state.get_deploy_lock(&name).await;
    let (app_ref, lock_ref) = (&*app, &*lock);
    let recovery = move |now| {
        try_lock_deploy_for_background(app_ref, lock_ref, BackgroundWork::AutomaticRecovery, now)
    };
    let priority = || app.deploy_lock_access.lock().operator_priority_until;
    let update_secrets =
        || state.update_secrets(&name, HashMap::from([("FIX".into(), "1".into())]));
    let release_dir = dir.path().join("apps/crash-loop/production/releases/v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    std::fs::write(
        release_dir.join("app.json"),
        serde_json::to_vec(&serde_json::json!({
            "protocol_version": tako_core::PROTOCOL_VERSION,
            "runtime": "node",
            "main": "index.js",
            "idle_timeout": 300,
            "app_dir": "",
        }))
        .unwrap(),
    )
    .unwrap();
    let release_path = release_dir.to_string_lossy().into_owned();
    let run_release = || {
        state.run_release(
            &name,
            "v1",
            &release_path,
            "true",
            HashMap::new(),
            HashMap::new(),
            false,
        )
    };
    let error = |response: Response| match response {
        Response::Error { message } => message,
        // CodeQL[rust/cleartext-logging]: this recovery fixture supplies only a fabricated secret; update_secrets responses never echo secret values.
        Response::Ok { data } => panic!("expected an error, got {data}"),
    };
    let recovering = format!(
        "Automatic recovery of {name} is in progress; retry in a moment. \
         Recovery will pause to let the next attempt through."
    );

    // Another operator operation holds the lock: fail at once, as before,
    // and do not hold background work back.
    {
        let _deploying = lock.lock().await;
        let started = std::time::Instant::now();
        assert_eq!(
            error(update_secrets().await),
            format!("Deploy already in progress for app '{name}'. Please wait and try again.")
        );
        assert!(started.elapsed() < OPERATOR_DEPLOY_LOCK_WAIT);
        assert_eq!(priority(), None);
    }

    // Recovery holds the lock briefly: the operator waits it out.
    let held = recovery(Instant::now()).unwrap();
    let (response, ()) = tokio::join!(update_secrets(), async {
        tokio::time::sleep(OPERATOR_DEPLOY_LOCK_WAIT / 4).await;
        drop(held);
    });
    // CodeQL[rust/cleartext-logging]: this recovery fixture supplies only a fabricated secret; update_secrets responses never echo secret values.
    assert!(response.is_ok(), "{response:?}");
    assert_eq!(app.config.read().secrets["FIX"], "1");
    assert_eq!(priority(), None);

    // Recovery holds it longer (think: a Compose reconcile waiting for a
    // hanging container). Both operations fail after the short wait, saying
    // why, and recovery yields the lock to the retry from then on.
    let held = recovery(Instant::now()).unwrap();
    for response in [update_secrets().await, run_release().await] {
        assert_eq!(error(response), recovering);
    }
    assert!(priority().is_some());
    drop(held);
    tick(&state, Instant::now()).await;
    assert!(
        app.get_instances().is_empty(),
        "recovery took the lock back"
    );
    assert!(recovery(Instant::now()).is_none());
    let response = run_release().await;
    // CodeQL[rust/cleartext-logging]: this recovery fixture supplies only a fabricated secret; update_secrets responses never echo secret values.
    assert!(response.is_ok(), "{response:?}");
    assert_eq!(priority(), None, "the retry did not clear the window");

    // An operator that gives up inside its wait leaves nothing held, and
    // the window it opened expires on its own.
    let held = recovery(Instant::now()).unwrap();
    assert!(
        tokio::time::timeout(OPERATOR_DEPLOY_LOCK_WAIT / 4, update_secrets())
            .await
            .is_err()
    );
    drop(held);
    assert!(app.deploy_lock_access.lock().background.is_none());
    let opened = Instant::now();
    assert!(recovery(opened + OPERATOR_PRIORITY_WINDOW / 2).is_none());
    drop(lock.try_lock().expect("the deploy lock leaked"));
    let resumed = opened + OPERATOR_PRIORITY_WINDOW + Duration::from_secs(1);
    tick(&state, resumed).await;
    assert_eq!(live(&app), 1, "recovery never resumed");
    state.app_manager.stop_app(&name).await.unwrap();
}

#[tokio::test]
async fn operators_get_through_a_scheduled_backup_and_recovery_claims_slots_during_it() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    let app = running_app(&state, dir.path(), "scheduled", 1, sh(READY));
    let name = app.name();
    let scheduled = |now| state.try_lock_deploy_for_scheduled_backup(&name, now);
    let priority = || app.deploy_lock_access.lock().operator_priority_until;
    let update_secrets =
        || state.update_secrets(&name, HashMap::from([("FIX".into(), "1".into())]));
    let backing_up = format!(
        "A scheduled backup of {name} is in progress; retry in a moment. \
         Scheduled backups will pause to let the next attempt through."
    );

    // A snapshot that finishes within the short wait: the operator gets in.
    let held = scheduled(Instant::now()).await.unwrap();
    let (response, ()) = tokio::join!(update_secrets(), async {
        tokio::time::sleep(OPERATOR_DEPLOY_LOCK_WAIT / 4).await;
        drop(held);
    });
    // CodeQL[rust/cleartext-logging]: this recovery fixture supplies only a fabricated secret; update_secrets responses never echo secret values.
    assert!(response.is_ok(), "{response:?}");
    assert_eq!(priority(), None);

    // A longer capture: the operator is told why, and scheduled backups
    // leave the lock to the retry until the window ends.
    let held = scheduled(Instant::now()).await.unwrap();
    match update_secrets().await {
        // CodeQL[rust/cleartext-logging]: this recovery fixture supplies only a fabricated secret; update_secrets responses never echo secret values.
        Response::Error { message } => assert_eq!(message, backing_up),
        Response::Ok { .. } => panic!("the operator took a held deploy lock"),
    }
    assert!(priority().is_some());
    drop(held);
    assert!(app.deploy_lock_access.lock().background.is_none());
    assert!(scheduled(Instant::now()).await.is_none());
    let expired = Instant::now() + OPERATOR_PRIORITY_WINDOW + Duration::from_secs(1);
    drop(
        scheduled(expired)
            .await
            .expect("scheduled backups never resumed"),
    );
    let response = update_secrets().await;
    // CodeQL[rust/cleartext-logging]: this recovery fixture supplies only a fabricated secret; update_secrets responses never echo secret values.
    assert!(response.is_ok(), "{response:?}");
    assert_eq!(priority(), None, "the retry did not clear the window");

    // Native recovery claims slots while a scheduled backup holds the lock,
    // and the window closes with the lock.
    let held = scheduled(Instant::now()).await.unwrap();
    tick(&state, Instant::now()).await;
    assert_eq!(
        live(&app),
        1,
        "recovery waited for the scheduled backup to release the deploy lock"
    );
    drop(held);
    assert!(!*app.recovery_claims_open.lock());
    assert!(app.deploy_lock_access.lock().background.is_none());
    let lock = state.get_deploy_lock(&name).await;
    drop(lock.try_lock().expect("the deploy lock leaked"));
    state.app_manager.stop_app(&name).await.unwrap();
}

#[tokio::test]
async fn an_idle_scale_to_zero_app_stops_reporting_old_restarts() {
    let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
    let state = test_state(dir.path());
    for idle_state in [AppState::Idle, AppState::Running] {
        let app = running_app(
            &state,
            dir.path(),
            &format!("on-demand-{idle_state}"),
            0,
            sh(READY),
        );
        let instance = healthy_instance(&app);
        // It crashed while serving a request; nothing replaces it (min 0).
        handle_health_event(&state, dead(&app, &instance)).await;
        assert!(app.get_instances().is_empty());
        assert!(app.recovery_status().is_some());
        app.set_state(idle_state);

        let start = Instant::now();
        tick(&state, start).await;
        tick(&state, start + Duration::from_secs(59)).await;
        assert!(app.recovery_status().is_some(), "{idle_state}");
        tick(&state, start + Duration::from_secs(60)).await;
        assert!(
            app.recovery_status().is_none(),
            "{idle_state}: an idle app kept its old restart report"
        );
        assert!(app.get_instances().is_empty(), "{idle_state}");
    }
}

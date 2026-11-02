use crate::ServerState;
use crate::instances::{App, HealthEvent, InstanceEvent};
use crate::scaling::IdleEvent;
use crate::socket::{AppState, InstanceState};

pub(crate) async fn handle_instance_event(state: &ServerState, event: InstanceEvent) {
    match event {
        InstanceEvent::Started { app, instance_id } => {
            tracing::debug!(app = %app, instance = %instance_id, "Instance started");
        }
        InstanceEvent::Ready { app, instance_id } => {
            tracing::info!(app = %app, instance = %instance_id, "Instance ready");
            state.cold_start.mark_ready(&app);
            crate::metrics::set_instance_health(&app, &instance_id, true);

            if let Some(app_ref) = state.app_manager.get_app(&app) {
                app_ref.clear_last_error();
                update_instance_count_metric(&app, &app_ref);
            }
        }
    }
}

pub(crate) async fn handle_health_event(state: &ServerState, event: HealthEvent) {
    match event {
        HealthEvent::Healthy { app, instance_id } => {
            tracing::info!(app = %app, instance = %instance_id, "Instance is healthy");
            crate::metrics::set_instance_health(&app, &instance_id, true);
            state.cold_start.mark_ready(&app);

            if let Some(app_ref) = state.app_manager.get_app(&app) {
                app_ref.clear_last_error();
                update_instance_count_metric(&app, &app_ref);
            }
        }
        HealthEvent::Unhealthy { app, instance_id } => {
            tracing::warn!(app = %app, instance = %instance_id, "Instance became unhealthy");
            crate::metrics::set_instance_health(&app, &instance_id, false);
        }
        HealthEvent::Dead { app, instance_id } => {
            tracing::error!(app = %app, instance = %instance_id, "Instance is dead (no heartbeat)");
            crate::metrics::set_instance_health(&app, &instance_id, false);
            crate::metrics::remove_instance_metrics(&app, &instance_id);
            state.cold_start.mark_failed(&app, "instance_dead");
            if let Some(app_ref) = state.app_manager.get_app(&app) {
                app_ref.set_last_error("Instance marked dead");
                update_instance_count_metric(&app, &app_ref);
            }
            replace_instance_if_needed(state, &app, &instance_id, "dead").await;
        }
        HealthEvent::Recovered { app, instance_id } => {
            tracing::info!(app = %app, instance = %instance_id, "Instance recovered from unhealthy");
            crate::metrics::set_instance_health(&app, &instance_id, true);
        }
    }
}

pub(crate) fn update_instance_count_metric(app_name: &str, app: &App) {
    let count = app
        .get_instances()
        .iter()
        .filter(|i| {
            matches!(
                i.state(),
                InstanceState::Starting | InstanceState::Ready | InstanceState::Healthy
            )
        })
        .count();
    crate::metrics::set_instances_running(app_name, count as i64);
}

pub(crate) async fn handle_idle_event(state: &ServerState, event: IdleEvent) {
    let name = match &event {
        IdleEvent::InstanceIdle { app, .. } | IdleEvent::AppIdle { app } => app,
    };
    // A deployment can temporarily attach two logical instances to one Compose
    // ingress. Its predecessor still owns the container until commit. Skip
    // eviction while any lifecycle mutation holds the lock; the monitor retries.
    let lock = state.get_deploy_lock(name).await;
    let Ok(_guard) = lock.try_lock() else {
        return;
    };
    match event {
        IdleEvent::InstanceIdle { app, instance_id } => {
            if let Some(app_ref) = state.app_manager.get_app(&app)
                && let Some(instance) = app_ref.get_instance(&instance_id)
            {
                let (min_instances, idle_timeout) = {
                    let config = app_ref.config.read();
                    (config.min_instances, config.idle_timeout)
                };
                let healthy = app_ref
                    .get_instances()
                    .iter()
                    .filter(|instance| instance.state() == InstanceState::Healthy)
                    .count();
                if healthy <= min_instances as usize
                    || instance.state() != InstanceState::Healthy
                    || instance.in_flight() != 0
                    || instance.idle_time() <= idle_timeout
                {
                    return;
                }
                if let Err(e) = instance.kill().await {
                    tracing::warn!(app = %app, instance = %instance_id, "Failed to kill idle instance: {}", e);
                }
                app_ref.remove_instance(&instance_id);
                crate::metrics::remove_instance_metrics(&app, &instance_id);

                let running_count = app_ref
                    .get_instances()
                    .iter()
                    .filter(|i| {
                        matches!(
                            i.state(),
                            InstanceState::Starting | InstanceState::Ready | InstanceState::Healthy
                        )
                    })
                    .count();
                crate::metrics::set_instances_running(&app, running_count as i64);
                let min_instances = app_ref.config.read().min_instances;

                if running_count == 0 && min_instances == 0 {
                    app_ref.set_state(AppState::Idle);
                    state.cold_start.reset(&app);
                }
            }
        }
        IdleEvent::AppIdle { app } => {
            if let Some(app_ref) = state.app_manager.get_app(&app) {
                if app_ref.config.read().min_instances != 0
                    || app_ref.get_instances().iter().any(|instance| {
                        matches!(
                            instance.state(),
                            InstanceState::Starting | InstanceState::Ready | InstanceState::Healthy
                        )
                    })
                {
                    return;
                }
                app_ref.set_state(AppState::Idle);
            }
            state.cold_start.reset(&app);
        }
    }
}

async fn replace_instance_if_needed(
    state: &ServerState,
    app_name: &str,
    instance_id: &str,
    reason: &str,
) {
    let app = match state.app_manager.get_app(app_name) {
        Some(app) => app,
        None => {
            tracing::warn!(app = %app_name, "Cannot replace instance: app not found");
            return;
        }
    };

    // A rollout owns every Compose container mutation for this app. Health
    // events may observe intentional stop-first transitions; never race them
    // with an independent reconciliation or orphan cleanup.
    let lock = state.get_deploy_lock(app_name).await;
    let _compose_guard = if app.config.read().compose.is_some() {
        match lock.try_lock() {
            Ok(guard) => Some(guard),
            Err(_) => return,
        }
    } else {
        None
    };

    let instance = match app.get_instance(instance_id) {
        Some(inst) => inst,
        None => {
            tracing::debug!(app = %app_name, instance = %instance_id, "Instance already removed");
            return;
        }
    };

    let failed_build = instance.build_version().to_string();
    let current_version = app.version();
    let current_count = app
        .get_instances()
        .into_iter()
        .filter(|i| i.build_version() == failed_build.as_str())
        .count() as u32;
    let min_instances = app.config.read().min_instances;
    let min_for_build = if failed_build == current_version {
        min_instances
    } else {
        0
    };

    if current_count > min_for_build {
        tracing::info!(
            app = %app_name,
            instance = %instance_id,
            reason = reason,
            build = %failed_build,
            current = current_count,
            min = min_for_build,
            "Not replacing {} instance: have more than minimum instances",
            reason
        );
        if let Err(e) = instance.kill().await {
            tracing::error!(app = %app_name, instance = %instance_id, "Failed to kill instance: {}", e);
        }
        app.remove_instance(instance_id);
        return;
    }

    tracing::info!(
        app = %app_name,
        instance = %instance_id,
        reason = reason,
        "Replacing {} instance with a new one",
        reason
    );

    if let Err(e) = instance.kill().await {
        tracing::error!(app = %app_name, instance = %instance_id, "Failed to kill old instance: {}", e);
    }
    app.remove_instance(instance_id);

    let spawner = state.app_manager.spawner();
    if app.config.read().compose.is_some() {
        if let Err(error) = spawner.reconcile_compose(&app).await {
            app.set_last_error(format!("Compose recovery failed: {error}"));
            tracing::error!(app = %app_name, %error, "Could not reconcile Compose replicas");
        }
        update_instance_count_metric(app_name, &app);
        return;
    }
    let new_instance = app.allocate_instance();

    match spawner.spawn(&app, new_instance.clone()).await {
        Ok(()) => {
            tracing::info!(
                app = %app_name,
                old_instance = %instance_id,
                new_instance = %new_instance.id,
                "Successfully spawned replacement instance"
            );
        }
        Err(e) => {
            tracing::error!(
                app = %app_name,
                instance = %new_instance.id,
                "Failed to spawn replacement instance: {}",
                e
            );
            app.remove_instance(&new_instance.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, sync::Arc, time::Duration};

    #[tokio::test]
    async fn idle_events_cannot_mutate_instances_during_deployment_or_below_minimum() {
        let dir = tempfile::tempdir().unwrap();
        let state = ServerState::new(
            dir.path().into(),
            Arc::new(crate::tls::CertManager::new(
                crate::tls::CertManagerConfig {
                    cert_dir: dir.path().join("certs"),
                    ..Default::default()
                },
            )),
            None,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
        )
        .unwrap();
        let app = state.app_manager.register_app(crate::instances::AppConfig {
            name: "idle-rollout".into(),
            environment: "production".into(),
            min_instances: 1,
            idle_timeout: Duration::ZERO,
            ..Default::default()
        });
        app.set_state(AppState::Deploying);
        let old = app.allocate_instance();
        let new = app.allocate_instance();
        old.set_state(InstanceState::Healthy);
        new.set_state(InstanceState::Healthy);
        let name = "idle-rollout/production";
        let event = || IdleEvent::InstanceIdle {
            app: name.into(),
            instance_id: old.id.clone(),
        };
        let lock = state.get_deploy_lock(name).await;
        let guard = lock.lock().await;
        handle_idle_event(&state, event()).await;
        handle_idle_event(&state, IdleEvent::AppIdle { app: name.into() }).await;
        assert_eq!(old.state(), InstanceState::Healthy);
        assert_eq!(app.state(), AppState::Deploying);
        assert_eq!(app.get_instances().len(), 2);
        drop(guard);
        // Outside a rollout, an actually surplus idle instance is evicted.
        tokio::time::sleep(Duration::from_millis(2)).await;
        handle_idle_event(&state, event()).await;
        assert!(app.get_instance(&old.id).is_none());
        // A queued event from an earlier monitor pass cannot remove the last.
        handle_idle_event(
            &state,
            IdleEvent::InstanceIdle {
                app: name.into(),
                instance_id: new.id.clone(),
            },
        )
        .await;
        assert_eq!(new.state(), InstanceState::Healthy);
    }
}

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
    match event {
        IdleEvent::InstanceIdle { app, instance_id } => {
            if let Some(app_ref) = state.app_manager.get_app(&app)
                && let Some(instance) = app_ref.get_instance(&instance_id)
            {
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

    let new_instance = app.allocate_instance();
    let spawner = state.app_manager.spawner();

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

use crate::ServerState;
use crate::instances::{App, HealthEvent, Instance, InstanceEvent, effective_instance_limit};
use crate::scaling::IdleEvent;
use crate::socket::{AppState, InstanceState};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the level-triggered instance reconcile looks at every app.
pub(crate) const INSTANCE_RECONCILE_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) async fn handle_instance_event(state: &ServerState, event: InstanceEvent) {
    if state.is_draining() {
        return;
    }
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
    if state.is_draining() {
        return;
    }
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
            crate::metrics::remove_instance_metrics(&app, &instance_id);
            let Some(app_ref) = state.app_manager.get_app(&app) else {
                return;
            };
            // The updater owns candidates until cutover. It removes a failed
            // candidate from the registry before clearing routing suppression.
            if app_ref.is_instance_routing_suppressed(&instance_id)
                || app_ref.get_instance(&instance_id).is_none()
            {
                return;
            }
            tracing::error!(app = %app, instance = %instance_id, "Instance is dead (no heartbeat)");
            state.cold_start.mark_failed(&app, "instance_dead");
            app_ref.set_last_error("Instance marked dead");
            update_instance_count_metric(&app, &app_ref);
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
    let Ok(_management) = state.management_lock.try_lock() else {
        return;
    };
    if state.is_draining() {
        return;
    }
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
    let Some(app_ref) = state.app_manager.get_app(name) else {
        return;
    };
    let Ok(lifecycle) = app_ref.lifecycle_gate.try_write() else {
        return;
    };
    if app_ref.state() == AppState::Stopped {
        return;
    }
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
                app_ref.set_instance_state(&instance, InstanceState::Draining);
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

                // Routing and cold starts may proceed once the idle instance is
                // excluded and scale-to-zero state is published. Keep shared
                // admission until shutdown finishes so stop/restore still wait
                // for this instance, without blocking healthy peers for the
                // entire process termination grace period.
                let _lifecycle = tokio::sync::RwLockWriteGuard::downgrade(lifecycle);
                if let Err(e) = instance.kill().await {
                    tracing::warn!(app = %app, instance = %instance_id, "Failed to kill idle instance: {}", e);
                }
                app_ref.remove_instance(&instance_id);
                crate::metrics::remove_instance_metrics(&app, &instance_id);
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
    // A drain holds the spawn gate for writing while it stops instances, so a
    // replacement spawned under the read side cannot outlive it. The health
    // loop is single-threaded: never block it on a deployment's management lock
    // or on a drain that is already stopping this instance.
    let Ok(_spawning) = state.spawn_gate.try_read() else {
        return;
    };
    if state.is_draining() {
        return;
    }
    let app = match state.app_manager.get_app(app_name) {
        Some(app) => app,
        None => {
            tracing::warn!(app = %app_name, "Cannot replace instance: app not found");
            return;
        }
    };
    let Ok(_lifecycle) = app.lifecycle_gate.try_read() else {
        return;
    };
    if app.state() == AppState::Stopped {
        return;
    }

    // A rollout owns every Compose container mutation for this app. Health
    // events may observe intentional stop-first transitions; never race them
    // with an independent reconciliation or orphan cleanup.
    let lock = state.get_deploy_lock(app_name).await;
    let _compose_guard = if app.config.read().compose.is_some() {
        match try_lock_deploy_for_background(
            &app,
            &lock,
            BackgroundWork::AutomaticRecovery,
            Instant::now(),
        ) {
            Some(guard) => Some(guard),
            None => return,
        }
    } else {
        None
    };

    // The periodic reconcile may already be repairing this app. It sees this
    // instance too, so leave it there rather than start a second replacement.
    let Ok(_recovering) = app.recovery_gate.try_lock() else {
        return;
    };

    let instance = match app.get_instance(instance_id) {
        Some(inst) => inst,
        None => {
            tracing::debug!(app = %app_name, instance = %instance_id, "Instance already removed");
            return;
        }
    };
    app.record_instance_exit(instance.exit_report());

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

    // A first loss is replaced at once. While instances keep failing, the
    // periodic reconcile starts the replacement when the backoff expires.
    if let Some(wait) = app.restart_backoff_remaining(Instant::now()) {
        tracing::warn!(
            app = %app_name,
            instance = %instance_id,
            wait_ms = wait.as_millis() as u64,
            "Instances keep failing; delaying the replacement"
        );
        return;
    }

    if app.config.read().compose.is_some() {
        recover_compose(state, app_name, &app).await;
        return;
    }
    let new_instance = app.allocate_instance();
    start_replacement(state, app_name, &app, new_instance).await;
}

async fn recover_compose(state: &ServerState, app_name: &str, app: &App) {
    app.record_restart();
    if let Err(error) = state.app_manager.spawner().reconcile_compose(app).await {
        app.set_last_error(format!("Compose recovery failed: {error}"));
        app.record_instance_exit(tako_core::InstanceExit {
            reason: tako_core::InstanceExitReason::FailedToStart,
            exit_code: None,
            signal: None,
            at_unix_secs: (crate::drain_observer::unix_millis() / 1000) as i64,
        });
        tracing::error!(app = %app_name, %error, "Could not reconcile Compose replicas");
    }
    update_instance_count_metric(app_name, app);
}

/// Start an already allocated replacement. Returns whether it became healthy.
/// Caller holds the spawn gate, the app's lifecycle gate and its recovery gate.
async fn start_replacement(
    state: &ServerState,
    app_name: &str,
    app: &App,
    instance: Arc<Instance>,
) -> bool {
    app.record_restart();
    match state
        .app_manager
        .spawner()
        .spawn(app, instance.clone())
        .await
    {
        Ok(()) => {
            tracing::info!(
                app = %app_name,
                instance = %instance.id,
                "Successfully spawned replacement instance"
            );
            true
        }
        Err(e) => {
            tracing::error!(
                app = %app_name,
                instance = %instance.id,
                "Failed to spawn replacement instance: {}",
                e
            );
            // A deploy, scale-down or stop that arrived meanwhile owns this
            // instance and stopped it on purpose; that is not a failed start.
            if app.remove_instance(&instance.id).is_some()
                && matches!(app.state(), AppState::Running | AppState::Error)
                && app.version() == instance.build_version()
            {
                app.record_instance_exit(instance.exit_report());
                app.set_last_error(format!("Replacement instance failed to start: {e}"));
            }
            false
        }
    }
}

/// What the reconcile needs to know about one app's instances.
struct Survey {
    /// Current-version instances that are up, coming up, or only failing
    /// probes. The health checker turns the last kind into healthy or dead
    /// within a few probes, so they are not missing yet.
    present: u32,
    all_healthy: bool,
    /// Instances of any version that may still own a process.
    running: u32,
    /// Dead instances that nobody removed.
    dead: Vec<Arc<Instance>>,
    /// Instances the app should have, within the server's limit.
    target: u32,
    limit: u32,
}

fn survey(app: &App) -> Survey {
    let (version, min_instances, limit) = {
        let config = app.config.read();
        let limit = if config.compose.is_some() {
            // Replica counts of a Compose stack are validated by its rollout.
            u32::MAX
        } else {
            effective_instance_limit(config.max_instances)
        };
        (config.version.clone(), config.min_instances, limit)
    };
    let mut survey = Survey {
        present: 0,
        all_healthy: true,
        running: 0,
        dead: Vec::new(),
        target: min_instances.min(limit),
        limit,
    };
    for instance in app.get_instances() {
        // A rollout owns its candidates, alive or dead, until cutover.
        if app.is_instance_routing_suppressed(&instance.id) {
            survey.running += 1;
            continue;
        }
        let state = instance.state();
        match state {
            InstanceState::Stopped => {
                survey.dead.push(instance);
                continue;
            }
            InstanceState::Draining => {}
            InstanceState::Starting
            | InstanceState::Ready
            | InstanceState::Healthy
            | InstanceState::Unhealthy => {
                if instance.build_version() == version {
                    survey.present += 1;
                    survey.all_healthy &= state == InstanceState::Healthy;
                }
            }
        }
        survey.running += 1;
    }
    survey
}

/// Whether the reconcile may restore this app's instances. A running app,
/// and an app in `Error` that still has instances or is being repaired: a
/// failed restore or rollback can leave live instances, the health path
/// replaces them in any state but `Stopped`, and once it backs off it leaves
/// the next replacement to the reconcile. An app in `Error` with neither was
/// left stopped on purpose.
fn reconciles(app: &App) -> bool {
    match app.state() {
        AppState::Running => true,
        AppState::Error => app.has_recovery_history() || !app.get_instances().is_empty(),
        AppState::Idle | AppState::Deploying | AppState::Stopped => false,
    }
}

/// Cheap, lock-free look at one app: does it need the reconcile at all?
/// Also feeds the stable-window observation that forgets an old backoff.
fn needs_recovery(app: &App, now: Instant) -> bool {
    let reconciles = reconciles(app);
    // An idle on-demand app is never reconciled, but it is still observed so
    // an old restart report settles instead of lingering until a deploy.
    if !reconciles && app.state() != AppState::Idle {
        return false;
    }
    let survey = survey(app);
    let settled = survey.dead.is_empty() && survey.present >= survey.target;
    // Healthy: at its minimum with every current instance healthy. With a
    // minimum of zero and no instances left there is nothing to recover.
    app.observe_recovery_health(settled && survey.all_healthy, now);
    reconciles && !settled
}

/// Level-triggered instance recovery. Health events are edge-triggered: a
/// replacement that was skipped under a contended gate, or that failed to
/// start, leaves nothing behind for the health loop to notice. This looks at
/// the level instead: every running app that is below its minimum instance
/// count, or still holds a dead instance, gets one recovery pass.
///
/// Each app recovers in its own task, so one slow start cannot delay another
/// app. The returned handles are only awaited by tests.
pub(crate) fn reconcile_instances(
    state: &Arc<ServerState>,
    now: Instant,
) -> Vec<tokio::task::JoinHandle<()>> {
    if state.is_draining() {
        return Vec::new();
    }
    state
        .app_manager
        .list_apps()
        .iter()
        .filter_map(|name| state.app_manager.get_app(name))
        .filter(|app| needs_recovery(app, now))
        .map(|app| {
            let state = state.clone();
            tokio::spawn(async move { recover_app(&state, &app, now).await })
        })
        .collect()
}

/// How long an operator operation waits for background work to release the
/// deploy lock: below the CLI's 5 s timeout for quick commands.
const OPERATOR_DEPLOY_LOCK_WAIT: Duration =
    Duration::from_millis(if cfg!(test) { 200 } else { 2000 });
/// How long background work then leaves the lock to the operator's retry.
const OPERATOR_PRIORITY_WINDOW: Duration = Duration::from_secs(60);

/// Work that takes an app's deploy lock on its own, not for an operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackgroundWork {
    AutomaticRecovery,
    #[allow(dead_code)] // For scheduled backups; operator backups are operator work.
    ScheduledBackup,
}

impl BackgroundWork {
    fn busy_message(self, app_name: &str) -> String {
        let (work, pauses) = match self {
            Self::AutomaticRecovery => ("Automatic recovery", "Recovery"),
            Self::ScheduledBackup => ("A scheduled backup", "Scheduled backups"),
        };
        format!(
            "{work} of {app_name} is in progress; retry in a moment. \
             {pauses} will pause to let the next attempt through."
        )
    }
}

/// Who holds an app's deploy lock for background work, and until when
/// background work leaves it to an operator operation it turned away.
#[derive(Debug, Default)]
pub(crate) struct DeployLockAccess {
    background: Option<BackgroundWork>,
    operator_priority_until: Option<Instant>,
}

/// The per-app deploy lock, held by background work.
pub(crate) struct BackgroundDeployLock<'a> {
    app: &'a App,
    guard: Option<tokio::sync::MutexGuard<'a, ()>>,
}

impl Drop for BackgroundDeployLock<'_> {
    fn drop(&mut self) {
        // Release and forget the holder at once, so an operator never takes
        // another holder for background work.
        let mut access = self.app.deploy_lock_access.lock();
        self.guard.take();
        access.background = None;
    }
}

/// Try the deploy lock for background work. Never waits: `None` while the
/// lock is held or an operator has priority, and the caller tries later.
pub(crate) fn try_lock_deploy_for_background<'a>(
    app: &'a App,
    lock: &'a tokio::sync::Mutex<()>,
    work: BackgroundWork,
    now: Instant,
) -> Option<BackgroundDeployLock<'a>> {
    let mut access = app.deploy_lock_access.lock();
    if access
        .operator_priority_until
        .is_some_and(|until| now < until)
    {
        return None;
    }
    let guard = lock.try_lock().ok()?;
    access.background = Some(work);
    Some(BackgroundDeployLock {
        app,
        guard: Some(guard),
    })
}

impl ServerState {
    /// Take the deploy lock for an operator operation, or fail at once when
    /// another operator operation holds it. Background work holding it gets
    /// a moment to finish; past that the operation fails with a message that
    /// names the work. Either way background work leaves the lock to the
    /// operator's retry for `OPERATOR_PRIORITY_WINDOW`.
    pub(crate) async fn lock_deploy_for_operator<'a>(
        &self,
        app_name: &str,
        lock: &'a tokio::sync::Mutex<()>,
    ) -> Result<tokio::sync::MutexGuard<'a, ()>, String> {
        let busy = || {
            format!("Deploy already in progress for app '{app_name}'. Please wait and try again.")
        };
        let Some(app) = self.app_manager.get_app(app_name) else {
            return lock.try_lock().map_err(|_| busy());
        };
        {
            let mut access = app.deploy_lock_access.lock();
            if let Ok(guard) = lock.try_lock() {
                access.operator_priority_until = None;
                return Ok(guard);
            }
            access.background.ok_or_else(busy)?;
            access.operator_priority_until = Some(Instant::now() + OPERATOR_PRIORITY_WINDOW);
        }
        let acquired = tokio::time::timeout(OPERATOR_DEPLOY_LOCK_WAIT, lock.lock()).await;
        let mut access = app.deploy_lock_access.lock();
        let guard = acquired.map_err(|_| {
            access
                .background
                .map_or_else(busy, |work| work.busy_message(app_name))
        })?;
        access.operator_priority_until = None;
        Ok(guard)
    }
}

/// The per-app deploy lock, held by a backup. A backup does not change the
/// instance set, so while it holds the lock (the snapshot can take minutes)
/// native recovery may still claim instance slots. Dropping this closes that
/// window before it releases the lock: whoever takes the lock next, such as a
/// rollout taking its old-instance snapshot, sees every slot claimed meanwhile.
pub(crate) struct BackupDeployLock {
    app: Option<Arc<App>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for BackupDeployLock {
    fn drop(&mut self) {
        if let Some(app) = &self.app {
            *app.recovery_claims_open.lock() = false;
        }
        self.guard.take();
    }
}

impl ServerState {
    pub(crate) async fn lock_deploy_for_backup(&self, app_name: &str) -> BackupDeployLock {
        let guard = self.get_deploy_lock(app_name).await.lock_owned().await;
        let app = self.app_manager.get_app(app_name);
        if let Some(app) = &app {
            *app.recovery_claims_open.lock() = true;
        }
        BackupDeployLock {
            app,
            guard: Some(guard),
        }
    }
}

/// Run `claim`, which changes a native app's instance set, where recovery may
/// do so: under the deploy lock, or inside a backup's claim window. `None`
/// when neither is available; the next tick tries again. `claim` must not
/// wait: it holds both locks.
fn claim_instances<T>(
    app: &App,
    lock: &tokio::sync::Mutex<()>,
    now: Instant,
    claim: impl FnOnce() -> T,
) -> Option<T> {
    let mutation =
        try_lock_deploy_for_background(app, lock, BackgroundWork::AutomaticRecovery, now);
    let claims_open = app.recovery_claims_open.lock();
    if mutation.is_none() && !*claims_open {
        return None;
    }
    Some(claim())
}

/// Remove the app's dead instances and report each loss, exactly once.
fn retire_dead(app: &App, app_name: &str) -> Vec<Arc<Instance>> {
    let mut retired = Vec::new();
    for instance in survey(app).dead {
        let exit = instance.exit_report();
        if app.remove_instance(&instance.id).is_some() {
            app.record_instance_exit(exit);
            crate::metrics::remove_instance_metrics(app_name, &instance.id);
            retired.push(instance);
        }
    }
    retired
}

/// One recovery pass for one app. Every gate is only tried: if a drain, stop,
/// restore, deploy, scale, secrets update or health replacement is in the
/// way, this pass does nothing and the next tick looks again.
async fn recover_app(state: &ServerState, app: &Arc<App>, now: Instant) {
    let app_name = app.name();
    // Same gates, in the same order, as `replace_instance_if_needed`.
    let Ok(_spawning) = state.spawn_gate.try_read() else {
        return;
    };
    if state.is_draining() {
        return;
    }
    let Ok(_lifecycle) = app.lifecycle_gate.try_read() else {
        return;
    };
    if !reconciles(app) {
        return;
    }
    // Deploy, scale, secrets and idle eviction hold this lock while they
    // change the instance set; never count or remove instances under them.
    let lock = state.get_deploy_lock(&app_name).await;

    if app.config.read().compose.is_some() {
        let Some(_mutation) =
            try_lock_deploy_for_background(app, &lock, BackgroundWork::AutomaticRecovery, now)
        else {
            return;
        };
        let Ok(_recovering) = app.recovery_gate.try_lock() else {
            return;
        };
        // As for health events, a rollout owns every Compose container
        // mutation: keep the lock for the whole stack reconciliation.
        let retired = retire_dead(app, &app_name);
        for instance in &retired {
            if let Err(e) = instance.kill().await {
                tracing::error!(app = %app_name, instance = %instance.id, "Failed to kill instance: {}", e);
            }
        }
        let survey = survey(app);
        if (retired.is_empty() && survey.present >= survey.target)
            || app.restart_backoff_remaining(now).is_some()
        {
            return;
        }
        tracing::info!(app = %app_name, "Reconciling Compose replicas below the minimum");
        recover_compose(state, &app_name, app).await;
        return;
    }

    let Ok(_recovering) = app.recovery_gate.try_lock() else {
        return;
    };
    // A native start can take as long as the startup timeout, and operator
    // operations do not wait that long for the deploy lock, so only claim
    // each slot under it and start outside: a rollout that arrives
    // meanwhile finds the starting instance and retires it with the old ones.
    // The health path starts native replacements without that lock, so a
    // backup holding it must not hold recovery back either.
    let Some(retired) = claim_instances(app, &lock, now, || retire_dead(app, &app_name)) else {
        return;
    };
    for instance in retired {
        if let Err(e) = instance.kill().await {
            tracing::error!(app = %app_name, instance = %instance.id, "Failed to kill instance: {}", e);
        }
    }
    loop {
        if app.restart_backoff_remaining(now).is_some() {
            break;
        }
        let claimed = claim_instances(app, &lock, now, || {
            if !reconciles(app)
                || state
                    .drain_requested
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                return None;
            }
            let survey = survey(app);
            if survey.present >= survey.target || survey.running >= survey.limit {
                return None;
            }
            tracing::info!(
                app = %app_name,
                present = survey.present,
                min = survey.target,
                "Starting an instance to restore the minimum"
            );
            Some(app.allocate_instance())
        });
        let Some(instance) = claimed.flatten() else {
            break;
        };
        if !start_replacement(state, &app_name, app, instance).await {
            break;
        }
    }
    update_instance_count_metric(&app_name, app);
}

#[cfg(test)]
mod recovery_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn dead_instances_are_handled_without_the_management_lock_and_not_during_drain() {
        let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
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
            name: "health".into(),
            environment: "production".into(),
            path: dir.path().into(),
            // Exercise a failed replacement without leaving a child process running.
            command: vec!["./missing-app".into()],
            min_instances: 1,
            ..Default::default()
        });
        app.set_state(AppState::Running);
        let instance = app.allocate_instance();
        instance.set_state(InstanceState::Healthy);
        let name = "health/production";
        let dead = || HealthEvent::Dead {
            app: name.into(),
            instance_id: instance.id.clone(),
        };
        // A drain owns the spawn gate: the dead instance is left for it to stop.
        let draining = state.spawn_gate.write().await;
        handle_health_event(&state, dead()).await;
        assert!(app.get_instance(&instance.id).is_some());
        drop(draining);
        // A deployment of another app holds the management lock for minutes; the
        // single health loop must keep going regardless.
        let _deploying = state.management_lock.lock().await;
        tokio::time::timeout(Duration::from_secs(5), handle_health_event(&state, dead()))
            .await
            .expect("health events must not queue behind the management lock");
        assert!(app.get_instance(&instance.id).is_none());
    }

    #[tokio::test]
    async fn dead_rollout_candidate_stays_with_the_updater_until_rollback() {
        let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
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
            name: "rollback".into(),
            environment: "production".into(),
            version: "v1".into(),
            min_instances: 1,
            path: dir.path().into(),
            command: vec!["./missing-app".into()],
            ..Default::default()
        });
        let serving = app.allocate_instance();
        app.set_instance_state(&serving, InstanceState::Healthy);
        app.config.write().version = "v2".into();
        app.set_state(AppState::Deploying);
        let candidate = app.allocate_instance();
        app.suppress_instance_routing(&candidate.id);
        app.set_instance_state(&candidate, InstanceState::Stopped);
        let dead = || HealthEvent::Dead {
            app: app.name(),
            instance_id: candidate.id.clone(),
        };

        // The checker can observe the child's exit before rollback removes it.
        handle_health_event(&state, dead()).await;
        assert!(
            app.get_instance(&candidate.id).is_some(),
            "health recovery took the rollout candidate"
        );

        app.remove_instance(&candidate.id);
        app.config.write().version = "v1".into();
        app.set_last_error("v2 rollout failed");
        handle_health_event(&state, dead()).await;
        assert_eq!(app.get_instances().len(), 1);
        assert_eq!(app.get_instances()[0].id, serving.id);
        assert_eq!(app.last_error().as_deref(), Some("v2 rollout failed"));
    }

    #[tokio::test]
    async fn idle_shutdown_keeps_routing_and_cold_starts_available() {
        use std::process::Stdio;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for min_instances in [0, 1] {
            let dir = tempfile::tempdir().unwrap();
            let state = Arc::new(
                ServerState::new(
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
                .unwrap(),
            );
            let app = state.app_manager.register_app(crate::instances::AppConfig {
                name: "idle-routing".into(),
                min_instances,
                idle_timeout: Duration::ZERO,
                ..Default::default()
            });
            let name = app.name();
            state.load_balancer.register_app(app.clone());
            app.set_state(AppState::Running);
            state.cold_start.begin(&name);
            state.cold_start.mark_ready(&name);
            let idle = app.allocate_instance();
            app.set_instance_state(&idle, InstanceState::Healthy);
            let serving = (min_instances != 0).then(|| {
                let instance = app.allocate_instance();
                app.set_instance_state(&instance, InstanceState::Healthy);
                instance
            });

            // Hold native shutdown open deterministically, rather than relying
            // on a sleep landing inside the termination grace period.
            let mut child = tokio::process::Command::new("/bin/sh")
                .args(["-c", "trap '' TERM; printf ready; read line"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let mut ready = [0; 5];
            let mut stdout = child.stdout.take().unwrap();
            tokio::time::timeout(Duration::from_secs(5), stdout.read_exact(&mut ready))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&ready, b"ready");
            idle.set_process(child);
            tokio::time::sleep(Duration::from_millis(2)).await;
            let eviction_state = state.clone();
            let event = IdleEvent::InstanceIdle {
                app: name.clone(),
                instance_id: idle.id.clone(),
            };
            let eviction = tokio::spawn(async move {
                handle_idle_event(&eviction_state, event).await;
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while idle.state() != InstanceState::Draining {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(!eviction.is_finished());
            assert!(app.lifecycle_gate.try_write().is_err());

            let serving = serving.unwrap_or_else(|| {
                // The final idle instance must publish scale-to-zero before
                // allowing a new cold start; shutdown must not reset it later.
                let _admission = app.lifecycle_gate.try_read().unwrap();
                assert_eq!(app.state(), AppState::Idle);
                assert!(state.cold_start.begin(&name).leader);
                let instance = app.allocate_instance();
                app.set_state(AppState::Running);
                app.set_instance_state(&instance, InstanceState::Healthy);
                state.cold_start.mark_ready(&name);
                instance
            });
            let resolution = crate::proxy::resolve_app_backend(
                &state.load_balancer,
                &state.cold_start,
                &state.drain_requested,
                &state.spawn_gate,
                &name,
            )
            .await;
            let crate::proxy::BackendResolution::Ready { backend, .. } = resolution else {
                panic!("idle shutdown blocked a healthy backend");
            };
            assert_eq!(backend.instance_id(), serving.id);
            stdin.write_all(b"done\n").await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), eviction)
                .await
                .unwrap()
                .unwrap();
            assert!(app.get_instance(&idle.id).is_none());
            assert_eq!(app.state(), AppState::Running);
            assert!(!state.cold_start.begin(&name).leader);
        }
    }

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

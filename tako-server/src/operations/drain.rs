use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::Serialize;
use tako_core::{AppState, Command, InstanceState, Response};
use tokio::time::Instant;

const MAX_DRAIN_SECONDS: u64 = 3600;
const CLEANUP_RETRY_SECONDS: u64 = 30;

#[derive(Debug, Serialize)]
pub(crate) struct DrainReport {
    status: &'static str,
    pub(crate) completed: bool,
    local_completed: bool,
    other_processes_quiescent: bool,
    counts_scope: &'static str,
    timed_out: bool,
    http_drained: bool,
    forced_instances: usize,
    forced_http_requests: u64,
    forced_proxy_requests: usize,
    forced_proxy_shutdown_requests: usize,
    remaining_proxy_requests: usize,
    forced_workflow_workers: usize,
    remaining_instances: usize,
    remaining_workflow_workers: usize,
    remaining_workflow_background_tasks: usize,
    errors: Vec<String>,
}

pub(super) fn blocked_while_draining(command: &Command) -> bool {
    match command {
        Command::PrepareRelease { .. }
        | Command::PrepareReleaseUpload { .. }
        | Command::PrepareDeploy { .. }
        | Command::FinalizeRelease { .. }
        | Command::RunRelease { .. }
        | Command::Deploy { .. }
        | Command::BackupNow { .. }
        | Command::RestoreBackup { .. }
        | Command::Scale { .. }
        | Command::Rollback { .. }
        | Command::UpdateSecrets { .. }
        | Command::EnqueueRun { .. }
        | Command::RegisterSchedules { .. }
        | Command::ClaimRun { .. }
        | Command::Signal { .. } => true,
        Command::Hello { .. }
        | Command::ServerInfo
        | Command::DrainServer { .. }
        | Command::ResumeServer
        | Command::CleanupPreparedDeploy { .. }
        | Command::CleanupRelease { .. }
        | Command::CheckDeploySpace { .. }
        | Command::ListBackups { .. }
        | Command::BackupStatus { .. }
        | Command::BackupDownloadUrl { .. }
        | Command::Stop { .. }
        | Command::Delete { .. }
        | Command::Status { .. }
        | Command::List
        | Command::ListReleases { .. }
        | Command::Routes
        | Command::GetSecretsHash { .. }
        | Command::EnterUpgrading { .. }
        | Command::ExitUpgrading { .. }
        | Command::InjectChallengeToken { .. }
        | Command::HeartbeatRun { .. }
        | Command::SaveStep { .. }
        | Command::CompleteRun { .. }
        | Command::CancelRun { .. }
        | Command::FailRun { .. }
        | Command::DeferRun { .. }
        | Command::WaitForEvent { .. }
        | Command::ChannelPublish { .. } => false,
    }
}

impl crate::ServerState {
    fn prepare_server_drain(
        &self,
        timeout_seconds: u64,
    ) -> Result<(crate::state_store::DrainLease, Instant), Response> {
        if timeout_seconds > MAX_DRAIN_SECONDS {
            return Err(Response::error(
                "Drain timeout must be between 0 and 3600 seconds.",
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
        let operation = match self.state_store.try_drain_operation() {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                return Err(Response::error(
                    "Another server drain/resume is in progress; retry shortly.",
                ));
            }
            Err(error) => return Err(Response::error(error.to_string())),
        };
        let wall_deadline = crate::drain_observer::unix_millis()
            .saturating_add(timeout_seconds.saturating_mul(1000));
        if let Err(error) = self.state_store.request_drain_until(wall_deadline) {
            return Err(Response::error(format!(
                "Could not persist server drain: {error}"
            )));
        }
        self.drain_requested.store(true, Ordering::Release);
        self.proxy_requests.begin_drain();
        Ok((operation, deadline))
    }

    // Management dispatch calls this inside its owned task, BEFORE taking management_lock.
    pub(crate) async fn drain_server_before_management(&self, timeout_seconds: u64) -> Response {
        let (_operation, deadline) = match self.prepare_server_drain(timeout_seconds) {
            Ok(prepared) => prepared,
            Err(response) => return response,
        };
        let _management = match tokio::time::timeout_at(deadline, self.management_lock.lock()).await
        {
            Ok(guard) => guard,
            Err(_) => {
                let forced_proxy_requests = self.proxy_requests.active_count();
                let forced_proxy_shutdown_requests = self.proxy_requests.force_drain();
                return Response::ok(serde_json::json!({
                    "status": "draining", "completed": false, "local_completed": false,
                    "other_processes_quiescent": false, "counts_scope": "local_process",
                    "timed_out": true, "http_drained": false,
                    "forced_proxy_requests": forced_proxy_requests,
                    "forced_proxy_shutdown_requests": forced_proxy_shutdown_requests,
                    "remaining_proxy_requests": self.proxy_requests.active_count(),
                    "errors": ["Drain intent persisted; an existing management operation exceeded the drain deadline. Local cleanup will continue through the observer."]
                }));
            }
        };
        self.finish_server_drain(deadline).await
    }

    // Direct dispatch callers already hold management_lock.
    pub(crate) async fn drain_server(&self, timeout_seconds: u64) -> Response {
        let (_operation, deadline) = match self.prepare_server_drain(timeout_seconds) {
            Ok(prepared) => prepared,
            Err(response) => return response,
        };
        self.finish_server_drain(deadline).await
    }

    async fn finish_server_drain(&self, deadline: Instant) -> Response {
        let mut report = self.drain_runtime_until(deadline).await;
        if report.local_completed {
            loop {
                match self.state_store.try_drain_activity(true) {
                    Ok(Some(_exclusive)) => {
                        report.other_processes_quiescent = true;
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        report.errors.push(error.to_string());
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    report.timed_out = true;
                    break;
                }
                tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_millis(10)))
                    .await;
            }
        }
        report.completed = report.local_completed && report.other_processes_quiescent;
        report.status = if report.completed {
            "drained"
        } else {
            "draining"
        };
        Response::ok(report)
    }

    pub(crate) async fn drain_runtime_until(&self, deadline: Instant) -> DrainReport {
        self.drain_runtime_with_cleanup_until(deadline, deadline)
            .await
    }

    pub(crate) async fn retry_runtime_drain(&self) -> DrainReport {
        let now = Instant::now();
        self.drain_runtime_with_cleanup_until(now, now + Duration::from_secs(CLEANUP_RETRY_SECONDS))
            .await
    }

    async fn drain_runtime_with_cleanup_until(
        &self,
        deadline: Instant,
        cleanup_deadline: Instant,
    ) -> DrainReport {
        self.drain_requested.store(true, Ordering::Release);
        self.proxy_requests.begin_drain();
        // In-flight cold starts and replacements finish under the read side first;
        // later ones observe the flag and never spawn. A spawn still waiting for
        // readiness must not extend the requested drain window, so the wait ends
        // at the cleanup deadline and the report keeps cleanup pending for the
        // observer, which retries once the spawn has released the gate.
        let spawns_settled =
            match tokio::time::timeout_at(cleanup_deadline, self.spawn_gate.write()).await {
                Ok(guard) => Some(guard),
                Err(_) => None,
            };
        self.workflows.begin_server_drain();
        let apps: Vec<_> = self
            .app_manager
            .list_apps()
            .iter()
            .filter_map(|name| self.app_manager.get_app(name))
            .collect();
        let mut instances = Vec::new();
        for app in &apps {
            app.set_state(AppState::Stopped);
            for instance in app.get_instances() {
                app.set_instance_state(&instance, InstanceState::Draining);
                instances.push((app.clone(), instance));
            }
        }

        loop {
            let http_done = instances
                .iter()
                .all(|(_, instance)| instance.in_flight() == 0);
            if http_done
                && self.proxy_requests.active_count() == 0
                && self.workflows.server_drain_status().is_complete()
            {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_millis(10)))
                .await;
        }

        let forced_instances = instances
            .iter()
            .filter(|(_, instance)| instance.in_flight() > 0)
            .count();
        let forced_http_requests = instances
            .iter()
            .map(|(_, instance)| instance.in_flight())
            .sum();
        let forced_proxy_requests = self.proxy_requests.active_count();
        let forced_proxy_shutdown_requests = if forced_proxy_requests > 0 {
            self.proxy_requests.force_drain()
        } else {
            0
        };
        let workflows_before_stop = self.workflows.server_drain_status();
        let (forced_workflow_workers, workflow_errors) = self.workflows.force_server_drain();
        let mut report = DrainReport {
            status: "drained",
            completed: false,
            local_completed: false,
            other_processes_quiescent: false,
            counts_scope: "local_process",
            timed_out: forced_instances > 0
                || forced_proxy_requests > 0
                || !workflows_before_stop.is_complete(),
            http_drained: forced_instances == 0 && forced_proxy_requests == 0,
            forced_instances,
            forced_http_requests,
            forced_proxy_requests,
            forced_proxy_shutdown_requests,
            remaining_proxy_requests: self.proxy_requests.active_count(),
            forced_workflow_workers,
            remaining_instances: 0,
            remaining_workflow_workers: workflows_before_stop.workers,
            remaining_workflow_background_tasks: workflows_before_stop.background_tasks,
            errors: Vec::new(),
        };
        if workflow_errors > 0 {
            report.errors.push(format!(
                "Failed to signal {workflow_errors} workflow workers"
            ));
        }
        if spawns_settled.is_none() {
            report.timed_out = true;
            report
                .errors
                .push("instance spawns were still active at the drain deadline".into());
        }
        for (app, instance) in instances {
            match tokio::time::timeout_at(cleanup_deadline, instance.kill()).await {
                Ok(Ok(())) => {
                    app.remove_instance(&instance.id);
                    crate::metrics::remove_instance_metrics(&app.name(), &instance.id);
                }
                Ok(Err(error)) => {
                    report
                        .errors
                        .push(format!("{} / {}: {error}", app.name(), instance.id))
                }
                Err(_) => {
                    report.timed_out = true;
                    report.errors.push(format!(
                        "{} / {}: stop deadline expired",
                        app.name(),
                        instance.id
                    ));
                }
            }
        }
        for app in apps {
            let config = app.config.read().clone();
            if config.compose.is_some() {
                match crate::compose_runtime::for_config(&config) {
                    Ok(compose) => match tokio::time::timeout_at(cleanup_deadline, compose.stop())
                        .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => report.errors.push(format!("{}: {error}", app.name())),
                        Err(_) => {
                            report.timed_out = true;
                            report
                                .errors
                                .push(format!("{}: compose stop deadline expired", app.name()));
                        }
                    },
                    Err(error) => report.errors.push(format!("{}: {error}", app.name())),
                }
            }
            report.remaining_instances += app.get_instances().len();
            crate::runtime_events::update_instance_count_metric(&app.name(), &app);
        }
        let workflows = self.workflows.server_drain_status();
        report.remaining_workflow_workers = workflows.workers;
        report.remaining_workflow_background_tasks = workflows.background_tasks;
        report.remaining_proxy_requests = self.proxy_requests.active_count();
        report.completed = report.remaining_instances == 0
            && report.remaining_proxy_requests == 0
            && workflows.is_complete()
            && report.errors.is_empty();
        report.local_completed = report.completed;
        if report.local_completed {
            self.drain_activity.lock().take();
        }
        report
    }

    pub(crate) async fn resume_server(&self) -> Response {
        let _operation = match self.state_store.try_drain_operation() {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                return Response::error(
                    "Another server drain/resume is in progress; retry shortly.",
                );
            }
            Err(error) => return Response::error(error.to_string()),
        };
        if !self.is_draining() {
            return Response::ok(serde_json::json!({"status":"resumed", "already_running":true}));
        }
        let requested = match self.state_store.drain_requested() {
            Ok(value) => value,
            Err(error) => return Response::error(error.to_string()),
        };
        let cleanup = self.retry_runtime_drain().await;
        if !cleanup.completed {
            return Response::error(format!(
                "Server remains drained; cleanup is incomplete: {}",
                serde_json::to_string(&cleanup).unwrap()
            ));
        }
        if requested {
            match self.state_store.try_drain_activity(true) {
                Ok(Some(_exclusive)) => {}
                Ok(None) => {
                    return Response::error(
                        "Server remains drained; other processes are still cleaning up. Retry resume shortly.",
                    );
                }
                Err(error) => return Response::error(error.to_string()),
            }
        }
        match self.restore_drained_runtime(requested).await {
            Ok(()) => {
                Response::ok(serde_json::json!({"status":"resumed", "already_running":false}))
            }
            Err(error) => Response::error(error),
        }
    }

    // Caller holds both the local management and cross-process operation locks.
    pub(crate) async fn restore_drained_runtime(&self, clear_intent: bool) -> Result<(), String> {
        let activity = self
            .state_store
            .try_drain_activity(false)
            .map_err(|error| error.to_string())?
            .ok_or("Server remains drained; activity lease is busy")?;
        *self.drain_activity.lock() = Some(activity);
        for name in self.app_manager.list_apps() {
            self.app_manager.remove_app(&name);
            self.load_balancer.unregister_app(&name);
            self.routes.write().remove_app_routes(&name);
            self.cold_start.reset(&name);
        }
        if let Err(error) = self.restore_apps_from_state_store(true).await {
            let cleanup = self.retry_runtime_drain().await;
            return Err(format!(
                "Server remains drained; restore failed: {error}; cleanup completed: {}",
                cleanup.completed
            ));
        }
        if !self.proxy_requests.resume() {
            let cleanup = self.retry_runtime_drain().await;
            return Err(format!(
                "Server remains drained; proxy requests still active; cleanup completed: {}",
                cleanup.completed
            ));
        }
        if clear_intent && let Err(error) = self.state_store.set_drain_requested(false) {
            let cleanup = self.retry_runtime_drain().await;
            return Err(format!(
                "Server remains drained; could not persist resume: {error}; cleanup completed: {}",
                cleanup.completed
            ));
        }
        self.drain_requested.store(false, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests;

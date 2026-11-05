use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::Serialize;
use tako_core::{AppState, Command, InstanceState, Response};
use tokio::time::Instant;

const MAX_DRAIN_SECONDS: u64 = 3600;

#[derive(Debug, Serialize)]
struct DrainReport {
    status: &'static str,
    completed: bool,
    timed_out: bool,
    http_drained: bool,
    forced_instances: usize,
    forced_http_requests: u64,
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
    pub(crate) async fn drain_server(&self, timeout_seconds: u64) -> Response {
        if timeout_seconds > MAX_DRAIN_SECONDS {
            return Response::error("Drain timeout must be between 0 and 3600 seconds.");
        }
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
        if let Err(error) = self.state_store.set_drain_requested(true) {
            return Response::error(format!("Could not persist server drain: {error}"));
        }
        self.drain_requested.store(true, Ordering::Release);
        Response::ok(self.drain_runtime_until(deadline).await)
    }

    async fn drain_runtime_until(&self, deadline: Instant) -> DrainReport {
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
            if http_done && self.workflows.server_drain_status().is_complete() {
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
        let workflows_before_stop = self.workflows.server_drain_status();
        let (forced_workflow_workers, workflow_errors) = self.workflows.force_server_drain();
        let mut report = DrainReport {
            status: "drained",
            completed: false,
            timed_out: forced_instances > 0 || !workflows_before_stop.is_complete(),
            http_drained: forced_instances == 0,
            forced_instances,
            forced_http_requests,
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
        for (app, instance) in instances {
            match tokio::time::timeout_at(deadline, instance.kill()).await {
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
                    Ok(compose) => match tokio::time::timeout_at(deadline, compose.stop()).await {
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
        report.completed =
            report.remaining_instances == 0 && workflows.is_complete() && report.errors.is_empty();
        report
    }

    pub(crate) async fn resume_server(&self) -> Response {
        if !self.is_draining() {
            return Response::ok(serde_json::json!({"status":"resumed", "already_running":true}));
        }
        let cleanup = self.drain_runtime_until(Instant::now()).await;
        if !cleanup.completed {
            return Response::error(format!(
                "Server remains drained; cleanup is incomplete: {}",
                serde_json::to_string(&cleanup).unwrap()
            ));
        }
        for name in self.app_manager.list_apps() {
            self.app_manager.remove_app(&name);
            self.load_balancer.unregister_app(&name);
            self.routes.write().remove_app_routes(&name);
            self.cold_start.reset(&name);
        }
        if let Err(error) = self.restore_apps_from_state_store(true).await {
            let cleanup = self.drain_runtime_until(Instant::now()).await;
            return Response::error(format!(
                "Server remains drained; restore failed: {error}; cleanup completed: {}",
                cleanup.completed
            ));
        }
        if let Err(error) = self.state_store.set_drain_requested(false) {
            let cleanup = self.drain_runtime_until(Instant::now()).await;
            return Response::error(format!(
                "Server remains drained; could not persist resume: {error}; cleanup completed: {}",
                cleanup.completed
            ));
        }
        self.drain_requested.store(false, Ordering::Release);
        Response::ok(serde_json::json!({"status":"resumed", "already_running":false}))
    }
}

#[cfg(test)]
mod tests;

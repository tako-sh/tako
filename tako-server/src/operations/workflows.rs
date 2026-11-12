//! Operator commands for an app's workflow runs.
//!
//! They act on this server's view of the app's workflow store: its local
//! SQLite store, or the shared Postgres store every server of the app uses.
//! Stopped apps keep their store, so their runs stay inspectable.

use crate::socket::Response;
use crate::workflows::WorkflowManagerError;
use tako_core::WorkflowRunStatus;
use tako_workflows::enqueue::RunsDbError;

impl crate::ServerState {
    pub(crate) async fn list_workflow_runs(
        &self,
        app: &str,
        status: Option<WorkflowRunStatus>,
        name: Option<String>,
        limit: Option<u32>,
    ) -> Response {
        if let Some(response) = self.require_app(app) {
            return response;
        }
        respond(self.workflows.list_runs(app, status, name, limit).await)
    }

    pub(crate) async fn get_workflow_run(&self, app: &str, id: &str) -> Response {
        if let Some(response) = self.require_app(app) {
            return response;
        }
        respond(self.workflows.get_run(app, id).await)
    }

    pub(crate) async fn retry_workflow_run(&self, app: &str, id: &str) -> Response {
        if let Some(response) = self.require_app(app) {
            return response;
        }
        let response = self.workflows.retry_run(app, id).await;
        if response.is_ok() {
            tracing::info!(app, run_id = id, "Workflow run retried by operator");
        }
        respond(response)
    }

    pub(crate) async fn cancel_workflow_run(&self, app: &str, id: &str) -> Response {
        if let Some(response) = self.require_app(app) {
            return response;
        }
        let response = self.workflows.cancel_run(app, id).await;
        if response.is_ok() {
            tracing::info!(app, run_id = id, "Workflow run cancelled by operator");
        }
        respond(response)
    }

    fn require_app(&self, app: &str) -> Option<Response> {
        self.app_manager
            .get_app(app)
            .is_none()
            .then(|| Response::error(format!("App not found: {app}")))
    }
}

fn respond<T: serde::Serialize>(result: Result<T, WorkflowManagerError>) -> Response {
    match result {
        Ok(data) => Response::ok(data),
        Err(WorkflowManagerError::InvalidRequest(message)) => Response::error(message),
        Err(WorkflowManagerError::Db(
            error @ (RunsDbError::RunNotFound { .. }
            | RunsDbError::RunNotRetryable { .. }
            | RunsDbError::RunAlreadyFinished { .. }),
        )) => Response::error(error.to_string()),
        Err(error) => {
            tracing::warn!(%error, "Workflow operator command failed");
            Response::error(format!("Workflow store error: {error}"))
        }
    }
}

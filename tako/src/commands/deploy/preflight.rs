use std::time::Instant;

use crate::config::ServersToml;
use crate::management_http::{self, ManagementClient};
use crate::output;
use crate::validation::validate_no_route_conflicts;
use tako_core::Command;
use tracing::Instrument;

use super::remote::parse_existing_routes_response;
use super::task_tree::DeployTaskTreeController;
use super::{PreflightPhaseResult, ServerCheck};

pub(super) async fn run_server_preflight_checks(
    server_names: Vec<String>,
    servers: ServersToml,
    deploy_app_name: String,
    routes: Vec<String>,
    task_tree: Option<DeployTaskTreeController>,
) -> Result<PreflightPhaseResult, String> {
    let start = Instant::now();
    let mut check_set = tokio::task::JoinSet::new();

    for server_name in &server_names {
        let server = servers
            .get(server_name)
            .ok_or_else(|| format!("Server '{}' not found in servers.toml", server_name))?;
        let name = server_name.clone();
        let task_tree_for_task = task_tree.clone();
        let check_name = name.clone();
        let host = server.host.clone();
        let check_deploy_app_name = deploy_app_name.clone();
        let check_routes = routes.clone();
        let span = output::scope(&name);
        check_set.spawn(
            async move {
                let result = async {
                    let _t = output::timed("Preflight check");

                    // Mark "Preflight" as running in the task tree — this runs
                    // concurrently with the build phase so the user sees progress.
                    if let Some(task_tree) = &task_tree_for_task {
                        task_tree.mark_deploy_step_running(&name, "connecting");
                    }

                    let mut client =
                        map_preflight_management_result(&host, ManagementClient::new(&host).await)?;
                    let info = map_preflight_management_result(
                        &host,
                        client.send(&Command::ServerInfo).await,
                    )?;
                    let info = management_http::parse_ok_data::<tako_core::ServerRuntimeInfo>(
                        info,
                        "server info",
                    )?;
                    let mode = info.mode;

                    // Disk space check
                    let disk = map_preflight_management_result(
                        &host,
                        client
                            .send(&Command::CheckDeploySpace {
                                min_free_bytes: DEPLOY_MIN_FREE_DISK_BYTES,
                            })
                            .await,
                    )?;
                    if let Some(message) = disk.error_message() {
                        return Err(management_http::ManagementError::Message(
                            message.to_string(),
                        ));
                    }

                    // Route conflict check
                    let routes_response = map_preflight_management_result(
                        &host,
                        client.send(&Command::Routes).await,
                    )?;
                    let existing = parse_existing_routes_response(routes_response)
                        .map_err(management_http::ManagementError::Message)?;
                    validate_no_route_conflicts(&existing, &check_deploy_app_name, &check_routes)
                        .map_err(|e| {
                        management_http::ManagementError::Message(format!("Route conflict: {}", e))
                    })?;

                    // Mark "Preflight" as succeeded.
                    if let Some(task_tree) = &task_tree_for_task {
                        task_tree.succeed_deploy_step(&name, "connecting", None);
                    }

                    Ok::<_, management_http::ManagementError>(ServerCheck { name, mode })
                }
                .await;
                if let Err(error) = &result
                    && let Some(task_tree) = &task_tree_for_task
                {
                    task_tree.fail_preflight_check(&check_name, error.to_string());
                }
                result
            }
            .instrument(span),
        );
    }

    while let Some(result) = check_set.join_next().await {
        let check = result
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;

        if check.mode == tako_core::UpgradeMode::Upgrading {
            if let Some(task_tree) = &task_tree {
                task_tree.fail_preflight_check(&check.name, "Server is currently upgrading");
            }
            return Err(format!(
                "{} is currently upgrading. Retry after the upgrade completes.",
                check.name,
            ));
        }
    }

    Ok(PreflightPhaseResult {
        elapsed: start.elapsed(),
    })
}

/// Fixed minimum free disk space required on the remote server.
const DEPLOY_MIN_FREE_DISK_BYTES: u64 = 256 * 1024 * 1024;

fn map_preflight_management_result<T>(
    host: &str,
    result: Result<T, management_http::ManagementError>,
) -> Result<T, management_http::ManagementError> {
    result.map_err(|error| {
        management_http::ManagementError::Message(format_preflight_management_error(host, &error))
    })
}

fn format_preflight_management_error(
    host: &str,
    error: &management_http::ManagementError,
) -> String {
    let message = error.to_string();
    if looks_like_management_auth_error(&message) {
        return format!(
            "Management auth failed for {host}. Run `tako servers add {host}` again to enroll this machine, then try again."
        );
    }
    if looks_like_management_connection_error(&message) {
        return format!(
            "tako-server is not reachable at {host}:{}. Run `tako servers add <admin-user>@{host}` to install it, or start the service, then try again.",
            management_http::MANAGEMENT_PORT
        );
    }
    message
}

fn looks_like_management_auth_error(message: &str) -> bool {
    message.contains("management auth failed")
        || message.contains("management auth required")
        || message.contains("key not found")
}

fn looks_like_management_connection_error(message: &str) -> bool {
    message.contains("error trying to connect")
        || message.contains("Connection refused")
        || message.contains("connection refused")
        || message.contains("connection reset")
        || message.contains("connection closed")
        || message.contains("dns error")
        || message.contains("failed to lookup address")
        || message.contains("operation timed out")
        || message.contains("timed out")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_min_free_disk_is_256mb() {
        assert_eq!(DEPLOY_MIN_FREE_DISK_BYTES, 256 * 1024 * 1024);
    }

    #[test]
    fn preflight_connection_error_points_to_install_or_start() {
        let error = management_http::ManagementError::Message(
            "error trying to connect: tcp connect error: Connection refused".to_string(),
        );

        let message = format_preflight_management_error("prod.example.com", &error);

        assert_eq!(
            message,
            "tako-server is not reachable at prod.example.com:9844. Run `tako servers add <admin-user>@prod.example.com` to install it, or start the service, then try again."
        );
    }

    #[test]
    fn preflight_auth_error_points_to_reenrollment() {
        let error = management_http::ManagementError::Message("management auth failed".to_string());

        let message = format_preflight_management_error("prod.example.com", &error);

        assert_eq!(
            message,
            "Management auth failed for prod.example.com. Run `tako servers add prod.example.com` again to enroll this machine, then try again."
        );
    }

    #[test]
    fn preflight_remote_server_errors_pass_through() {
        let error = management_http::ManagementError::Message(
            "Server has less than 256 MB free disk space".to_string(),
        );

        let message = format_preflight_management_error("prod.example.com", &error);

        assert_eq!(message, "Server has less than 256 MB free disk space");
    }
}

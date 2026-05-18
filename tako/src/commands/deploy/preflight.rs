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

                    let mut client = ManagementClient::new(&host).await?;
                    let info = client.send(&Command::ServerInfo).await?;
                    let info = management_http::parse_ok_data::<tako_core::ServerRuntimeInfo>(
                        info,
                        "server info",
                    )?;
                    let mode = info.mode;

                    // Disk space check
                    let disk = client
                        .send(&Command::CheckDeploySpace {
                            min_free_bytes: DEPLOY_MIN_FREE_DISK_BYTES,
                        })
                        .await?;
                    if let Some(message) = disk.error_message() {
                        return Err(management_http::ManagementError::Message(
                            message.to_string(),
                        ));
                    }

                    // Route conflict check
                    let existing =
                        parse_existing_routes_response(client.send(&Command::Routes).await?)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_min_free_disk_is_256mb() {
        assert_eq!(DEPLOY_MIN_FREE_DISK_BYTES, 256 * 1024 * 1024);
    }
}

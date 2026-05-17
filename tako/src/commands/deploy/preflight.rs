use std::collections::HashMap;
use std::time::Instant;

use crate::config::ServersToml;
use crate::output;
use crate::shell::shell_single_quote;
use crate::ssh::{SshClient, SshConfig};
use crate::validation::validate_no_route_conflicts;
use tracing::Instrument;

use super::format::format_size;
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
        let ssh_config = SshConfig::from_server(&server.host, server.port);
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

                    let mut ssh = SshClient::new(ssh_config);
                    ssh.connect().await?;
                    let info = ssh.tako_server_info().await?;

                    let mut mode = info.mode;

                    if mode == tako_core::UpgradeMode::Upgrading {
                        let reset_cmd = SshClient::run_with_root_or_sudo(
                            "sqlite3 /opt/tako/tako.db \
                         \"UPDATE server_state SET server_mode = 'normal' WHERE id = 1; \
                          DELETE FROM upgrade_lock WHERE id = 1;\"",
                        );
                        if ssh.exec_checked(&reset_cmd).await.is_ok() {
                            let _ = ssh.tako_restart().await;
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            if let Ok(new_info) = ssh.tako_server_info().await {
                                mode = new_info.mode;
                            }
                        }
                    }

                    // Disk space check
                    ensure_remote_disk_space(&ssh)
                        .await
                        .map_err(|e| crate::ssh::SshError::Connection(e.to_string()))?;

                    // Route conflict check
                    let existing = parse_existing_routes_response(ssh.tako_routes().await?)
                        .map_err(|e| crate::ssh::SshError::Connection(e.to_string()))?;
                    validate_no_route_conflicts(&existing, &check_deploy_app_name, &check_routes)
                        .map_err(|e| {
                        crate::ssh::SshError::Connection(format!("Route conflict: {}", e))
                    })?;

                    // Mark "Preflight" as succeeded — connection stays open for
                    // the deploy phase to reuse.
                    if let Some(task_tree) = &task_tree_for_task {
                        task_tree.succeed_deploy_step(&name, "connecting", None);
                    }

                    Ok::<_, crate::ssh::SshError>((ServerCheck { name, mode }, ssh))
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

    let mut ssh_clients = HashMap::new();
    while let Some(result) = check_set.join_next().await {
        let (check, ssh) = result
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

        ssh_clients.insert(check.name.clone(), ssh);
    }

    Ok(PreflightPhaseResult {
        ssh_clients,
        elapsed: start.elapsed(),
    })
}

const DEPLOY_DISK_CHECK_PATH: &str = "/opt/tako";
/// Fixed minimum free disk space required on the remote server.
const DEPLOY_MIN_FREE_DISK_BYTES: u64 = 256 * 1024 * 1024;

pub(super) fn parse_df_available_kb(stdout: &str) -> Result<u64, String> {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| "empty df output".to_string())?;
    line.parse::<u64>()
        .map_err(|_| format!("unexpected df output: '{line}'"))
}

async fn ensure_remote_disk_space(
    ssh: &SshClient,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cmd = format!(
        "df -Pk {} | awk 'NR==2 {{print $4}}'",
        shell_single_quote(DEPLOY_DISK_CHECK_PATH)
    );
    let output = ssh.exec(&cmd).await?;
    if !output.success() {
        return Err(format!(
            "Failed to check free disk space under {}: {}",
            DEPLOY_DISK_CHECK_PATH,
            output.combined().trim()
        )
        .into());
    }

    let available_kb = parse_df_available_kb(&output.stdout)
        .map_err(|e| format!("Failed to parse free disk space: {}", e))?;
    let available_bytes = available_kb.saturating_mul(1024);
    if available_bytes < DEPLOY_MIN_FREE_DISK_BYTES {
        return Err(format!(
            "Insufficient disk space under {}. Required: at least {}. Available: {}.",
            DEPLOY_DISK_CHECK_PATH,
            format_size(DEPLOY_MIN_FREE_DISK_BYTES),
            format_size(available_bytes),
        )
        .into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_df_available_kb_accepts_numeric_output() {
        assert_eq!(parse_df_available_kb("12345\n").unwrap(), 12345);
        assert_eq!(parse_df_available_kb("  98765  ").unwrap(), 98765);
    }

    #[test]
    fn parse_df_available_kb_rejects_empty_or_non_numeric_output() {
        assert!(parse_df_available_kb("").is_err());
        assert!(parse_df_available_kb("N/A").is_err());
        assert!(parse_df_available_kb("12.5").is_err());
    }

    #[test]
    fn deploy_min_free_disk_is_256mb() {
        assert_eq!(DEPLOY_MIN_FREE_DISK_BYTES, 256 * 1024 * 1024);
    }
}

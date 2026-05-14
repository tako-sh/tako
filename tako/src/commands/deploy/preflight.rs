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

                    Ok::<_, crate::ssh::SshError>((
                        ServerCheck {
                            name,
                            mode,
                            dns_provider: info.dns_provider,
                        },
                        ssh,
                    ))
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

    let mut checks = Vec::new();
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
        checks.push(check);
    }

    Ok(PreflightPhaseResult {
        checks,
        ssh_clients,
        elapsed: start.elapsed(),
    })
}

pub(super) fn check_wildcard_dns_support(
    routes: &[String],
    checks: &[ServerCheck],
) -> Result<(), Box<dyn std::error::Error>> {
    let wildcard_routes: Vec<_> = routes.iter().filter(|r| r.starts_with("*.")).collect();
    if wildcard_routes.is_empty() {
        return Ok(());
    }

    if checks.iter().all(|c| c.dns_provider.is_some()) {
        tracing::debug!("All servers support wildcard domains");
        return Ok(());
    }

    let missing: Vec<_> = checks
        .iter()
        .filter(|c| c.dns_provider.is_none())
        .map(|c| c.name.as_str())
        .collect();
    let route_list = wildcard_routes
        .iter()
        .map(|r| r.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    Err(format!(
        "Server(s) {} need DNS-01 for wildcard route(s) {route_list}\n\
         Run `tako servers configure <name>` for each listed server.",
        missing.join(", "),
    )
    .into())
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
    fn check_wildcard_dns_support_passes_without_wildcards() {
        let routes = vec!["api.example.com".to_string()];
        let checks = vec![ServerCheck {
            name: "prod-1".to_string(),
            mode: tako_core::UpgradeMode::Normal,
            dns_provider: None,
        }];
        assert!(check_wildcard_dns_support(&routes, &checks).is_ok());
    }

    #[test]
    fn check_wildcard_dns_support_passes_when_all_have_dns() {
        let routes = vec!["*.example.com".to_string()];
        let checks = vec![ServerCheck {
            name: "prod-1".to_string(),
            mode: tako_core::UpgradeMode::Normal,
            dns_provider: Some("cloudflare".to_string()),
        }];
        assert!(check_wildcard_dns_support(&routes, &checks).is_ok());
    }

    #[test]
    fn check_wildcard_dns_support_fails_when_server_lacks_dns() {
        let routes = vec!["*.example.com".to_string()];
        let checks = vec![
            ServerCheck {
                name: "prod-1".to_string(),
                mode: tako_core::UpgradeMode::Normal,
                dns_provider: Some("cloudflare".to_string()),
            },
            ServerCheck {
                name: "prod-2".to_string(),
                mode: tako_core::UpgradeMode::Normal,
                dns_provider: None,
            },
        ];
        let err = check_wildcard_dns_support(&routes, &checks).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("prod-2"), "should name the server: {msg}");
        assert!(
            msg.contains("servers configure"),
            "should suggest the command: {msg}"
        );
    }

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

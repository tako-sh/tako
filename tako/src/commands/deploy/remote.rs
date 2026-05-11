use std::future::Future;
use std::path::Path;
use std::time::Instant;

use crate::output;
use crate::shell::shell_single_quote;
use crate::ssh::{SshClient, SshConfig};
use tako_core::{Command, Response};

use super::DeployConfig;
use super::format::{format_deploy_step_failure, format_size};
use super::task_tree::DeployTaskTreeController;

/// Artifacts smaller than this upload fast enough that the progress bar just
/// flashes on and off. Skip the live bar below this size.
const PROGRESS_BAR_MIN_BYTES: u64 = 10 * 1024 * 1024;

type ReleaseCommandResult = Option<Result<(), String>>;
type ReleaseCommandSender = tokio::sync::watch::Sender<ReleaseCommandResult>;
type ReleaseCommandReceiver = tokio::sync::watch::Receiver<ReleaseCommandResult>;

pub(super) fn parse_existing_routes_response(
    response: Response,
) -> Result<Vec<(String, Vec<String>)>, String> {
    match response {
        Response::Ok { data } => Ok(data
            .get("routes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let app = item.get("app")?.as_str()?.to_string();
                        let routes = item
                            .get("routes")
                            .and_then(|r| r.as_array())
                            .map(|r| {
                                r.iter()
                                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        Some((app, routes))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()),
        Response::Error { message } => Err(format!("tako-server error (routes): {}", message)),
    }
}

pub(super) fn extract_server_error_message(response: &str) -> String {
    serde_json::from_str::<serde_json::Value>(response)
        .ok()
        .and_then(|v| v["message"].as_str().map(String::from))
        .map(|message| {
            message
                .strip_prefix("Deploy failed: ")
                .unwrap_or(&message)
                .to_string()
        })
        .unwrap_or_else(|| response.to_string())
}

pub(super) fn deploy_response_has_error(response: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(response)
        .ok()
        .and_then(|value| {
            value
                .get("status")
                .and_then(|status| status.as_str())
                .map(|status| status == "error")
        })
        .unwrap_or(false)
}

pub(super) fn cleanup_partial_release_command(release_dir: &str) -> String {
    format!("rm -rf {}", shell_single_quote(release_dir))
}

pub(super) async fn cleanup_partial_release(
    ssh: &SshClient,
    release_dir: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ssh.exec_checked(&cleanup_partial_release_command(release_dir))
        .await?;
    Ok(())
}

pub(super) async fn remote_directory_exists(
    ssh: &SshClient,
    path: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let quoted = shell_single_quote(path);
    let cmd = format!("if [ -d {quoted} ]; then echo yes; else echo no; fi");
    let output = ssh.exec(&cmd).await?;
    if !output.success() {
        return Err(format!(
            "Failed to check remote directory existence for {}: {}",
            path,
            output.combined().trim()
        )
        .into());
    }
    Ok(output.stdout.trim() == "yes")
}

pub(super) async fn connect_and_prepare_remote_release_dir(
    ssh: &mut SshClient,
    release_dir: &str,
    shared_dir: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    ssh.connect().await?;
    prepare_remote_release_dir(ssh, release_dir, shared_dir).await
}

/// Prepare the remote release directory on an already-connected SSH session.
pub(super) async fn prepare_remote_release_dir(
    ssh: &SshClient,
    release_dir: &str,
    shared_dir: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let release_dir_preexisted = remote_directory_exists(ssh, release_dir).await?;
    if !release_dir_preexisted {
        ssh.exec_checked(&format!(
            "mkdir -p {} {}",
            shell_single_quote(release_dir),
            shell_single_quote(shared_dir)
        ))
        .await?;
    }

    Ok(release_dir_preexisted)
}

pub(super) async fn run_deploy_step<T, E, Fut>(
    loading: &str,
    success: &str,
    use_spinner: bool,
    work: Fut,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
where
    Fut: Future<Output = Result<T, E>> + Send,
    T: Send,
    E: Send + std::fmt::Display + Into<Box<dyn std::error::Error + Send + Sync>>,
{
    if use_spinner {
        let error_label = format!("{} failed", loading.trim_end_matches('…'));
        output::with_spinner_async_err(loading, success, &error_label, work)
            .await
            .map_err(Into::into)
    } else {
        tracing::debug!("{}", loading);
        work.await.map_err(Into::into)
    }
}

pub(super) async fn run_task_tree_deploy_step<T, E, Fut>(
    task_tree: &DeployTaskTreeController,
    server_name: &str,
    step: &str,
    work: Fut,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
where
    Fut: Future<Output = Result<T, E>> + Send,
    T: Send,
    E: Send + std::fmt::Display + Into<Box<dyn std::error::Error + Send + Sync>>,
{
    run_task_tree_deploy_step_with_detail(task_tree, server_name, step, None, work).await
}

async fn run_task_tree_deploy_step_with_detail<T, E, Fut>(
    task_tree: &DeployTaskTreeController,
    server_name: &str,
    step: &str,
    success_detail: Option<String>,
    work: Fut,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
where
    Fut: Future<Output = Result<T, E>> + Send,
    T: Send,
    E: Send + std::fmt::Display + Into<Box<dyn std::error::Error + Send + Sync>>,
{
    run_task_tree_deploy_step_with_detail_and_error_cleanup(
        task_tree,
        server_name,
        step,
        success_detail,
        work,
        || async {},
    )
    .await
}

pub(super) async fn run_task_tree_deploy_step_with_detail_and_error_cleanup<
    T,
    E,
    Fut,
    Cleanup,
    CleanupFut,
>(
    task_tree: &DeployTaskTreeController,
    server_name: &str,
    step: &str,
    success_detail: Option<String>,
    work: Fut,
    cleanup_on_error: Cleanup,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
where
    Fut: Future<Output = Result<T, E>> + Send,
    T: Send,
    E: Send + std::fmt::Display + Into<Box<dyn std::error::Error + Send + Sync>>,
    Cleanup: FnOnce() -> CleanupFut + Send,
    CleanupFut: Future<Output = ()> + Send,
{
    task_tree.mark_deploy_step_running(server_name, step);
    match work.await {
        Ok(value) => {
            let success_label = match step {
                "connecting" => "Preflight",
                "uploading" => "Uploaded",
                "preparing" => "Prepared",
                "starting" => "Started",
                _ => step,
            };
            task_tree.rename_deploy_step(server_name, step, success_label);
            task_tree.succeed_deploy_step(server_name, step, success_detail);
            Ok(value)
        }
        Err(error) => {
            let message = error.to_string();
            cleanup_on_error().await;
            task_tree.fail_deploy_step(server_name, step, message.clone());
            let failed_label = match step {
                "connecting" => "Preflight failed",
                "uploading" => "Upload failed",
                "preparing" => "Prepare failed",
                "starting" => "Start failed",
                _ => step,
            };
            task_tree.rename_deploy_step(server_name, step, failed_label);
            task_tree.fail_deploy_target_without_detail(server_name);
            task_tree.cancel_pending_deploy_children(server_name, "cancelled");
            Err(error.into())
        }
    }
}

pub(super) fn remote_release_archive_path(release_dir: &str) -> String {
    format!("{release_dir}/artifacts.tar.zst")
}

pub(super) fn build_remote_extract_archive_command(
    release_dir: &str,
    remote_archive: &str,
) -> String {
    format!(
        "tako-server --extract-zstd-archive {} --extract-dest {} && rm -f {}",
        shell_single_quote(remote_archive),
        shell_single_quote(release_dir),
        shell_single_quote(remote_archive)
    )
}

async fn connect_for_deploy(
    config: &DeployConfig,
    server_name: &str,
    server: &crate::config::ServerEntry,
    release_dir: &str,
    use_spinner: bool,
    task_tree: Option<&DeployTaskTreeController>,
    preconnected_ssh: Option<SshClient>,
) -> Result<(SshClient, bool), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(ssh) = preconnected_ssh {
        let preexisted = prepare_remote_release_dir(&ssh, release_dir, &config.shared_dir())
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e })?;
        return Ok((ssh, preexisted));
    }

    let ssh_config = SshConfig::from_server(&server.host, server.port);
    let mut ssh = SshClient::new(ssh_config);
    let preexisted = if let Some(task_tree) = task_tree {
        run_task_tree_deploy_step(
            task_tree,
            server_name,
            "connecting",
            connect_and_prepare_remote_release_dir(&mut ssh, release_dir, &config.shared_dir()),
        )
        .await?
    } else {
        run_deploy_step(
            "Preflight",
            "Preflight",
            use_spinner,
            connect_and_prepare_remote_release_dir(&mut ssh, release_dir, &config.shared_dir()),
        )
        .await?
    };
    Ok((ssh, preexisted))
}

#[allow(clippy::too_many_arguments)]
async fn upload_release_artifact(
    ssh: &SshClient,
    server_name: &str,
    archive_path: &Path,
    remote_archive: &str,
    archive_size_bytes: u64,
    release_dir_preexisted: bool,
    use_spinner: bool,
    task_tree: Option<&DeployTaskTreeController>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if release_dir_preexisted {
        tracing::debug!("Release dir already exists, skipping upload");
        if let Some(task_tree) = task_tree {
            task_tree.skip_deploy_step(server_name, "uploading", "cached");
        }
        return Ok(());
    }

    let upload_timer = output::timed(&format!(
        "Upload artifact ({})",
        format_size(archive_size_bytes)
    ));
    if let Some(task_tree) = task_tree {
        let total_size = archive_size_bytes;
        let task_tree_for_progress = task_tree.clone();
        let server_name_for_progress = server_name.to_string();
        let upload_started_at = Instant::now();
        let show_progress = archive_size_bytes >= PROGRESS_BAR_MIN_BYTES;
        run_task_tree_deploy_step_with_detail(task_tree, server_name, "uploading", None, async {
            let callback: Option<Box<dyn Fn(u64, u64) + Send>> = if show_progress {
                Some(Box::new(move |done, _total| {
                    let fraction = if total_size > 0 {
                        done as f64 / total_size as f64
                    } else {
                        0.0
                    };
                    task_tree_for_progress.update_deploy_step_progress(
                        &server_name_for_progress,
                        "uploading",
                        output::format_transfer_compact_detail(
                            done,
                            total_size,
                            upload_started_at.elapsed(),
                        ),
                        fraction,
                    );
                }))
            } else {
                None
            };
            ssh.upload_with_progress(archive_path, remote_archive, callback)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
        })
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format_deploy_step_failure("Uploading", &e.to_string()).into()
        })?;
    } else {
        let upload_result: Result<(), Box<dyn std::error::Error + Send + Sync>> = if use_spinner {
            let progress = std::sync::Arc::new(output::TransferProgress::new(
                "Uploading",
                "Uploaded",
                archive_size_bytes,
            ));
            let progress_update = progress.clone();
            ssh.upload_with_progress(
                archive_path,
                remote_archive,
                Some(Box::new(move |done, _total| {
                    progress_update.set_position(done)
                })),
            )
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
            progress.finish();
            Ok(())
        } else {
            ssh.upload(archive_path, remote_archive)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
        };
        upload_result.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format_deploy_step_failure("Uploading", &e.to_string()).into()
        })?;
    }
    drop(upload_timer);
    Ok(())
}

async fn extract_and_prepare_release(
    config: &DeployConfig,
    ssh: &SshClient,
    release_dir: &str,
    remote_archive: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _t = output::timed("Extract and configure release");
    let extract_cmd = build_remote_extract_archive_command(release_dir, remote_archive);
    let shared = shell_single_quote(&config.shared_dir());
    let rel = shell_single_quote(release_dir);
    let shared_link_cmd = format!(
        "mkdir -p {}/logs && ln -sfn {}/logs {}/logs",
        shared, shared, rel
    );
    let combined_cmd = format!("{} && {}", extract_cmd, shared_link_cmd);
    ssh.exec_checked(&combined_cmd).await?;

    let prepare_cmd = Command::PrepareRelease {
        app: config.app_name.clone(),
        path: release_dir.to_string(),
    };
    let json = serde_json::to_string(&prepare_cmd)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
    let response = ssh
        .tako_command(&json)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
    if deploy_response_has_error(&response) {
        return Err(extract_server_error_message(&response).into());
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn prepare_uploaded_release(
    config: &DeployConfig,
    ssh: &SshClient,
    server_name: &str,
    release_dir: &str,
    remote_archive: &str,
    release_dir_preexisted: bool,
    use_spinner: bool,
    task_tree: Option<&DeployTaskTreeController>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if release_dir_preexisted {
        if let Some(task_tree) = task_tree {
            task_tree.skip_deploy_step(server_name, "preparing", "skipped");
        }
        return Ok(());
    }

    if let Some(task_tree) = task_tree {
        run_task_tree_deploy_step(task_tree, server_name, "preparing", async {
            extract_and_prepare_release(config, ssh, release_dir, remote_archive).await
        })
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format_deploy_step_failure("Preparing", &e.to_string()).into()
        })?;
    } else {
        run_deploy_step("Preparing…", "Prepared", use_spinner, async {
            extract_and_prepare_release(config, ssh, release_dir, remote_archive).await
        })
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format_deploy_step_failure("Preparing", &e.to_string()).into()
        })?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_release_command_step(
    config: &DeployConfig,
    ssh: &SshClient,
    server_name: &str,
    release_dir: &str,
    release_dir_preexisted: bool,
    task_tree: Option<&DeployTaskTreeController>,
    release_tx: Option<&ReleaseCommandSender>,
    release_rx: Option<ReleaseCommandReceiver>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if config.release_command.is_none() {
        return Ok(());
    }

    let is_leader = server_name == config.leader_server;

    if let Some(task_tree) = task_tree {
        task_tree.add_release_step(server_name, is_leader);
    }

    if is_leader {
        if let Some(task_tree) = task_tree {
            task_tree.mark_release_step_running(server_name);
        }
        let cmd = config
            .release_command_payload(release_dir)
            .expect("release command is present");
        let json = serde_json::to_string(&cmd)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
        let response_text = ssh
            .tako_command(&json)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

        if deploy_response_has_error(&response_text) {
            let msg = extract_server_error_message(&response_text);
            if let Some(sender) = release_tx {
                let _ = sender.send(Some(Err(msg.clone())));
            }
            if let Some(task_tree) = task_tree {
                task_tree.fail_release_step(server_name, msg.clone());
            }
            if !release_dir_preexisted {
                let _ = cleanup_partial_release(ssh, release_dir).await;
            }
            return Err(format_deploy_step_failure("Release command", &msg).into());
        }

        if let Some(task_tree) = task_tree {
            task_tree.succeed_release_step(server_name, None);
        }
        if let Some(sender) = release_tx {
            let _ = sender.send(Some(Ok(())));
        }
        return Ok(());
    }

    let mut rx = release_rx.expect("followers must hold a receiver");
    loop {
        let current = rx.borrow().clone();
        if let Some(result) = current {
            match result {
                Ok(()) => {
                    if let Some(task_tree) = task_tree {
                        task_tree.succeed_release_step(server_name, None);
                    }
                    return Ok(());
                }
                Err(msg) => {
                    if let Some(task_tree) = task_tree {
                        task_tree.cancel_release_step(server_name, "leader failed");
                    }
                    if !release_dir_preexisted {
                        let _ = cleanup_partial_release(ssh, release_dir).await;
                    }
                    return Err(format_deploy_step_failure("Release command (leader)", &msg).into());
                }
            }
        }
        if rx.changed().await.is_err() {
            if let Some(task_tree) = task_tree {
                task_tree.cancel_release_step(server_name, "release cancelled");
            }
            return Err("Release command cancelled".into());
        }
    }
}

async fn finish_deploy_housekeeping(
    config: &DeployConfig,
    ssh: &SshClient,
    release_dir: &str,
    server_name: &str,
    task_tree: Option<&DeployTaskTreeController>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ssh.symlink(release_dir, &config.current_link())
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

    let releases_dir = format!("{}/releases", config.remote_base);
    let cleanup_cmd = format!(
        "find {} -mindepth 1 -maxdepth 1 -type d -mtime +30 -exec rm -rf {{}} \\;",
        shell_single_quote(&releases_dir)
    );
    if let Err(e) = ssh.exec(&cleanup_cmd).await {
        tracing::warn!("Failed to clean up old releases: {e}");
    }

    if let Some(task_tree) = task_tree {
        task_tree.succeed_deploy_target(server_name, None);
    }

    Ok(())
}

/// Deploy to a single server.
///
/// If `preconnected_ssh` is provided (from the preflight phase), the existing
/// connection is reused and the "Preflight" task-tree step is skipped (it was
/// already marked complete during preflight).  Otherwise a fresh SSH connection
/// is established here.
///
/// `release_tx` is `Some` only for the leader server when a release command is
/// configured. `release_rx` is `Some` only for follower servers in that case.
#[allow(clippy::too_many_arguments)]
pub(super) async fn deploy_to_server(
    config: &DeployConfig,
    server_name: &str,
    server: &crate::config::ServerEntry,
    archive_path: &Path,
    target_label: &str,
    use_spinner: bool,
    task_tree: Option<DeployTaskTreeController>,
    preconnected_ssh: Option<SshClient>,
    release_tx: Option<ReleaseCommandSender>,
    release_rx: Option<ReleaseCommandReceiver>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _server_deploy_timer =
        output::timed(&format!("Server deploy ({target_label}:{})", server.port));
    let release_dir = config.release_dir();

    let (mut ssh, release_dir_preexisted) = connect_for_deploy(
        config,
        server_name,
        server,
        &release_dir,
        use_spinner,
        task_tree.as_ref(),
        preconnected_ssh,
    )
    .await?;
    let archive_size_bytes = std::fs::metadata(archive_path)?.len();
    tracing::debug!("Archive size: {}", format_size(archive_size_bytes));
    let mut cleaned_partial_release = false;

    let result = async {
        let remote_archive = remote_release_archive_path(&release_dir);
        upload_release_artifact(
            &ssh,
            server_name,
            archive_path,
            &remote_archive,
            archive_size_bytes,
            release_dir_preexisted,
            use_spinner,
            task_tree.as_ref(),
        )
        .await?;

        prepare_uploaded_release(
            config,
            &ssh,
            server_name,
            &release_dir,
            &remote_archive,
            release_dir_preexisted,
            use_spinner,
            task_tree.as_ref(),
        )
        .await?;

        run_release_command_step(
            config,
            &ssh,
            server_name,
            &release_dir,
            release_dir_preexisted,
            task_tree.as_ref(),
            release_tx.as_ref(),
            release_rx,
        )
        .await?;

        tracing::debug!(
            "{}",
            super::format::format_deploy_main_message(
                &config.main,
                target_label,
                config.use_unified_target_process,
            )
        );

        // Resolve secrets before the starting step to keep it fast.
        let deploy_secrets = match query_remote_secrets_hash(&ssh, &config.app_name).await {
            Some(remote_hash) if remote_hash == config.secrets_hash => None,
            _ => Some(config.secrets.clone()),
        };

        let start_result = if let Some(task_tree) = &task_tree {
            run_task_tree_deploy_step_with_detail_and_error_cleanup(
                task_tree,
                server_name,
                "starting",
                None,
                async {
                    let cmd = Command::Deploy {
                        app: config.app_name.clone(),
                        version: config.version.clone(),
                        path: release_dir.clone(),
                        routes: config.routes.clone(),
                        secrets: deploy_secrets,
                    };
                    let json = serde_json::to_string(&cmd)
                        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
                    let response = ssh
                        .tako_command(&json)
                        .await
                        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

                    if deploy_response_has_error(&response) {
                        return Err(extract_server_error_message(&response).into());
                    }

                    Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                },
                || {
                    cleaned_partial_release = true;
                    async {
                        if !release_dir_preexisted
                            && let Err(e) = cleanup_partial_release(&ssh, &release_dir).await
                        {
                            tracing::warn!(
                                "Failed to cleanup partial release directory {release_dir}: {e}"
                            );
                        }
                    }
                },
            )
            .await
        } else {
            run_deploy_step("Starting…", "Started", use_spinner, async {
                let cmd = Command::Deploy {
                    app: config.app_name.clone(),
                    version: config.version.clone(),
                    path: release_dir.clone(),
                    routes: config.routes.clone(),
                    secrets: deploy_secrets,
                };
                let json = serde_json::to_string(&cmd)
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
                let response = ssh
                    .tako_command(&json)
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

                if deploy_response_has_error(&response) {
                    return Err(extract_server_error_message(&response).into());
                }

                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            })
            .await
        };
        start_result.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format_deploy_step_failure("Starting", &e.to_string()).into()
        })?;

        finish_deploy_housekeeping(config, &ssh, &release_dir, server_name, task_tree.as_ref())
            .await?;

        Ok(())
    }
    .await;

    if result.is_err()
        && !release_dir_preexisted
        && !cleaned_partial_release
        && let Err(e) = cleanup_partial_release(&ssh, &release_dir).await
    {
        tracing::warn!("Failed to cleanup partial release directory {release_dir}: {e}");
    }

    // Always disconnect (best-effort).
    let _ = ssh.disconnect().await;

    result
}

/// Query the remote server for the SHA-256 hash of an app's current secrets.
/// Returns `None` if the query fails.
pub(super) async fn query_remote_secrets_hash(ssh: &SshClient, app_name: &str) -> Option<String> {
    let cmd = Command::GetSecretsHash {
        app: app_name.to_string(),
    };
    let json = serde_json::to_string(&cmd).ok()?;
    let response_str = ssh.tako_command(&json).await.ok()?;
    let value: serde_json::Value = serde_json::from_str(&response_str).ok()?;
    if value.get("status").and_then(|s| s.as_str()) != Some("ok") {
        return None;
    }
    value
        .get("data")
        .and_then(|d| d.get("hash"))
        .and_then(|h| h.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests;

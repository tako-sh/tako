use std::future::Future;
use std::path::Path;

use crate::management_http::{self, ManagementClient};
use crate::output;
use tako_core::{Command, ReleaseUploadPlan, Response};

use super::DeployConfig;
use super::format::{format_deploy_step_failure, format_size};
use super::task_tree::DeployTaskTreeController;

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

fn release_response_result(response: Response) -> Result<serde_json::Value, String> {
    match response {
        Response::Ok { data } => Ok(data),
        Response::Error { message } => Err(message
            .strip_prefix("Deploy failed: ")
            .unwrap_or(&message)
            .to_string()),
    }
}

async fn prepare_release_upload_plan(
    client: &mut ManagementClient,
    config: &DeployConfig,
) -> Result<ReleaseUploadPlan, management_http::ManagementError> {
    let response = client
        .send(&Command::PrepareReleaseUpload {
            app: config.app_name.clone(),
            version: config.version.clone(),
        })
        .await?;
    management_http::parse_ok_data(response, "release upload plan")
}

#[allow(clippy::too_many_arguments)]
async fn upload_release_artifact(
    client: &mut ManagementClient,
    config: &DeployConfig,
    server_name: &str,
    archive_path: &Path,
    archive_size_bytes: u64,
    upload_plan: ReleaseUploadPlan,
    use_spinner: bool,
    task_tree: Option<&DeployTaskTreeController>,
) -> Result<ReleaseUploadPlan, Box<dyn std::error::Error + Send + Sync>> {
    if !upload_plan.upload_required {
        tracing::debug!("Release already exists, skipping artifact upload");
        if let Some(task_tree) = task_tree {
            task_tree.skip_deploy_step(server_name, "uploading", "cached");
        }
        return Ok(upload_plan);
    }

    let upload_timer = output::timed(&format!(
        "Upload artifact ({})",
        format_size(archive_size_bytes)
    ));
    let upload_detail = Some(format_size(archive_size_bytes));
    let result = if let Some(task_tree) = task_tree {
        run_task_tree_deploy_step_with_detail(
            task_tree,
            server_name,
            "uploading",
            upload_detail,
            async {
                let response = client
                    .upload_release_artifact(&config.app_name, &config.version, archive_path)
                    .await?;
                management_http::parse_ok_data::<ReleaseUploadPlan>(
                    response,
                    "release artifact upload",
                )
            },
        )
        .await
    } else {
        run_deploy_step("Uploading…", "Uploaded", use_spinner, async {
            let response = client
                .upload_release_artifact(&config.app_name, &config.version, archive_path)
                .await?;
            management_http::parse_ok_data::<ReleaseUploadPlan>(response, "release artifact upload")
        })
        .await
    };
    drop(upload_timer);

    result.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
        format_deploy_step_failure("Uploading", &e.to_string()).into()
    })
}

async fn prepare_release(
    config: &DeployConfig,
    client: &mut ManagementClient,
    release_dir: &str,
) -> Result<(), management_http::ManagementError> {
    let response = client
        .send(&Command::PrepareRelease {
            app: config.app_name.clone(),
            path: release_dir.to_string(),
        })
        .await?;
    release_response_result(response).map_err(management_http::ManagementError::Message)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn prepare_uploaded_release(
    config: &DeployConfig,
    client: &mut ManagementClient,
    server_name: &str,
    release_dir: &str,
    release_preexisted: bool,
    use_spinner: bool,
    task_tree: Option<&DeployTaskTreeController>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if release_preexisted {
        if let Some(task_tree) = task_tree {
            task_tree.skip_deploy_step(server_name, "preparing", "skipped");
        }
        return Ok(());
    }

    let result = if let Some(task_tree) = task_tree {
        run_task_tree_deploy_step(task_tree, server_name, "preparing", async {
            prepare_release(config, client, release_dir).await
        })
        .await
    } else {
        run_deploy_step("Preparing…", "Prepared", use_spinner, async {
            prepare_release(config, client, release_dir).await
        })
        .await
    };

    result.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
        format_deploy_step_failure("Preparing", &e.to_string()).into()
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_release_command_step(
    config: &DeployConfig,
    client: &mut ManagementClient,
    server_name: &str,
    release_dir: &str,
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
        let response = client
            .send(&cmd)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

        if let Err(msg) = release_response_result(response) {
            if let Some(sender) = release_tx {
                let _ = sender.send(Some(Err(msg.clone())));
            }
            if let Some(task_tree) = task_tree {
                task_tree.fail_release_step(server_name, msg.clone());
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

async fn send_deploy_command(
    config: &DeployConfig,
    client: &mut ManagementClient,
    release_dir: &str,
    deploy_secrets: Option<std::collections::HashMap<String, String>>,
) -> Result<(), management_http::ManagementError> {
    let response = client
        .send(&Command::Deploy {
            app: config.app_name.clone(),
            version: config.version.clone(),
            path: release_dir.to_string(),
            routes: config.routes.clone(),
            source_ip: config.source_ip,
            secrets: deploy_secrets,
            storages: Some(config.storages.clone()),
            ssl: config.ssl.clone(),
        })
        .await?;
    release_response_result(response).map_err(management_http::ManagementError::Message)?;
    Ok(())
}

async fn cleanup_release_on_host(host: &str, app: &str, version: &str) -> Result<(), String> {
    let mut client = ManagementClient::new(host)
        .await
        .map_err(|error| error.to_string())?;
    cleanup_release_with_client(&mut client, app, version).await
}

async fn cleanup_release_with_client(
    client: &mut ManagementClient,
    app: &str,
    version: &str,
) -> Result<(), String> {
    let response = client
        .send(&Command::CleanupRelease {
            app: app.to_string(),
            version: version.to_string(),
        })
        .await
        .map_err(|error| error.to_string())?;
    release_response_result(response)?;
    Ok(())
}

async fn finish_deploy_housekeeping(
    config: &DeployConfig,
    client: &mut ManagementClient,
    server_name: &str,
    task_tree: Option<&DeployTaskTreeController>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let response = client
        .send(&Command::FinalizeRelease {
            app: config.app_name.clone(),
            version: config.version.clone(),
        })
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
    release_response_result(response)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    if let Some(task_tree) = task_tree {
        task_tree.succeed_deploy_target(server_name, None);
    }

    Ok(())
}

/// Deploy to a single server over signed HTTP management.
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
    release_tx: Option<ReleaseCommandSender>,
    release_rx: Option<ReleaseCommandReceiver>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _server_deploy_timer =
        output::timed(&format!("Server deploy ({target_label}:{})", server.host));
    let mut client = ManagementClient::new(&server.host)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
    let archive_size_bytes = std::fs::metadata(archive_path)?.len();
    tracing::debug!("Archive size: {}", format_size(archive_size_bytes));

    let upload_plan = prepare_release_upload_plan(&mut client, config)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
    let release_preexisted = !upload_plan.upload_required;
    let release_dir = upload_plan.path.clone();

    let result = async {
        let uploaded_plan = upload_release_artifact(
            &mut client,
            config,
            server_name,
            archive_path,
            archive_size_bytes,
            upload_plan,
            use_spinner,
            task_tree.as_ref(),
        )
        .await?;
        let release_dir = uploaded_plan.path;

        prepare_uploaded_release(
            config,
            &mut client,
            server_name,
            &release_dir,
            release_preexisted,
            use_spinner,
            task_tree.as_ref(),
        )
        .await?;

        run_release_command_step(
            config,
            &mut client,
            server_name,
            &release_dir,
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
        let deploy_secrets = match query_remote_secrets_hash(&mut client, &config.app_name).await {
            Some(remote_hash) if remote_hash == config.secrets_hash => None,
            _ => Some(config.secrets.clone()),
        };

        let start_result = if let Some(task_tree) = &task_tree {
            let cleanup_host = server.host.clone();
            let cleanup_app = config.app_name.clone();
            let cleanup_version = config.version.clone();
            run_task_tree_deploy_step_with_detail_and_error_cleanup(
                task_tree,
                server_name,
                "starting",
                None,
                async {
                    send_deploy_command(config, &mut client, &release_dir, deploy_secrets).await
                },
                move || async move {
                    if !release_preexisted
                        && let Err(error) =
                            cleanup_release_on_host(&cleanup_host, &cleanup_app, &cleanup_version)
                                .await
                    {
                        tracing::warn!("Failed to cleanup partial release: {error}");
                    }
                },
            )
            .await
        } else {
            run_deploy_step("Starting…", "Started", use_spinner, async {
                send_deploy_command(config, &mut client, &release_dir, deploy_secrets).await
            })
            .await
        };
        start_result.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format_deploy_step_failure("Starting", &e.to_string()).into()
        })?;

        finish_deploy_housekeeping(config, &mut client, server_name, task_tree.as_ref()).await?;

        Ok(())
    }
    .await;

    if result.is_err()
        && !release_preexisted
        && let Err(error) =
            cleanup_release_with_client(&mut client, &config.app_name, &config.version).await
    {
        tracing::warn!("Failed to cleanup partial release {release_dir}: {error}");
    }

    result
}

/// Query the remote server for the SHA-256 hash of an app's current secrets.
/// Returns `None` if the query fails.
pub(super) async fn query_remote_secrets_hash(
    client: &mut ManagementClient,
    app_name: &str,
) -> Option<String> {
    let response = client
        .send(&Command::GetSecretsHash {
            app: app_name.to_string(),
        })
        .await
        .ok()?;
    let data = release_response_result(response).ok()?;
    data.get("hash")
        .and_then(|hash| hash.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests;

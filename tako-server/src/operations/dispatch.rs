use crate::release::{validate_app_name, validate_release_version};
use crate::socket::{Command, Response};
use tako_core::{HelloResponse, PROTOCOL_VERSION};

impl crate::ServerState {
    /// Handle a command from the management socket
    pub async fn handle_command(&self, cmd: Command) -> Response {
        match cmd {
            Command::Hello { protocol_version } => {
                let data = HelloResponse {
                    protocol_version: PROTOCOL_VERSION,
                    server_version: crate::server_version().to_string(),
                    capabilities: vec![
                        "on_demand_cold_start".to_string(),
                        "idle_scale_to_zero".to_string(),
                        "scale".to_string(),
                        "upgrade_mode_control".to_string(),
                        "server_runtime_info".to_string(),
                        "release_history".to_string(),
                        "rollback".to_string(),
                        "management_http_uploads".to_string(),
                        "backups".to_string(),
                    ],
                    server_identity: self.runtime_config().server_identity.clone(),
                };

                if protocol_version != PROTOCOL_VERSION {
                    return Response::error(format!(
                        "Protocol version mismatch: client={} server={}",
                        protocol_version, PROTOCOL_VERSION
                    ));
                }

                Response::ok(data)
            }
            Command::PrepareRelease { app, path } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                self.prepare_release(&app, &path).await
            }
            Command::PrepareReleaseUpload { app, version } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Err(msg) = validate_release_version(&version) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("prepare-release-upload").await
                {
                    return resp;
                }
                self.prepare_release_upload(&app, &version).await
            }
            Command::CleanupRelease { app, version } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Err(msg) = validate_release_version(&version) {
                    return Response::error(msg);
                }
                self.cleanup_release(&app, &version).await
            }
            Command::FinalizeRelease { app, version } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Err(msg) = validate_release_version(&version) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("finalize-release").await {
                    return resp;
                }
                self.finalize_release(&app, &version).await
            }
            Command::CheckDeploySpace { min_free_bytes } => {
                self.check_deploy_space(min_free_bytes).await
            }
            Command::RunRelease {
                app,
                version,
                path,
                command_line,
                vars,
                secrets,
            } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Err(msg) = validate_release_version(&version) {
                    return Response::error(msg);
                }
                self.run_release(&app, &version, &path, &command_line, vars, secrets)
                    .await
            }
            Command::Deploy {
                app,
                version,
                path,
                routes,
                source_ip,
                secrets,
                storages,
                ssl,
                backup,
            } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Err(msg) = validate_release_version(&version) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("deploy").await {
                    return resp;
                }
                self.deploy_app(
                    &app,
                    &version,
                    &path,
                    routes,
                    source_ip,
                    secrets,
                    storages,
                    ssl,
                    backup.map(|backup| *backup),
                )
                .await
            }
            Command::BackupNow { app, backup } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("backup-now").await {
                    return resp;
                }
                if let Some(backup) = backup.as_deref()
                    && let Err(error) = self.state_store.set_backup(&app, Some(backup))
                {
                    return Response::error(format!("Failed to store backup config: {error}"));
                }
                self.backup_now(&app).await
            }
            Command::ListBackups { app } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                self.list_backups(&app).await
            }
            Command::BackupStatus { app } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                self.backup_status(&app).await
            }
            Command::BackupDownloadUrl { app, backup_id } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                self.backup_download_url(&app, &backup_id).await
            }
            Command::RestoreBackup { app, backup_id } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("restore-backup").await {
                    return resp;
                }
                self.restore_backup(&app, &backup_id).await
            }
            Command::Scale { app, instances } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("scale").await {
                    return resp;
                }
                self.scale_app(&app, instances).await
            }
            Command::Stop { app } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("stop").await {
                    return resp;
                }
                self.stop_app(&app).await
            }
            Command::Delete { app } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("delete").await {
                    return resp;
                }
                self.delete_app(&app).await
            }
            Command::Status { app } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                self.get_status(&app).await
            }
            Command::List => self.list_apps().await,
            Command::ListReleases { app } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                self.list_releases(&app).await
            }
            Command::Routes => self.list_routes().await,
            Command::Rollback { app, version } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Err(msg) = validate_release_version(&version) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("rollback").await {
                    return resp;
                }
                self.rollback_app(&app, &version).await
            }
            Command::UpdateSecrets { app, secrets } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("update-secrets").await {
                    return resp;
                }
                self.update_secrets(&app, secrets).await
            }
            Command::GetSecretsHash { app } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                let secrets = self.state_store.get_secrets(&app).unwrap_or_default();
                let hash = tako_core::compute_secrets_hash(&secrets);
                Response::ok(serde_json::json!({ "hash": hash }))
            }
            Command::ServerInfo => Response::ok(self.runtime_info().await),
            Command::EnterUpgrading { owner } => match self.try_enter_upgrading(&owner).await {
                Ok(true) => Response::ok(serde_json::json!({
                    "status": "upgrading",
                    "owner": owner
                })),
                Ok(false) => {
                    let owner_msg = self
                        .state_store
                        .upgrade_lock_owner()
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "unknown".to_string());
                    Response::error(format!(
                        "Server is already upgrading (owner: {}).",
                        owner_msg
                    ))
                }
                Err(e) => Response::error(format!("Failed to enter upgrading mode: {}", e)),
            },
            Command::ExitUpgrading { owner } => match self.exit_upgrading(&owner).await {
                Ok(true) => Response::ok(serde_json::json!({
                    "status": "normal",
                    "owner": owner
                })),
                Ok(false) => Response::error(
                    "Failed to exit upgrading mode: owner does not hold the upgrade lock."
                        .to_string(),
                ),
                Err(e) => Response::error(format!("Failed to exit upgrading mode: {}", e)),
            },
            Command::InjectChallengeToken {
                token,
                key_authorization,
            } => {
                let mut tokens = self.challenge_tokens.write();
                tokens.insert(token.clone(), key_authorization);
                Response::ok(serde_json::json!({
                    "status": "injected",
                    "token": token
                }))
            }
            Command::EnqueueRun { .. }
            | Command::RegisterSchedules { .. }
            | Command::ClaimRun { .. }
            | Command::HeartbeatRun { .. }
            | Command::SaveStep { .. }
            | Command::CompleteRun { .. }
            | Command::CancelRun { .. }
            | Command::FailRun { .. }
            | Command::DeferRun { .. }
            | Command::WaitForEvent { .. }
            | Command::Signal { .. }
            | Command::ChannelPublish { .. } => Response::error(
                "workflow/channel commands must be sent over the internal socket, not the management socket"
                    .to_string(),
            ),
        }
    }
}

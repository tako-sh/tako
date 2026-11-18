use crate::release::{validate_app_name, validate_release_version};
use crate::socket::{Command, Response};
use std::collections::HashMap;
use tako_core::{HelloResponse, PROTOCOL_VERSION};

/// Commands that only inspect state. Management dispatch runs them without the
/// management lock, so probes and status reads keep answering within the CLI's
/// short RPC timeout while a deployment, drain, or resume holds the lock.
pub(crate) fn command_is_read_only(command: &Command) -> bool {
    matches!(
        command,
        Command::Hello { .. }
            | Command::ServerInfo
            | Command::Status { .. }
            | Command::List
            | Command::ListReleases { .. }
            | Command::Routes
            | Command::GetSecretsHash { .. }
            | Command::ListBackups { .. }
            | Command::BackupStatus { .. }
            | Command::BackupDownloadUrl { .. }
            | Command::CheckDeploySpace { .. }
            | Command::ListWorkflowRuns { .. }
            | Command::GetWorkflowRun { .. }
            // Database operations serialize per database and can run for
            // minutes (snapshots, transfers); they must not hold up deploys.
            | Command::DatabaseStatus { .. }
            | Command::ListDatabaseSnapshots { .. }
            | Command::SnapshotDatabase { .. }
            | Command::RestoreDatabase { .. }
            | Command::QueryDatabase { .. }
            | Command::MigrateDatabase { .. }
            | Command::ResetDatabase { .. }
            | Command::BeginDatabaseImport { .. }
            | Command::DatabaseImportChunk { .. }
            | Command::FinishDatabaseImport { .. }
            | Command::BeginDatabaseExport { .. }
            | Command::DatabaseExportChunk { .. }
            | Command::FinishDatabaseExport { .. }
    )
}

impl crate::ServerState {
    /// Handle a command from the management socket
    pub async fn handle_command(&self, cmd: Command) -> Response {
        self.handle_command_as(cmd, super::Caller::Full).await
    }

    /// Handle a command sent by `caller`.
    pub(crate) async fn handle_command_as(&self, cmd: Command, caller: super::Caller) -> Response {
        if self.is_draining() && super::drain::blocked_while_draining(&cmd) {
            return Response::error(
                "Server is drained; resume it before starting new application work.",
            );
        }
        match cmd {
            Command::DrainServer { timeout_seconds } => self.drain_server(timeout_seconds).await,
            Command::ResumeServer => self.resume_server().await,
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
                        "server_drain_resume".to_string(),
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
            Command::PrepareRelease {
                app,
                path,
                secrets_digest,
                force,
            } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                let secrets = match self.pinned_app_secrets(&app, secrets_digest.as_deref()).await {
                    Ok(secrets) => secrets,
                    Err(error) => return Response::error(error),
                };
                self.prepare_release(&app, &path, secrets, force).await
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
            Command::PrepareDeploy {
                app,
                version,
                path,
                routes,
                mut ssl,
                secrets_digest,
                force,
            } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Err(msg) = validate_release_version(&version) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("prepare-deploy").await {
                    return resp;
                }
                if let Err(error) = self.refuse_claimed_routes(&app, &routes, caller) {
                    return Response::error(error);
                }
                ssl.cloudflare_api_token = None;
                if let Some(digest) = secrets_digest.as_deref() {
                    match crate::secret_snapshot::resolve(&self.runtime.data_dir, &app, Some(digest), &[]).await {
                        Ok(resolved) => {
                            if let Err(error) = resolved.refuse_expired() {
                                return Response::error(error);
                            }
                            resolved.fill_ssl(&mut ssl)
                        }
                        Err(error) => return Response::error(error),
                    }
                }
                self.prepare_deploy(&app, &version, &path, routes, ssl, force)
                    .await
            }
            Command::CleanupPreparedDeploy { app, version } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Err(msg) = validate_release_version(&version) {
                    return Response::error(msg);
                }
                self.cleanup_prepared_deploy(&app, &version).await
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
                secrets_digest,
                databases,
                force,
            } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Err(msg) = validate_release_version(&version) {
                    return Response::error(msg);
                }
                let secrets = match self.pinned_app_secrets(&app, secrets_digest.as_deref()).await {
                    Ok(secrets) => secrets,
                    Err(error) => return Response::error(error),
                };
                self.run_release(super::releases::ReleaseRun {
                    app_name: &app,
                    version: &version,
                    path: &path,
                    command_line: &command_line,
                    vars,
                    secrets,
                    databases,
                    force,
                })
                .await
            }
            Command::Deploy {
                app,
                version,
                path,
                routes,
                source_ip,
                secrets_digest,
                mut storages,
                databases,
                mut ssl,
                mut backup,
                force,
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
                if let Err(error) = self.refuse_claimed_routes(&app, &routes, caller) {
                    return Response::error(error);
                }
                // Clients never send secret values. Fill them from the pinned
                // snapshot, or keep what this server last applied.
                strip_secret_parts(&mut storages, &mut ssl, &mut backup);
                let (secrets, runtime_credentials) = match secrets_digest.as_deref() {
                    Some(digest) => {
                        let resolved = match crate::secret_snapshot::resolve(
                            &self.runtime.data_dir,
                            &app,
                            Some(digest),
                            &[],
                        )
                        .await
                        {
                            Ok(resolved) => resolved,
                            Err(error) => return Response::error(error),
                        };
                        if let Err(error) = resolved.refuse_expired() {
                            return Response::error(error);
                        }
                        resolved.fill_ssl(&mut ssl);
                        if let Some(storages) = storages.as_mut()
                            && let Err(error) = resolved.fill_storages(storages)
                        {
                            return Response::error(error);
                        }
                        if let Some(backup) = backup.as_deref_mut()
                            && let Err(error) = resolved.fill_backup(backup)
                        {
                            return Response::error(error);
                        }
                        (Some(resolved.app.clone()), Some(resolved.runtime_credentials()))
                    }
                    None => {
                        if let Err(error) = self.keep_applied_secret_parts(&app, &mut storages, &mut backup) {
                            return Response::error(error);
                        }
                        (None, None)
                    }
                };
                self.deploy_app(super::deploy::DeployRequest {
                    app_name: &app,
                    version: &version,
                    path: &path,
                    routes,
                    source_ip,
                    secrets,
                    runtime_credentials,
                    storages,
                    databases,
                    ssl,
                    backup: super::deploy::BackupUpdate::Replace(backup),
                    force,
                })
                .await
            }
            Command::BackupNow {
                app,
                mut backup,
                secrets_digest,
            } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("backup-now").await {
                    return resp;
                }
                if let Some(binding) = backup.as_deref_mut() {
                    binding.storage.access_key_id = None;
                    binding.storage.secret_access_key = None;
                    binding.backup_keys.clear();
                    // Every server backs up with the snapshot the client pinned.
                    // Only the backup target's keys and the backup keys need
                    // to be current; expired app secrets don't block a backup.
                    let resolved = match crate::secret_snapshot::resolve(
                        &self.runtime.data_dir,
                        &app,
                        secrets_digest.as_deref(),
                        &[],
                    )
                    .await
                    {
                        Ok(resolved) => resolved,
                        Err(error) => return Response::error(error),
                    };
                    if let Err(error) = resolved.fill_backup(binding) {
                        return Response::error(error);
                    }
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
            Command::Scale { app, instances, service, reset } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("scale").await {
                    return resp;
                }
                self.scale_app(&app, instances, service.as_deref(), reset).await
            }
            Command::Start { app, service } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("start").await {
                    return resp;
                }
                self.start_app(&app, service.as_deref()).await
            }
            Command::Stop { app, service } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("stop").await {
                    return resp;
                }
                self.stop_app(&app, service.as_deref()).await
            }
            command @ (Command::CreateDatabase { .. }
            | Command::DeleteDatabase { .. }
            | Command::DatabaseStatus { .. }
            | Command::ConfigureDatabaseBackup { .. }
            | Command::GrantDatabases { .. }
            | Command::SnapshotDatabase { .. }
            | Command::ListDatabaseSnapshots { .. }
            | Command::RestoreDatabase { .. }
            | Command::QueryDatabase { .. }
            | Command::MigrateDatabase { .. }
            | Command::ResetDatabase { .. }
            | Command::BeginDatabaseImport { .. }
            | Command::DatabaseImportChunk { .. }
            | Command::FinishDatabaseImport { .. }
            | Command::BeginDatabaseExport { .. }
            | Command::DatabaseExportChunk { .. }
            | Command::FinishDatabaseExport { .. }) => {
                let mutating = !matches!(
                    command,
                    Command::DatabaseStatus { .. }
                        | Command::ListDatabaseSnapshots { .. }
                        | Command::BeginDatabaseExport { .. }
                        | Command::DatabaseExportChunk { .. }
                        | Command::FinishDatabaseExport { .. }
                );
                if mutating
                    && let Some(resp) = self.reject_mutating_when_upgrading("database").await
                {
                    return resp;
                }
                self.handle_database_command(command).await
            }
            Command::Delete { app } => {
                if let Some(message) = self.compose_child_delete_error(&app) {
                    return Response::error(message);
                }
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
                self.rollback_app_as(&app, &version, caller).await
            }
            Command::SecretsDigest { app, published } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                match crate::secret_snapshot::resolve(&self.runtime.data_dir, &app, None, &published)
                    .await
                {
                    Ok(resolved) => Response::ok(serde_json::json!({ "digest": resolved.digest })),
                    Err(error) => Response::error(error),
                }
            }
            Command::GetSecretsHash { app } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                let secrets = match self.state_store.get_secrets(&app) {
                    Ok(secrets) => secrets,
                    Err(error) => {
                        return Response::error(format!("Failed to read secrets: {error}"));
                    }
                };
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
            Command::ListWorkflowRuns {
                app,
                status,
                name,
                limit,
            } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                self.list_workflow_runs(&app, status, name, limit).await
            }
            Command::GetWorkflowRun { app, id } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                self.get_workflow_run(&app, &id).await
            }
            Command::RetryWorkflowRun { app, id } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("retry-workflow-run").await {
                    return resp;
                }
                self.retry_workflow_run(&app, &id).await
            }
            Command::CancelWorkflowRun { app, id } => {
                if let Err(msg) = validate_app_name(&app) {
                    return Response::error(msg);
                }
                if let Some(resp) = self.reject_mutating_when_upgrading("cancel-workflow-run").await {
                    return resp;
                }
                self.cancel_workflow_run(&app, &id).await
            }
            Command::EnqueueRun { .. }
            | Command::RegisterSchedules { .. }
            | Command::ClaimRun { .. }
            | Command::HeartbeatRun { .. }
            | Command::SaveStep { .. }
            | Command::GetRunResult { .. }
        | Command::CompleteRun { .. }
            | Command::CancelRun { .. }
            | Command::FailRun { .. }
            | Command::DeferRun { .. }
            | Command::WaitForEvent { .. }
            | Command::Signal { .. }
            | Command::NetworkCall { .. }
        | Command::ChannelPublish { .. }
            | Command::ChannelClose { .. } => Response::error(
                "workflow/channel commands must be sent over the internal socket, not the management socket"
                    .to_string(),
            ),
        }
    }
}

/// Drop any secret parts a client sent in bindings; the server fills them.
fn strip_secret_parts(
    storages: &mut Option<HashMap<String, tako_core::StorageBinding>>,
    ssl: &mut tako_core::SslBinding,
    backup: &mut Option<Box<tako_core::BackupBinding>>,
) {
    ssl.cloudflare_api_token = None;
    for binding in storages
        .iter_mut()
        .flat_map(|storages| storages.values_mut())
    {
        if binding.provider == tako_core::StorageProvider::S3 {
            binding.access_key_id = None;
            binding.secret_access_key = None;
        }
    }
    if let Some(backup) = backup.as_deref_mut() {
        backup.storage.access_key_id = None;
        backup.storage.secret_access_key = None;
        backup.backup_keys.clear();
    }
}

impl crate::ServerState {
    /// A route belongs to one app environment, so a staging deploy cannot
    /// take over production's domain.
    pub(super) fn refuse_claimed_routes(
        &self,
        app: &str,
        routes: &[String],
        caller: super::Caller,
    ) -> Result<(), String> {
        let table = self.routes.read();
        if let Some((route, owner)) = table.claimed_elsewhere(app, routes) {
            return Err(format!(
                "Route '{route}' belongs to {owner}. Remove it from {owner} before deploying it here."
            ));
        }
        // Routing prefers the most specific route, so an overlapping route
        // could take some of another app environment's requests.
        if caller == super::Caller::Deploy
            && let Some((route, other, owner)) = table.overlapping_elsewhere(app, routes)
        {
            return Err(format!(
                "Route '{route}' overlaps route '{other}' of {owner}. A deploy client can't add a route that shares requests with another app environment; deploy it with a full client."
            ));
        }
        Ok(())
    }

    /// App secrets of the pinned snapshot, or none without a pin.
    async fn pinned_app_secrets(
        &self,
        app: &str,
        digest: Option<&str>,
    ) -> Result<HashMap<String, String>, String> {
        match digest {
            Some(digest) => {
                let resolved =
                    crate::secret_snapshot::resolve(&self.runtime.data_dir, app, Some(digest), &[])
                        .await?;
                resolved.refuse_expired()?;
                Ok(resolved.app)
            }
            None => Ok(HashMap::new()),
        }
    }

    /// Without a pinned snapshot, bindings keep the access keys this server
    /// last applied for the same storage.
    fn keep_applied_secret_parts(
        &self,
        app: &str,
        storages: &mut Option<HashMap<String, tako_core::StorageBinding>>,
        backup: &mut Option<Box<tako_core::BackupBinding>>,
    ) -> Result<(), String> {
        let same = |a: &tako_core::StorageBinding, b: &tako_core::StorageBinding| {
            a.provider == b.provider
                && a.resource == b.resource
                && a.bucket == b.bucket
                && a.endpoint == b.endpoint
                && a.region == b.region
        };
        if let Some(storages) = storages.as_mut() {
            let applied = self
                .state_store
                .get_storages(app)
                .map_err(|error| format!("Failed to read storages: {error}"))?;
            for (name, binding) in storages.iter_mut() {
                if binding.provider != tako_core::StorageProvider::S3 {
                    continue;
                }
                let previous = applied
                    .get(name)
                    .filter(|previous| same(previous, binding))
                    .ok_or_else(|| {
                        format!("Storage binding '{name}' needs its access keys; deploy it with a secret snapshot.")
                    })?;
                binding.access_key_id = previous.access_key_id.clone();
                binding.secret_access_key = previous.secret_access_key.clone();
            }
        }
        if let Some(binding) = backup.as_deref_mut() {
            let previous = self
                .state_store
                .get_backup(app)
                .map_err(|error| format!("Failed to read backup config: {error}"))?
                .filter(|previous| same(&previous.storage, &binding.storage))
                .ok_or("The backup target needs its keys; deploy it with a secret snapshot.")?;
            binding.storage.access_key_id = previous.storage.access_key_id;
            binding.storage.secret_access_key = previous.storage.secret_access_key;
            binding.backup_keys = previous.backup_keys;
        }
        Ok(())
    }
}

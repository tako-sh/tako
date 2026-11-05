//! Authorization for the public native management surface, after authentication.
//!
//! Callers supply verified, current permissions and server-owned provisioned state.
//! They must keep that state stable through dispatch, authorize upload/log endpoints
//! separately, and retain handler validation and process isolation. These read-only
//! checks neither authenticate a request nor provide race-free filesystem handles.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Component, Path};
use tako_core::{BackupBinding, Command, SslBinding, SslProvider, StorageBinding, StorageProvider};
use tako_network::{AppEnv, Permissions};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandClass<'a> {
    AuthenticatedProbe,
    DeploymentPreflight,
    ServerAdmin,
    InternalWorker,
    Application { app: &'a str },
}

/// Classification alone grants nothing. New protocol variants must be reviewed.
pub(crate) fn classify(command: &Command) -> CommandClass<'_> {
    match command {
        Command::Hello { .. } | Command::ServerInfo => CommandClass::AuthenticatedProbe,
        Command::CheckDeploySpace { .. } => CommandClass::DeploymentPreflight,
        Command::List
        | Command::Routes
        | Command::EnterUpgrading { .. }
        | Command::ExitUpgrading { .. }
        | Command::InjectChallengeToken { .. } => CommandClass::ServerAdmin,
        Command::RegisterSchedules { .. }
        | Command::ClaimRun { .. }
        | Command::HeartbeatRun { .. }
        | Command::SaveStep { .. }
        | Command::CompleteRun { .. }
        | Command::CancelRun { .. }
        | Command::FailRun { .. }
        | Command::DeferRun { .. }
        | Command::WaitForEvent { .. }
        | Command::ChannelPublish { .. } => CommandClass::InternalWorker,
        Command::PrepareRelease { app, .. }
        | Command::PrepareReleaseUpload { app, .. }
        | Command::PrepareDeploy { app, .. }
        | Command::CleanupPreparedDeploy { app, .. }
        | Command::CleanupRelease { app, .. }
        | Command::FinalizeRelease { app, .. }
        | Command::RunRelease { app, .. }
        | Command::Deploy { app, .. }
        | Command::BackupNow { app, .. }
        | Command::ListBackups { app }
        | Command::BackupStatus { app }
        | Command::BackupDownloadUrl { app, .. }
        | Command::RestoreBackup { app, .. }
        | Command::Scale { app, .. }
        | Command::Stop { app }
        | Command::Delete { app }
        | Command::Status { app }
        | Command::ListReleases { app }
        | Command::Rollback { app, .. }
        | Command::UpdateSecrets { app, .. }
        | Command::GetSecretsHash { app }
        | Command::EnqueueRun { app, .. }
        | Command::Signal { app, .. } => CommandClass::Application { app },
    }
}

/// A trusted snapshot for exactly one deployment. Never construct from request
/// metadata or substitute empty values when reading server state fails.
/// Routes must already be provisioned by an administrator. Existing overlapping
/// route claims and shared external credentials are not repaired by this policy.
pub(crate) struct ProvisionedApp<'a> {
    pub scope: &'a AppEnv,
    pub routes: &'a [String],
    pub ssl: &'a SslBinding,
    pub runtime_credentials: &'a HashMap<String, String>,
    pub storages: &'a HashMap<String, StorageBinding>,
    pub backup: Option<&'a BackupBinding>,
}

pub(crate) struct PolicyContext<'a> {
    pub data_dir: &'a Path,
    pub app: Option<ProvisionedApp<'a>>,
}

/// Fixed messages deliberately omit supplied paths, tokens, and stored secrets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Denial(&'static str);

impl fmt::Display for Denial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Denial {}

const SCOPE_REQUIRED: Denial = Denial("An explicit app/environment scope is required.");
const SCOPE_MISMATCH: Denial = Denial("Command app must exactly match the app/environment scope.");
const SCOPE_DENIED: Denial = Denial("Deployment permission is required for this app/environment.");
const PATH_DENIED: Denial = Denial(
    "Release path must be an existing directory within this app/environment's managed releases, without traversal or symlink redirection.",
);
const MANAGED_PATH_DENIED: Denial = Denial(
    "Managed app/environment directories must not redirect through symlinks or non-directory entries.",
);
const STATE_REQUIRED: Denial =
    Denial("Trusted provisioned state for this app/environment is required.");
const ROUTES_DENIED: Denial = Denial(
    "Scoped deployment must preserve the provisioned routes; a server administrator must provision route changes.",
);
const SSL_DENIED: Denial = Denial(
    "Scoped deployment must preserve provisioned SSL credentials and provider; unresolved staged credentials are not allowed.",
);
const BACKUP_DENIED: Denial =
    Denial("Scoped deployment must preserve the provisioned backup target and keys.");

/// Also use for authenticated non-RPC app descriptors (logs and artifact uploads).
/// Uploads still require version, managed-path, and archive validation.
pub(crate) fn authorize_app(
    permissions: &Permissions,
    scope: &AppEnv,
    app: &str,
) -> Result<(), Denial> {
    permissions
        .validate()
        .map_err(|_| Denial("Authenticated management permissions are invalid."))?;
    if app != scope.as_str() {
        return Err(SCOPE_MISMATCH);
    }
    // AppEnv permits a few names that existing server release handlers reject.
    crate::release::validate_app_name(scope.as_str())
        .map_err(|_| Denial("App/environment scope is not a valid managed deployment name."))?;
    if !permissions.server_admin && !permissions.can_deploy(scope) {
        return Err(SCOPE_DENIED);
    }
    Ok(())
}

pub(crate) fn authorize_command(
    permissions: &Permissions,
    scope: Option<&AppEnv>,
    command: &Command,
    context: &PolicyContext<'_>,
) -> Result<(), Denial> {
    permissions
        .validate()
        .map_err(|_| Denial("Authenticated management permissions are invalid."))?;
    let scope = match classify(command) {
        CommandClass::InternalWorker => {
            return Err(Denial(
                "Internal worker commands are not allowed on public management.",
            ));
        }
        CommandClass::AuthenticatedProbe => return Ok(()),
        CommandClass::ServerAdmin => {
            return if permissions.server_admin {
                Ok(())
            } else {
                Err(Denial(
                    "This command requires server administrator permission.",
                ))
            };
        }
        CommandClass::DeploymentPreflight => {
            return if permissions.server_admin {
                Ok(())
            } else {
                let scope = scope.ok_or(SCOPE_REQUIRED)?;
                authorize_app(permissions, scope, scope.as_str())
            };
        }
        CommandClass::Application { app } => {
            let scope = scope.ok_or(SCOPE_REQUIRED)?;
            authorize_app(permissions, scope, app)?;
            scope
        }
    };

    let app_root = Path::new("apps").join(scope.as_str());
    let releases = app_root.join("releases");
    validate_managed_directories(context.data_dir, &releases)?;
    // Backup, workflow and runtime handlers also access these app-owned roots.
    validate_managed_directories(context.data_dir, &app_root.join("data/app"))?;
    validate_managed_directories(context.data_dir, &app_root.join("data/tako"))?;
    // Exhaustive again: adding a variant requires reviewing its extra effects,
    // even if it has already been classified as an application operation.
    match command {
        Command::PrepareRelease { path, .. } => {
            validate_release_path(context.data_dir, scope, path)?;
        }
        Command::PrepareDeploy {
            version,
            path,
            routes,
            ssl,
            ..
        } => {
            validate_version_path(context.data_dir, &releases, version)?;
            validate_release_path(context.data_dir, scope, path)?;
            if !permissions.server_admin {
                let provisioned = provisioned_app(context, scope)?;
                preserve_routes(provisioned, routes)?;
                preserve_ssl(provisioned, ssl)?;
            }
        }
        Command::Deploy {
            version,
            path,
            routes,
            ssl,
            runtime_credentials,
            storages,
            backup,
            ..
        } => {
            validate_version_path(context.data_dir, &releases, version)?;
            validate_release_path(context.data_dir, scope, path)?;
            if !permissions.server_admin {
                let provisioned = provisioned_app(context, scope)?;
                preserve_routes(provisioned, routes)?;
                preserve_ssl(provisioned, ssl)?;
                if runtime_credentials
                    .as_ref()
                    .is_some_and(|value| value != provisioned.runtime_credentials)
                {
                    return Err(Denial(
                        "Scoped deployment cannot change server-held runtime credentials.",
                    ));
                }
                if storages
                    .as_ref()
                    .is_some_and(|value| value != provisioned.storages)
                {
                    return Err(Denial(
                        "Scoped deployment cannot change provisioned storage paths, endpoints, or credentials.",
                    ));
                }
                validate_local_storages(context.data_dir, scope, provisioned.storages)?;
                if backup.as_deref() != provisioned.backup {
                    return Err(BACKUP_DENIED);
                }
            }
        }
        Command::RunRelease { version, path, .. } => {
            validate_version_path(context.data_dir, &releases, version)?;
            validate_release_path(context.data_dir, scope, path)?;
        }
        Command::PrepareReleaseUpload { version, .. }
        | Command::CleanupPreparedDeploy { version, .. }
        | Command::CleanupRelease { version, .. }
        | Command::FinalizeRelease { version, .. } => {
            validate_version_path(context.data_dir, &releases, version)?;
        }
        Command::Rollback { version, .. } => {
            validate_version_path(context.data_dir, &releases, version)?;
            let path = context.data_dir.join(&releases).join(version);
            validate_release_path(context.data_dir, scope, path.to_str().ok_or(PATH_DENIED)?)?;
            if !permissions.server_admin {
                let provisioned = provisioned_app(context, scope)?;
                preserve_routes(provisioned, provisioned.routes)?;
                preserve_ssl(provisioned, provisioned.ssl)?;
                validate_local_storages(context.data_dir, scope, provisioned.storages)?;
                // Existing rollback constructs Deploy with backup=None (clear).
                if provisioned.backup.is_some() {
                    return Err(BACKUP_DENIED);
                }
            }
        }
        Command::BackupNow { backup, .. } => {
            if !permissions.server_admin
                && let Some(backup) = backup
                && Some(backup.as_ref()) != provisioned_app(context, scope)?.backup
            {
                return Err(BACKUP_DENIED);
            }
        }
        Command::ListBackups { .. }
        | Command::BackupStatus { .. }
        | Command::BackupDownloadUrl { .. }
        | Command::RestoreBackup { .. }
        | Command::Scale { .. }
        | Command::Stop { .. }
        | Command::Delete { .. }
        | Command::Status { .. }
        | Command::ListReleases { .. }
        | Command::UpdateSecrets { .. }
        | Command::GetSecretsHash { .. }
        | Command::EnqueueRun { .. }
        | Command::Signal { .. } => {}
        Command::Hello { .. }
        | Command::ServerInfo
        | Command::CheckDeploySpace { .. }
        | Command::List
        | Command::Routes
        | Command::EnterUpgrading { .. }
        | Command::ExitUpgrading { .. }
        | Command::InjectChallengeToken { .. }
        | Command::RegisterSchedules { .. }
        | Command::ClaimRun { .. }
        | Command::HeartbeatRun { .. }
        | Command::SaveStep { .. }
        | Command::CompleteRun { .. }
        | Command::CancelRun { .. }
        | Command::FailRun { .. }
        | Command::DeferRun { .. }
        | Command::WaitForEvent { .. }
        | Command::ChannelPublish { .. } => {
            return Err(Denial(
                "Command is not an application management operation.",
            ));
        }
    }
    Ok(())
}

fn provisioned_app<'a, 'b>(
    context: &'a PolicyContext<'b>,
    scope: &AppEnv,
) -> Result<&'a ProvisionedApp<'b>, Denial> {
    context
        .app
        .as_ref()
        .filter(|app| app.scope == scope)
        .ok_or(STATE_REQUIRED)
}

fn preserve_routes(app: &ProvisionedApp<'_>, requested: &[String]) -> Result<(), Denial> {
    let existing: BTreeSet<_> = app.routes.iter().collect();
    let requested_set: BTreeSet<_> = requested.iter().collect();
    if existing.is_empty() || existing != requested_set || requested.len() != requested_set.len() {
        return Err(ROUTES_DENIED);
    }
    crate::release::validate_deploy_routes(requested).map_err(|_| ROUTES_DENIED)
}

fn preserve_ssl(app: &ProvisionedApp<'_>, requested: &SslBinding) -> Result<(), Denial> {
    if requested != app.ssl {
        return Err(SSL_DENIED);
    }
    // Match resolve_deploy_ssl_binding's token requirement. Refuse its fallback
    // to staged state: that state is not part of this checked snapshot.
    let needs_token = requested.provider == SslProvider::Cloudflare
        || app.routes.iter().any(|route| route.starts_with("*."));
    if needs_token
        && requested
            .cloudflare_api_token
            .as_deref()
            .is_none_or(|token| token.trim().is_empty())
    {
        return Err(SSL_DENIED);
    }
    Ok(())
}

fn validate_version_path(data_dir: &Path, releases: &Path, version: &str) -> Result<(), Denial> {
    crate::release::validate_release_version(version)
        .map_err(|_| Denial("Release version must be a valid single managed path segment."))?;
    validate_managed_directories(data_dir, &releases.join(version))
}

fn validate_local_storages(
    data_dir: &Path,
    scope: &AppEnv,
    storages: &HashMap<String, StorageBinding>,
) -> Result<(), Denial> {
    let denied = Denial(
        "Local storage must use a relative path inside this app/environment's data directory, without traversal or symlink redirection.",
    );
    for binding in storages.values() {
        match binding.provider {
            StorageProvider::S3 => {}
            StorageProvider::Local => {
                let path = binding.path.as_deref().ok_or(denied)?;
                // The SDK resolves this path against TAKO_DATA_DIR. Absolute
                // paths or '..' would select a root outside this deployment.
                if path.is_empty()
                    || Path::new(path).is_absolute()
                    || path.contains('\\')
                    || path.split('/').any(|part| matches!(part, "" | "." | ".."))
                {
                    return Err(denied);
                }
                let relative = Path::new("apps")
                    .join(scope.as_str())
                    .join("data/app")
                    .join(path);
                validate_managed_directories(data_dir, &relative).map_err(|_| denied)?;
            }
        }
    }
    Ok(())
}

fn validate_release_path(data_dir: &Path, scope: &AppEnv, path: &str) -> Result<(), Denial> {
    let supplied = Path::new(path);
    if !supplied.is_absolute()
        || supplied
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(PATH_DENIED);
    }
    let resolved = crate::release::validate_release_path_for_app(data_dir, scope.as_str(), path)
        .map_err(|_| PATH_DENIED)?;
    let canonical_data = std::fs::canonicalize(data_dir).map_err(|_| PATH_DENIED)?;
    let root = canonical_data
        .join("apps")
        .join(scope.as_str())
        .join("releases");
    if resolved == root || !resolved.starts_with(&root) {
        return Err(PATH_DENIED);
    }
    // Check the supplied spelling too, so an alias cannot mask redirected roots.
    let relative = supplied
        .strip_prefix(data_dir)
        .or_else(|_| supplied.strip_prefix(&canonical_data))
        .map_err(|_| PATH_DENIED)?;
    validate_managed_directories(data_dir, relative).map_err(|_| PATH_DENIED)
}

fn validate_managed_directories(data_dir: &Path, relative: &Path) -> Result<(), Denial> {
    let mut path = std::fs::canonicalize(data_dir).map_err(|_| MANAGED_PATH_DENIED)?;
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            return Err(MANAGED_PATH_DENIED);
        };
        path.push(segment);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(MANAGED_PATH_DENIED),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(MANAGED_PATH_DENIED),
        }
    }
    Ok(())
}

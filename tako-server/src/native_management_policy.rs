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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    struct Fixture {
        dir: TempDir,
        scope: AppEnv,
        permissions: Permissions,
        routes: Vec<String>,
        ssl: SslBinding,
        runtime_credentials: HashMap<String, String>,
        storages: HashMap<String, StorageBinding>,
        backup: Option<BackupBinding>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            for app in ["demo/staging", "demo/production", "other/staging"] {
                std::fs::create_dir_all(dir.path().join("apps").join(app).join("releases/v1"))
                    .unwrap();
            }
            let scope: AppEnv = tako_core::deployment_app_id("demo", "staging")
                .parse()
                .unwrap();
            let permissions = Permissions {
                deployments: [scope.clone()].into_iter().collect(),
                ..Permissions::default()
            };
            Self {
                dir,
                scope,
                permissions,
                routes: vec![
                    "demo.example.test/api/*".into(),
                    "demo.example.test/assets/*".into(),
                ],
                ssl: SslBinding::default(),
                runtime_credentials: HashMap::new(),
                storages: HashMap::new(),
                backup: None,
            }
        }

        fn context(&self) -> PolicyContext<'_> {
            PolicyContext {
                data_dir: self.dir.path(),
                app: Some(ProvisionedApp {
                    scope: &self.scope,
                    routes: &self.routes,
                    ssl: &self.ssl,
                    runtime_credentials: &self.runtime_credentials,
                    storages: &self.storages,
                    backup: self.backup.as_ref(),
                }),
            }
        }

        fn release(&self, app: &str) -> String {
            self.dir
                .path()
                .join("apps")
                .join(app)
                .join("releases/v1")
                .to_str()
                .unwrap()
                .into()
        }

        fn command(&self, name: &str) -> Command {
            self.decode_command(json!({"command": name}))
        }

        fn decode_command(&self, mut value: Value) -> Command {
            let object = value.as_object_mut().unwrap();
            object.entry("app").or_insert(json!(self.scope.as_str()));
            object.entry("version").or_insert(json!("v1"));
            object
                .entry("path")
                .or_insert(json!(self.release(self.scope.as_str())));
            object.entry("routes").or_insert(json!(self.routes));
            object.entry("ssl").or_insert(json!(self.ssl));
            object.entry("secrets").or_insert(json!({}));
            object.entry("backup_id").or_insert(json!("backup-1"));
            object.entry("command_line").or_insert(json!("true"));
            object.entry("payload").or_insert(json!({}));
            object.entry("name").or_insert(json!("job"));
            object.entry("event_name").or_insert(json!("event"));
            serde_json::from_value(value).unwrap()
        }

        fn authorize(&self, command: &Command) -> Result<(), Denial> {
            authorize_command(
                &self.permissions,
                Some(&self.scope),
                command,
                &self.context(),
            )
        }
    }

    const APP_COMMANDS: &[&str] = &[
        "prepare_release",
        "prepare_release_upload",
        "prepare_deploy",
        "cleanup_prepared_deploy",
        "cleanup_release",
        "finalize_release",
        "run_release",
        "deploy",
        "backup_now",
        "list_backups",
        "backup_status",
        "backup_download_url",
        "restore_backup",
        "scale",
        "stop",
        "delete",
        "status",
        "list_releases",
        "rollback",
        "update_secrets",
        "get_secrets_hash",
        "enqueue_run",
        "signal",
    ];

    #[test]
    fn all_application_commands_require_exact_explicit_app_and_environment() {
        let f = Fixture::new();
        for name in APP_COMMANDS {
            let command = f.command(name);
            assert_eq!(
                classify(&command),
                CommandClass::Application {
                    app: f.scope.as_str()
                }
            );
            assert_eq!(f.authorize(&command), Ok(()), "{name}");
            assert_eq!(
                authorize_command(&f.permissions, None, &command, &f.context()),
                Err(SCOPE_REQUIRED),
                "{name}"
            );
            for app in [
                "demo/production",
                "other/staging",
                "demo",
                "demo/staging/extra",
                "demo%2Fstaging",
                "../staging",
            ] {
                let command = f.decode_command(json!({"command": name, "app": app}));
                assert_eq!(f.authorize(&command), Err(SCOPE_MISMATCH), "{name}: {app}");
            }
        }
    }

    #[test]
    fn selecting_another_scope_does_not_manufacture_a_grant() {
        let f = Fixture::new();
        for app in ["demo/production", "other/staging"] {
            let scope: AppEnv = app.parse().unwrap();
            let command = Command::Status { app: app.into() };
            assert_eq!(
                authorize_command(&f.permissions, Some(&scope), &command, &f.context()),
                Err(SCOPE_DENIED)
            );
        }
        assert_eq!(
            authorize_app(&f.permissions, &f.scope, "demo"),
            Err(SCOPE_MISMATCH)
        );
        assert_eq!(
            authorize_app(&f.permissions, &f.scope, f.scope.as_str()),
            Ok(())
        );
    }

    #[test]
    fn public_worker_impersonation_is_denied_even_for_server_admin() {
        let f = Fixture::new();
        let admin = Permissions {
            server_admin: true,
            ..f.permissions.clone()
        };
        for name in [
            "register_schedules",
            "claim_run",
            "heartbeat_run",
            "save_step",
            "complete_run",
            "cancel_run",
            "fail_run",
            "defer_run",
            "wait_for_event",
            "channel_publish",
        ] {
            let command = f.decode_command(json!({
                "command": name, "schedules": [], "worker_id": "trusted-worker", "names": ["job"],
                "lease_ms": 1000, "id": "run-1", "step_name": "step", "result": {},
                "error": "failed", "finalize": true, "channel": "private",
            }));
            assert_eq!(classify(&command), CommandClass::InternalWorker, "{name}");
            for permissions in [&f.permissions, &admin] {
                let error = authorize_command(permissions, Some(&f.scope), &command, &f.context())
                    .unwrap_err();
                assert!(error.to_string().contains("Internal worker"), "{name}");
            }
        }
    }

    #[test]
    fn network_admin_and_ssh_are_not_server_or_deployment_authority() {
        let f = Fixture::new();
        let network_admin = Permissions {
            network_admin: true,
            ssh_user: Some("developer".parse().unwrap()),
            ..Permissions::default()
        };
        let admin = Permissions {
            server_admin: true,
            ..Permissions::default()
        };
        for command in [
            Command::List,
            Command::Routes,
            Command::EnterUpgrading { owner: "x".into() },
            Command::ExitUpgrading { owner: "x".into() },
            Command::InjectChallengeToken {
                token: "x".into(),
                key_authorization: "x".into(),
            },
        ] {
            assert_eq!(classify(&command), CommandClass::ServerAdmin);
            for permissions in [&network_admin, &f.permissions] {
                assert!(
                    authorize_command(permissions, Some(&f.scope), &command, &f.context()).is_err()
                );
            }
            assert!(authorize_command(&admin, None, &command, &f.context()).is_ok());
        }
        assert_eq!(
            authorize_app(&network_admin, &f.scope, f.scope.as_str()),
            Err(SCOPE_DENIED)
        );
        assert_eq!(
            authorize_command(&admin, None, &f.command("status"), &f.context()),
            Err(SCOPE_REQUIRED)
        );
        assert_eq!(
            authorize_command(
                &admin,
                Some(&f.scope),
                &f.decode_command(json!({"command":"status", "app":"demo/production"})),
                &f.context()
            ),
            Err(SCOPE_MISMATCH)
        );
    }

    #[test]
    fn authenticated_probes_and_scoped_read_only_preflight() {
        let f = Fixture::new();
        for command in [
            Command::Hello {
                protocol_version: 0,
            },
            Command::ServerInfo,
        ] {
            assert_eq!(classify(&command), CommandClass::AuthenticatedProbe);
            assert!(
                authorize_command(&Permissions::default(), None, &command, &f.context()).is_ok()
            );
        }
        let command = Command::CheckDeploySpace {
            min_free_bytes: u64::MAX,
        };
        assert_eq!(classify(&command), CommandClass::DeploymentPreflight);
        assert!(f.authorize(&command).is_ok());
        assert_eq!(
            authorize_command(&f.permissions, None, &command, &f.context()),
            Err(SCOPE_REQUIRED)
        );
        assert_eq!(
            authorize_command(
                &Permissions::default(),
                Some(&f.scope),
                &command,
                &f.context()
            ),
            Err(SCOPE_DENIED)
        );
        let invalid = Permissions {
            ssh_user: Some("root".parse().unwrap()),
            ..Permissions::default()
        };
        assert!(authorize_command(&invalid, None, &Command::ServerInfo, &f.context()).is_err());
    }

    #[test]
    fn route_takeover_and_ownership_changes_are_denied_despite_force() {
        let f = Fixture::new();
        for name in ["prepare_deploy", "deploy"] {
            for force in [false, true] {
                for routes in [
                    vec!["victim.example.test"],
                    vec!["*.example.test"],
                    vec!["demo.example.test/*"],
                    vec!["demo.example.test/api/private"],
                    vec!["demo.example.test/api/*"],
                    vec![],
                ] {
                    let command =
                        f.decode_command(json!({"command":name,"routes":routes,"force":force}));
                    assert_eq!(
                        f.authorize(&command),
                        Err(ROUTES_DENIED),
                        "{name} {routes:?}"
                    );
                }
            }
            let command =
                f.decode_command(json!({"command":name,"routes":[f.routes[1],f.routes[0]]}));
            assert_eq!(f.authorize(&command), Ok(()));
            let command = f.decode_command(
                json!({"command":name,"routes":[f.routes[0],f.routes[1],f.routes[0]]}),
            );
            assert_eq!(f.authorize(&command), Err(ROUTES_DENIED));
        }
    }

    #[test]
    fn missing_or_cross_scope_provisioning_cannot_grant_routes() {
        let mut f = Fixture::new();
        let command = f.command("deploy");
        let missing = PolicyContext {
            data_dir: f.dir.path(),
            app: None,
        };
        assert_eq!(
            authorize_command(&f.permissions, Some(&f.scope), &command, &missing),
            Err(STATE_REQUIRED)
        );
        let other: AppEnv = "other/staging".parse().unwrap();
        let mut context = f.context();
        context.app.as_mut().unwrap().scope = &other;
        assert_eq!(
            authorize_command(&f.permissions, Some(&f.scope), &command, &context),
            Err(STATE_REQUIRED)
        );
        f.routes.clear();
        assert_eq!(f.authorize(&f.command("deploy")), Err(ROUTES_DENIED));
    }

    #[test]
    fn every_explicit_path_command_rejects_cross_app_and_traversal_even_with_force() {
        let f = Fixture::new();
        for name in ["prepare_release", "prepare_deploy", "run_release", "deploy"] {
            for force in [false, true] {
                for path in [
                    f.release("demo/production"),
                    f.release("other/staging"),
                    format!("{}/../v1", f.release(f.scope.as_str())),
                    format!("{}/apps/demo/staging/releases", f.dir.path().display()),
                    "apps/demo/staging/releases/v1".into(),
                    "/etc".into(),
                    format!("{}/missing", f.release(f.scope.as_str())),
                ] {
                    let command =
                        f.decode_command(json!({"command":name,"path":path,"force":force}));
                    assert_eq!(f.authorize(&command), Err(PATH_DENIED), "{name}: {path}");
                }
                let command =
                    f.decode_command(json!({"command":name,"app":"demo/production","force":force}));
                assert_eq!(f.authorize(&command), Err(SCOPE_MISMATCH));
            }
        }
    }

    #[test]
    fn derived_paths_reject_malicious_versions_but_allow_new_uploads() {
        let f = Fixture::new();
        for name in [
            "prepare_release_upload",
            "prepare_deploy",
            "cleanup_prepared_deploy",
            "cleanup_release",
            "finalize_release",
            "run_release",
            "deploy",
            "rollback",
        ] {
            for version in [
                "../production",
                "../../other/staging",
                "/etc",
                "..",
                "v1/child",
                "v1\\child",
                "v1%2fchild",
            ] {
                assert!(
                    f.authorize(&f.decode_command(json!({"command":name,"version":version})))
                        .is_err(),
                    "{name}: {version}"
                );
            }
        }
        assert_eq!(
            f.authorize(
                &f.decode_command(
                    json!({"command":"prepare_release_upload","version":"new-build"})
                )
            ),
            Ok(())
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_redirect_releases_or_the_app_root() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let releases = f.dir.path().join("apps/demo/staging/releases");
        symlink(f.release("other/staging"), releases.join("escape")).unwrap();
        for name in ["prepare_release", "prepare_deploy", "run_release", "deploy"] {
            let command = f.decode_command(
                json!({"command":name,"path":releases.join("escape"),"force":true}),
            );
            assert!(f.authorize(&command).is_err(), "{name}");
        }
        for name in [
            "prepare_release_upload",
            "cleanup_prepared_deploy",
            "cleanup_release",
            "finalize_release",
            "rollback",
        ] {
            assert!(
                f.authorize(&f.decode_command(json!({"command":name,"version":"escape"})))
                    .is_err(),
                "{name}"
            );
        }
        std::fs::rename(&releases, releases.with_file_name("saved-releases")).unwrap();
        symlink(f.dir.path().join("apps/other/staging/releases"), &releases).unwrap();
        // The existing validator follows this root; the policy must not.
        assert!(
            crate::release::validate_release_path_for_app(
                f.dir.path(),
                f.scope.as_str(),
                &f.release(f.scope.as_str())
            )
            .is_ok()
        );
        for name in APP_COMMANDS {
            assert_eq!(
                f.authorize(&f.command(name)),
                Err(MANAGED_PATH_DENIED),
                "{name}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn backup_and_runtime_data_roots_cannot_redirect_to_another_deployment() {
        use std::os::unix::fs::symlink;
        for relative in ["data", "data/app", "data/tako"] {
            let f = Fixture::new();
            let link = f.dir.path().join("apps/demo/staging").join(relative);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(f.dir.path().join("apps/other/staging"), &link).unwrap();
            for name in [
                "backup_now",
                "restore_backup",
                "enqueue_run",
                "deploy",
                "run_release",
            ] {
                assert_eq!(
                    f.authorize(&f.command(name)),
                    Err(MANAGED_PATH_DENIED),
                    "{relative}: {name}"
                );
            }
        }
    }

    #[test]
    fn ssl_provider_tokens_and_unresolved_staging_cannot_be_changed() {
        let mut f = Fixture::new();
        for name in ["prepare_deploy", "deploy"] {
            for ssl in [
                json!({"provider":"cloudflare","cloudflare_api_token":"attacker-secret"}),
                json!({"provider":"letsencrypt","cloudflare_api_token":"attacker-secret"}),
            ] {
                let error = f
                    .authorize(&f.decode_command(json!({"command":name,"ssl":ssl,"force":true})))
                    .unwrap_err();
                assert_eq!(error, SSL_DENIED);
                assert!(!error.to_string().contains("attacker-secret"));
            }
        }
        f.ssl.provider = SslProvider::Cloudflare;
        assert_eq!(f.authorize(&f.command("deploy")), Err(SSL_DENIED));
        f.ssl.cloudflare_api_token = Some("provisioned-token".into());
        assert_eq!(f.authorize(&f.command("deploy")), Ok(()));
        assert_eq!(
            f.authorize(
                &f.decode_command(json!({"command":"deploy","ssl":{"provider":"cloudflare"}}))
            ),
            Err(SSL_DENIED)
        );
        f.ssl = SslBinding::default();
        f.routes = vec!["*.example.test".into()];
        assert_eq!(f.authorize(&f.command("deploy")), Err(SSL_DENIED));
    }

    fn storage() -> StorageBinding {
        serde_json::from_value(
            json!({"provider":"local","path":"storage/files","signing_key":"key"}),
        )
        .unwrap()
    }

    fn backup() -> BackupBinding {
        serde_json::from_value(json!({
            "storage":{"provider":"s3","bucket":"demo-staging","endpoint":"https://storage.example.test", "access_key_id":"key-id","secret_access_key":"secret"},
            "backup_keys":[{"id":"key-1","key_base64":"secret-key"}],"retention_days":7
        })).unwrap()
    }

    #[test]
    fn runtime_credentials_and_local_or_remote_storage_are_frozen() {
        let mut f = Fixture::new();
        f.runtime_credentials
            .insert("provider-key".into(), "server-secret".into());
        f.storages.insert("files".into(), storage());
        assert_eq!(f.authorize(&f.command("deploy")), Ok(()));
        let command = f.decode_command(json!({"command":"deploy", "runtime_credentials":f.runtime_credentials,"storages":f.storages}));
        assert_eq!(f.authorize(&command), Ok(()));
        for value in [json!({}), json!({"provider-key":"attacker-secret"})] {
            let command = f.decode_command(
                json!({"command":"deploy","runtime_credentials":value,"force":true}),
            );
            assert!(
                f.authorize(&command)
                    .unwrap_err()
                    .to_string()
                    .contains("runtime credentials")
            );
        }
        for value in [
            json!({}),
            json!({"files":{"provider":"local","path":"/etc","signing_key":"x"}}),
            json!({"files":{"provider":"local","path":"/managed/other/production"}}),
            json!({"files":{"provider":"s3","endpoint":"http://127.0.0.1","bucket":"other"}}),
        ] {
            let command =
                f.decode_command(json!({"command":"deploy","storages":value,"force":true}));
            assert!(
                f.authorize(&command)
                    .unwrap_err()
                    .to_string()
                    .contains("storage")
            );
        }
        // Application secrets are a distinct, writable resource within this scope.
        assert_eq!(
            f.authorize(
                &f.decode_command(json!({"command":"update_secrets","secrets":{"APP_KEY":"new"}}))
            ),
            Ok(())
        );
    }

    #[test]
    fn backup_replacement_and_implicit_deletion_are_not_hidden_by_deploy_or_rollback() {
        let mut f = Fixture::new();
        let unprovisioned = f.decode_command(json!({"command":"backup_now","backup":backup()}));
        assert_eq!(f.authorize(&unprovisioned), Err(BACKUP_DENIED));
        f.backup = Some(backup());
        assert_eq!(f.authorize(&f.command("deploy")), Err(BACKUP_DENIED));
        assert_eq!(f.authorize(&f.command("rollback")), Err(BACKUP_DENIED));
        assert_eq!(f.authorize(&f.command("backup_now")), Ok(()));
        for name in ["deploy", "backup_now"] {
            assert_eq!(
                f.authorize(&f.decode_command(json!({"command":name,"backup":f.backup}))),
                Ok(())
            );
            let mut changed = backup();
            changed.storage.bucket = Some("other-production".into());
            assert_eq!(
                f.authorize(
                    &f.decode_command(json!({"command":name,"backup":changed,"force":true}))
                ),
                Err(BACKUP_DENIED)
            );
        }
    }

    #[test]
    fn retained_local_storage_does_not_authorize_cross_app_filesystem_access() {
        let mut f = Fixture::new();
        for path in [
            "/etc",
            "/managed/other/production",
            "../other",
            "storage/../../other",
            "",
            "storage/./files",
            "storage\\other",
        ] {
            let mut binding = storage();
            binding.path = Some(path.into());
            f.storages.insert("files".into(), binding);
            for name in ["deploy", "rollback"] {
                let error = f.authorize(&f.command(name)).unwrap_err();
                assert!(
                    error.to_string().starts_with("Local storage"),
                    "{name}: {path}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn retained_local_storage_cannot_follow_a_cross_app_symlink() {
        let mut f = Fixture::new();
        let root = f.dir.path().join("apps/demo/staging/data/app/storage");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(f.dir.path().join("apps/other/staging"), root.join("files"))
            .unwrap();
        f.storages.insert("files".into(), storage());
        assert!(
            f.authorize(&f.command("deploy"))
                .unwrap_err()
                .to_string()
                .starts_with("Local storage")
        );
    }

    #[test]
    fn server_admin_can_provision_but_never_bypass_path_confinement() {
        let mut f = Fixture::new();
        f.permissions = Permissions {
            server_admin: true,
            ..Permissions::default()
        };
        let command = f.decode_command(json!({"command":"deploy", "routes":["new.example.test"],
            "ssl":{"provider":"cloudflare","cloudflare_api_token":"admin-token"},
            "runtime_credentials":{"key":"value"}, "storages":{"files":storage()}, "backup":backup()}));
        assert_eq!(f.authorize(&command), Ok(()));
        assert_eq!(
            f.authorize(&f.decode_command(
                json!({"command":"deploy","path":f.release("other/staging"),"force":true})
            )),
            Err(PATH_DENIED)
        );
    }
}

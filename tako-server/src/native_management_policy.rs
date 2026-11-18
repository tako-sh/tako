//! Authorization for the public native management surface, after authentication.
//!
//! Callers supply verified, current permissions and server-owned provisioned state.
//! They must keep that state stable through dispatch, authorize upload/log endpoints
//! separately, and retain handler validation and process isolation. These read-only
//! checks neither authenticate a request nor provide race-free filesystem handles.

use std::collections::HashMap;
use std::fmt;
use std::path::{Component, Path};
use tako_core::{Command, DatabaseBinding, StorageBinding, StorageProvider};
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
        | Command::DrainServer { .. }
        | Command::ResumeServer
        | Command::EnterUpgrading { .. }
        | Command::ExitUpgrading { .. }
        | Command::InjectChallengeToken { .. }
        | Command::CreateDatabase { .. }
        | Command::DeleteDatabase { .. }
        | Command::DatabaseStatus { .. }
        | Command::ConfigureDatabaseBackup { .. }
        | Command::GrantDatabases { .. }
        | Command::SnapshotDatabase { app: None, .. }
        | Command::ListDatabaseSnapshots { .. }
        | Command::RestoreDatabase { .. }
        | Command::QueryDatabase { .. }
        | Command::MigrateDatabase { app: None, .. }
        | Command::ResetDatabase { .. }
        | Command::BeginDatabaseImport { .. }
        | Command::DatabaseImportChunk { .. }
        | Command::FinishDatabaseImport { .. }
        | Command::BeginDatabaseExport { .. }
        | Command::DatabaseExportChunk { .. }
        | Command::FinishDatabaseExport { .. } => CommandClass::ServerAdmin,
        // Before its release command, a deployer may snapshot a database its
        // app environment uses; the handler checks that relationship.
        Command::SnapshotDatabase { app: Some(app), .. }
        | Command::MigrateDatabase { app: Some(app), .. } => CommandClass::Application { app },
        Command::RegisterSchedules { .. }
        | Command::ClaimRun { .. }
        | Command::HeartbeatRun { .. }
        | Command::SaveStep { .. }
        | Command::GetRunResult { .. }
        | Command::CompleteRun { .. }
        | Command::CancelRun { .. }
        | Command::FailRun { .. }
        | Command::DeferRun { .. }
        | Command::WaitForEvent { .. }
        | Command::NetworkCall { .. }
        | Command::ChannelPublish { .. }
        | Command::ChannelClose { .. } => CommandClass::InternalWorker,
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
        | Command::Start { app, .. }
        | Command::Stop { app, .. }
        | Command::Delete { app }
        | Command::Status { app }
        | Command::ListReleases { app }
        | Command::Rollback { app, .. }
        | Command::SecretsDigest { app, .. }
        | Command::GetSecretsHash { app }
        | Command::EnqueueRun { app, .. }
        | Command::Signal { app, .. }
        | Command::ListWorkflowRuns { app, .. }
        | Command::GetWorkflowRun { app, .. }
        | Command::RetryWorkflowRun { app, .. }
        | Command::CancelWorkflowRun { app, .. } => CommandClass::Application { app },
    }
}

/// The database bindings this server holds for one app environment. Never
/// construct from request metadata or substitute empty values when reading
/// server state fails.
pub(crate) struct ProvisionedApp<'a> {
    pub scope: &'a AppEnv,
    pub databases: &'a HashMap<String, DatabaseBinding>,
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

/// Whether a requested binding names the database a provisioned one does.
/// Peer keys are issued by servers to administrators: one left out keeps the
/// provisioned key, and one sent must be that key.
fn keeps_binding(requested: &DatabaseBinding, provisioned: &DatabaseBinding) -> bool {
    provisioned.same_target(requested)
        && requested
            .peer_key
            .as_ref()
            .is_none_or(|key| provisioned.peer_key.as_ref() == Some(key))
}

/// Whether a requested binding set keeps the provisioned one.
fn same_bindings(
    requested: &HashMap<String, DatabaseBinding>,
    provisioned: &HashMap<String, DatabaseBinding>,
) -> bool {
    requested.len() == provisioned.len()
        && requested.iter().all(|(name, binding)| {
            provisioned
                .get(name)
                .is_some_and(|current| keeps_binding(binding, current))
        })
}

const DATABASES_DENIED: Denial =
    Denial("Scoped deployment cannot change database bindings; a full client must deploy them.");
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
const DEPLOY_ONLY: Denial =
    Denial("Deploy clients can only deploy, roll back, and read status and releases.");

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
    if !permissions.can_deploy(scope) {
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
            return if permissions.is_full() {
                Ok(())
            } else {
                Err(Denial("This command requires full access."))
            };
        }
        CommandClass::DeploymentPreflight => {
            return if permissions.is_full() {
                Ok(())
            } else {
                let scope = scope.ok_or(SCOPE_REQUIRED)?;
                authorize_app(permissions, scope, scope.as_str())
            };
        }
        CommandClass::Application { app } => {
            let scope = scope.ok_or(SCOPE_REQUIRED)?;
            authorize_app(permissions, scope, app)?;
            if !permissions.is_full() && !deploy_client_command(command) {
                return Err(DEPLOY_ONLY);
            }
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
        // A deploy client may make the first deploy of its app environment
        // and change its routes, SSL provider and the plain configuration of
        // its bindings: servers fill every secret part from the pinned
        // snapshot, and reject routes another app environment claims.
        Command::PrepareDeploy { version, path, .. } => {
            validate_version_path(context.data_dir, &releases, version)?;
            validate_release_path(context.data_dir, scope, path)?;
        }
        Command::Deploy {
            version,
            path,
            storages,
            databases,
            ..
        } => {
            validate_version_path(context.data_dir, &releases, version)?;
            validate_release_path(context.data_dir, scope, path)?;
            if !permissions.is_full() {
                if let Some(storages) = storages {
                    validate_local_storages(context.data_dir, scope, storages)?;
                }
                // Database bindings need grants, which only full clients make.
                // An empty set would remove every binding; see
                // `keep_database_bindings`.
                if let Some(databases) = databases.as_ref()
                    && !same_bindings(databases, provisioned_app(context, scope)?.databases)
                {
                    return Err(DATABASES_DENIED);
                }
            }
        }
        Command::RunRelease {
            version,
            path,
            databases,
            ..
        } => {
            validate_version_path(context.data_dir, &releases, version)?;
            validate_release_path(context.data_dir, scope, path)?;
            if !permissions.is_full() && !databases.is_empty() {
                let provisioned = provisioned_app(context, scope)?;
                if databases.iter().any(|(name, binding)| {
                    !provisioned
                        .databases
                        .get(name)
                        .is_some_and(|current| keeps_binding(binding, current))
                }) {
                    return Err(DATABASES_DENIED);
                }
            }
        }
        Command::SnapshotDatabase { .. } | Command::MigrateDatabase { .. } => {}
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
        }
        Command::BackupNow { .. }
        | Command::ListBackups { .. }
        | Command::BackupStatus { .. }
        | Command::BackupDownloadUrl { .. }
        | Command::RestoreBackup { .. }
        | Command::Scale { .. }
        | Command::Start { .. }
        | Command::Stop { .. }
        | Command::Delete { .. }
        | Command::Status { .. }
        | Command::ListReleases { .. }
        | Command::SecretsDigest { .. }
        | Command::GetSecretsHash { .. }
        | Command::EnqueueRun { .. }
        | Command::Signal { .. }
        | Command::ListWorkflowRuns { .. }
        | Command::GetWorkflowRun { .. }
        | Command::RetryWorkflowRun { .. }
        | Command::CancelWorkflowRun { .. } => {}
        Command::Hello { .. }
        | Command::ServerInfo
        | Command::DrainServer { .. }
        | Command::ResumeServer
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
        | Command::GetRunResult { .. }
        | Command::CompleteRun { .. }
        | Command::CancelRun { .. }
        | Command::FailRun { .. }
        | Command::DeferRun { .. }
        | Command::WaitForEvent { .. }
        | Command::NetworkCall { .. }
        | Command::ChannelPublish { .. }
        | Command::ChannelClose { .. }
        | Command::CreateDatabase { .. }
        | Command::DeleteDatabase { .. }
        | Command::DatabaseStatus { .. }
        | Command::ConfigureDatabaseBackup { .. }
        | Command::GrantDatabases { .. }
        | Command::ListDatabaseSnapshots { .. }
        | Command::RestoreDatabase { .. }
        | Command::QueryDatabase { .. }
        | Command::ResetDatabase { .. }
        | Command::BeginDatabaseImport { .. }
        | Command::DatabaseImportChunk { .. }
        | Command::FinishDatabaseImport { .. }
        | Command::BeginDatabaseExport { .. }
        | Command::DatabaseExportChunk { .. }
        | Command::FinishDatabaseExport { .. } => {
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

/// What a deploy client may send in its app environments.
fn deploy_client_command(command: &Command) -> bool {
    matches!(
        command,
        Command::PrepareRelease { .. }
            | Command::PrepareReleaseUpload { .. }
            | Command::PrepareDeploy { .. }
            | Command::Deploy { .. }
            | Command::RunRelease { .. }
            | Command::FinalizeRelease { .. }
            | Command::CleanupPreparedDeploy { .. }
            | Command::CleanupRelease { .. }
            | Command::SnapshotDatabase { .. }
            | Command::MigrateDatabase { .. }
            | Command::SecretsDigest { .. }
            | Command::Status { .. }
            | Command::ListReleases { .. }
            | Command::Rollback { .. }
    )
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

/// A deploy client never changes database bindings, so a deploy from one
/// that sends an empty set keeps the environment's bindings instead of
/// removing them. Call before [`authorize_command`]; a non-empty set that
/// differs is still refused there.
pub(crate) fn keep_database_bindings(permissions: &Permissions, command: &mut Command) {
    if !permissions.is_full()
        && let Command::Deploy { databases, .. } = command
        && databases.as_ref().is_some_and(HashMap::is_empty)
    {
        *databases = None;
    }
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
        databases: HashMap<String, DatabaseBinding>,
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
            let permissions = Permissions::Deploy([scope.clone()].into_iter().collect());
            Self {
                dir,
                scope,
                permissions,
                routes: vec![
                    "demo.example.test/api/*".into(),
                    "demo.example.test/assets/*".into(),
                ],
                databases: HashMap::new(),
            }
        }

        fn context(&self) -> PolicyContext<'_> {
            PolicyContext {
                data_dir: self.dir.path(),
                app: Some(ProvisionedApp {
                    scope: &self.scope,
                    databases: &self.databases,
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
            object
                .entry("ssl")
                .or_insert(json!(tako_core::SslBinding::default()));
            object.entry("secrets").or_insert(json!({}));
            object.entry("backup_id").or_insert(json!("backup-1"));
            object.entry("reason").or_insert(json!("release"));
            object.entry("command_line").or_insert(json!("true"));
            object.entry("payload").or_insert(json!({}));
            object.entry("name").or_insert(json!("job"));
            object.entry("event_name").or_insert(json!("event"));
            object.entry("id").or_insert(json!("run-1"));
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
        "snapshot_database",
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
        "secrets_digest",
        "get_secrets_hash",
        "enqueue_run",
        "signal",
        "list_workflow_runs",
        "get_workflow_run",
        "retry_workflow_run",
        "cancel_workflow_run",
    ];

    #[test]
    fn database_administration_requires_server_administrator_permission() {
        let id = "shop";
        for command in [
            json!({"command":"create_database","name":id,"generation":"0123456789abcdef0123456789abcdef","backup_key_base64":"a2V5"}),
            json!({"command":"delete_database","name":id}),
            json!({"command":"database_status"}),
            json!({"command":"configure_database_backup","name":id,"backup":null}),
            json!({"command":"grant_databases","app":"demo/staging","databases":[id]}),
            json!({"command":"snapshot_database","name":id,"reason":"manual","app":null}),
            json!({"command":"list_database_snapshots","name":id}),
            json!({"command":"restore_database","name":id}),
            json!({"command":"query_database","name":id,"sql":"SELECT 1"}),
            json!({"command":"migrate_database","name":id,"owner":"demo","migrations":[]}),
            json!({"command":"reset_database","name":id}),
            json!({"command":"begin_database_import","name":id,"size_bytes":1,"sha256":"00"}),
            json!({"command":"begin_database_export","name":id}),
        ] {
            let command: Command = serde_json::from_value(command).unwrap();
            assert_eq!(classify(&command), CommandClass::ServerAdmin, "{command:?}");
        }
        // The pre-release snapshot is the app deployer's; the server checks the
        // app uses the database.
        let release: Command = serde_json::from_value(json!({
            "command": "snapshot_database", "name": id, "reason": "release", "app": "demo/staging"
        }))
        .unwrap();
        assert!(
            matches!(classify(&release), CommandClass::Application { .. }),
            "{:?}",
            classify(&release)
        );
        // So are its own app's migrations, on a database its app uses.
        let migrations: Command = serde_json::from_value(json!({
            "command": "migrate_database", "name": id, "owner": "demo",
            "migrations": [{"name": "0001_initial.sql", "sql": "SELECT 1"}],
            "app": "demo/staging"
        }))
        .unwrap();
        assert_eq!(
            classify(&migrations),
            CommandClass::Application {
                app: "demo/staging"
            }
        );
    }

    #[test]
    fn scoped_deploys_cannot_change_database_bindings() {
        let f = Fixture::new();
        let binding = json!({"main": {"name": "shop", "server": "edge"}});
        let deploy = f.decode_command(json!({"command":"deploy","databases":binding}));
        assert_eq!(f.authorize(&deploy), Err(DATABASES_DENIED));
        let release = f.decode_command(json!({"command":"run_release","databases":binding}));
        assert_eq!(f.authorize(&release), Err(DATABASES_DENIED));
        let unchanged = f.decode_command(json!({"command":"deploy","databases":null}));
        assert_eq!(f.authorize(&unchanged), Ok(()));
    }

    #[test]
    fn an_empty_database_set_from_a_deploy_client_keeps_the_bindings() {
        let mut f = Fixture::new();
        f.databases.insert(
            "main".into(),
            DatabaseBinding {
                name: "shop".into(),
                server: "edge".into(),
                peer_key: None,
            },
        );
        let databases = |command: &Command| match command {
            Command::Deploy { databases, .. } => databases.clone(),
            _ => unreachable!(),
        };
        let mut empty = f.decode_command(json!({"command":"deploy","databases":{}}));
        // Unchanged, the policy alone refuses it: it would drop the binding.
        assert_eq!(f.authorize(&empty), Err(DATABASES_DENIED));
        keep_database_bindings(&f.permissions, &mut empty);
        assert_eq!(databases(&empty), None);
        assert_eq!(f.authorize(&empty), Ok(()));

        // A different, non-empty set is still refused.
        let other = json!({"main": {"name": "shop", "server": "other"}});
        let mut moved = f.decode_command(json!({"command":"deploy","databases":other}));
        keep_database_bindings(&f.permissions, &mut moved);
        assert!(databases(&moved).is_some());
        assert_eq!(f.authorize(&moved), Err(DATABASES_DENIED));

        // A full client's empty set still removes the bindings.
        let mut full = f.decode_command(json!({"command":"deploy","databases":{}}));
        keep_database_bindings(&Permissions::Full, &mut full);
        assert_eq!(databases(&full), Some(HashMap::new()));
    }

    #[test]
    fn scoped_release_commands_keep_provisioned_bindings_without_their_keys() {
        let mut f = Fixture::new();
        f.databases.insert(
            "main".into(),
            DatabaseBinding {
                name: "shop".into(),
                server: "edge".into(),
                peer_key: Some("issued-by-edge".into()),
            },
        );
        // Deployers without administration never see peer keys.
        let same = json!({"main": {"name": "shop", "server": "edge"}});
        let release = f.decode_command(json!({"command":"run_release","databases":same}));
        assert_eq!(f.authorize(&release), Ok(()));
        let moved = json!({"main": {"name": "shop", "server": "other"}});
        let release = f.decode_command(json!({"command":"run_release","databases":moved}));
        assert_eq!(f.authorize(&release), Err(DATABASES_DENIED));
        // Nor can they replace the issued key, on a release or a deploy.
        let rekeyed = json!({"main": {"name": "shop", "server": "edge", "peer_key": "guessed"}});
        let release = f.decode_command(json!({"command":"run_release","databases":rekeyed}));
        assert_eq!(f.authorize(&release), Err(DATABASES_DENIED));
        let deploy = f.decode_command(json!({"command":"deploy","databases":rekeyed}));
        assert_eq!(f.authorize(&deploy), Err(DATABASES_DENIED));
        // The issued key itself is accepted.
        let issued =
            json!({"main": {"name": "shop", "server": "edge", "peer_key": "issued-by-edge"}});
        let deploy = f.decode_command(json!({"command":"deploy","databases":issued}));
        assert_eq!(f.authorize(&deploy), Ok(()));
    }

    /// Everything else in an app environment needs full access, including
    /// backups, scaling, deletion, workflow commands and logs.
    const DEPLOY_CLIENT_COMMANDS: &[&str] = &[
        "prepare_release",
        "prepare_release_upload",
        "prepare_deploy",
        "cleanup_prepared_deploy",
        "cleanup_release",
        "finalize_release",
        "run_release",
        "deploy",
        "snapshot_database",
        "status",
        "list_releases",
        "rollback",
        "secrets_digest",
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
            let expected = if DEPLOY_CLIENT_COMMANDS.contains(name) {
                Ok(())
            } else {
                Err(DEPLOY_ONLY)
            };
            assert_eq!(f.authorize(&command), expected, "{name}");
            assert_eq!(
                authorize_command(&Permissions::Full, Some(&f.scope), &command, &f.context()),
                Ok(()),
                "{name}"
            );
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
    fn public_worker_impersonation_is_denied_even_for_full_clients() {
        let f = Fixture::new();
        let admin = Permissions::Full;
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
    fn deploy_clients_have_no_server_or_unlisted_deployment_authority() {
        let f = Fixture::new();
        let network_admin = Permissions::Deploy(Default::default());
        let admin = Permissions::Full;
        for command in [
            Command::List,
            Command::Routes,
            Command::DrainServer {
                timeout_seconds: 30,
            },
            Command::ResumeServer,
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
                authorize_command(
                    &Permissions::Deploy(Default::default()),
                    None,
                    &command,
                    &f.context()
                )
                .is_ok()
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
                &Permissions::Deploy(Default::default()),
                Some(&f.scope),
                &command,
                &f.context()
            ),
            Err(SCOPE_DENIED)
        );
        let invalid = Permissions::Deploy(
            (0..1025)
                .map(|index| format!("app{index}/prod").parse().unwrap())
                .collect(),
        );
        assert!(authorize_command(&invalid, None, &Command::ServerInfo, &f.context()).is_err());
    }

    #[test]
    fn deploy_clients_may_make_the_first_deploy_and_change_routes_ssl_and_plain_bindings() {
        let f = Fixture::new();
        let missing = PolicyContext {
            data_dir: f.dir.path(),
            app: None,
        };
        for name in ["prepare_deploy", "deploy"] {
            let command = f.decode_command(json!({
                "command": name,
                "routes": ["new.example.test", "*.example.test"],
                "ssl": {"provider": "cloudflare"},
                "backup": backup(),
                "storages": {"files": storage(), "media": s3()},
            }));
            assert_eq!(f.authorize(&command), Ok(()), "{name}");
            assert_eq!(
                authorize_command(&f.permissions, Some(&f.scope), &command, &missing),
                Ok(()),
                "{name}: no provisioned state"
            );
        }
        // Database bindings need grants, which only full clients make.
        let bound = f.decode_command(
            json!({"command":"deploy","databases":{"main":{"name":"shop","server":"edge"}}}),
        );
        assert_eq!(
            authorize_command(&f.permissions, Some(&f.scope), &bound, &missing),
            Err(STATE_REQUIRED)
        );
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
        // Confinement applies to every client; full access reaches every command.
        let mut f = Fixture::new();
        f.permissions = Permissions::Full;
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
            // Confinement applies to every client; full access reaches every command.
            let mut f = Fixture::new();
            f.permissions = Permissions::Full;
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

    fn storage() -> StorageBinding {
        serde_json::from_value(
            json!({"provider":"local","path":"storage/files","signing_key":"key"}),
        )
        .unwrap()
    }

    fn s3() -> StorageBinding {
        serde_json::from_value(json!({
            "provider":"s3","resource":"media","bucket":"demo-media","endpoint":"https://storage.example.test"
        }))
        .unwrap()
    }

    fn backup() -> tako_core::BackupBinding {
        serde_json::from_value(json!({
            "storage":{"provider":"s3","resource":"backups","bucket":"demo-staging","endpoint":"https://storage.example.test"},
            "backup_keys":[],"retention_days":7
        })).unwrap()
    }

    #[test]
    fn deploy_client_local_storage_stays_inside_its_data_directory() {
        let f = Fixture::new();
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
            let command = f.decode_command(
                json!({"command":"deploy","storages":{"files":binding},"force":true}),
            );
            let error = f.authorize(&command).unwrap_err();
            assert!(error.to_string().starts_with("Local storage"), "{path}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn deploy_client_local_storage_cannot_follow_a_cross_app_symlink() {
        let f = Fixture::new();
        let root = f.dir.path().join("apps/demo/staging/data/app/storage");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(f.dir.path().join("apps/other/staging"), root.join("files"))
            .unwrap();
        let command = f.decode_command(json!({"command":"deploy","storages":{"files":storage()}}));
        assert!(
            f.authorize(&command)
                .unwrap_err()
                .to_string()
                .starts_with("Local storage")
        );
    }

    #[test]
    fn full_clients_can_provision_but_never_bypass_path_confinement() {
        let mut f = Fixture::new();
        f.permissions = Permissions::Full;
        let command = f.decode_command(json!({"command":"deploy", "routes":["new.example.test"],
            "ssl":{"provider":"cloudflare","cloudflare_api_token":"admin-token"},
            "storages":{"files":storage()}, "backup":backup()}));
        assert_eq!(f.authorize(&command), Ok(()));
        assert_eq!(
            f.authorize(&f.decode_command(
                json!({"command":"deploy","path":f.release("other/staging"),"force":true})
            )),
            Err(PATH_DENIED)
        );
    }
}

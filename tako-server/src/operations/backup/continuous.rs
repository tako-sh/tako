use base64::{Engine, engine::general_purpose::STANDARD};
use object_store::{ObjectStore, path::Path as ObjectPath};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tako_backup::{Spool, encryption::Key};
use tako_core::{
    BackupBinding, DatabaseBackupStatus, StorageBinding, database_replica_id as id,
    same_backup_destination as same_destination,
};

#[derive(Clone)]
struct Declaration {
    database: String,
    shared: bool,
    consumers: Vec<String>,
    binding: BackupBinding,
}

#[derive(Serialize, Deserialize)]
struct Record {
    storage: StorageBinding,
    data_key: Option<String>,
    uploaded_at: Option<i64>,
}

struct Running {
    child: tokio::process::Child,
    _lease: File,
    remote: Arc<dyn ObjectStore>,
    prefix: ObjectPath,
    record: Record,
    binding: BackupBinding,
    next_upload: Instant,
    next_prune: Instant,
}

fn replica_prefix(network: &str, server: &str, id: &str) -> ObjectPath {
    // Server names are unique within a network. Hash the exact name instead
    // of sanitizing it, so punctuation cannot merge otherwise distinct names.
    let namespace = hex::encode(Sha256::digest(format!("{network}\0{server}")));
    ObjectPath::from(format!("_tako/sqlite/{namespace}/{id}"))
}

#[derive(Default)]
pub(crate) struct DatabaseReplicas {
    running: BTreeMap<String, Running>,
    retry_at: BTreeMap<String, (Instant, Duration)>,
}

fn add(
    declarations: &mut BTreeMap<String, Declaration>,
    app: &str,
    binding: BackupBinding,
) -> Result<(), String> {
    if binding.databases.is_empty() && binding.shared_databases.is_empty() {
        return Ok(());
    }
    super::validate_backup_binding(&binding)?;
    for (shared, databases) in [
        (false, &binding.databases),
        (true, &binding.shared_databases),
    ] {
        for database in databases {
            let id = id(app, database, shared);
            if let Some(existing) = declarations.get_mut(&id) {
                if !same_destination(&existing.binding.storage, &binding.storage) {
                    return Err(format!(
                        "Shared database {database:?} already uses another backup storage destination (apps: {}).",
                        existing.consumers.join(", ")
                    ));
                }
                for key in &binding.backup_keys {
                    if let Some(other) = existing
                        .binding
                        .backup_keys
                        .iter()
                        .find(|other| other.id == key.id)
                    {
                        if other != key {
                            return Err("Backup key id has conflicting values.".into());
                        }
                    } else {
                        existing.binding.backup_keys.push(key.clone());
                    }
                }
                existing.consumers.push(app.to_owned());
            } else {
                declarations.insert(
                    id,
                    Declaration {
                        database: database.clone(),
                        shared,
                        consumers: vec![app.to_owned()],
                        binding: binding.clone(),
                    },
                );
            }
        }
    }
    Ok(())
}

fn lease(spool: &Path) -> Result<File, String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(spool)
        .map_err(|e| e.to_string())?;
    let lease = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(spool.join("owner.lock"))
        .map_err(|e| e.to_string())?;
    lease.try_lock().map_err(|_| {
        "Another server process owns this database's backup stream. Retry after it stops."
            .to_string()
    })?;
    Ok(lease)
}

impl crate::ServerState {
    fn declarations(
        &self,
        replacement: Option<(&str, Option<&BackupBinding>)>,
    ) -> Result<BTreeMap<String, Declaration>, String> {
        let mut declarations = BTreeMap::new();
        for app in self.state_store.load_apps().map_err(|e| e.to_string())? {
            let app = app.config.deployment_id();
            if replacement.is_some_and(|(name, _)| name == app) {
                continue;
            }
            if let Some(binding) = self
                .state_store
                .get_backup(&app)
                .map_err(|e| e.to_string())?
            {
                add(&mut declarations, &app, binding)?;
            }
        }
        if let Some((app, Some(binding))) = replacement {
            add(&mut declarations, app, binding.clone())?;
        }
        Ok(declarations)
    }

    /// Checked before accepting a deploy, so a second app cannot silently fork
    /// an existing shared stream or redirect its uploads.
    pub(crate) fn validate_database_registration(
        &self,
        app: &str,
        binding: Option<&BackupBinding>,
    ) -> Result<(), String> {
        for (id, declaration) in self.declarations(Some((app, binding)))? {
            if let Some(record) = self.replica_record(&id)? {
                if !same_destination(&record.storage, &declaration.binding.storage) {
                    return Err(format!(
                        "Database {:?} has an existing replica in another storage destination. Keep its destination to preserve recovery history.",
                        declaration.database
                    ));
                }
            }
        }
        Ok(())
    }

    fn replica_record(&self, id: &str) -> Result<Option<Record>, String> {
        self.state_store
            .get_database_replica(id)
            .map_err(|e| e.to_string())?
            .map(|json| {
                serde_json::from_slice(&json).map_err(|e| format!("read database replica: {e}"))
            })
            .transpose()
    }

    fn save_replica(&self, id: &str, record: &Record) -> Result<(), String> {
        self.state_store
            .set_database_replica(id, &serde_json::to_vec(record).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    }

    fn spool(&self, id: &str) -> PathBuf {
        self.runtime.data_dir.join("database-replicas").join(id)
    }

    fn source_root(&self, declaration: &Declaration) -> PathBuf {
        let paths = crate::release::app_runtime_data_paths(
            &self.runtime.data_dir,
            &declaration.consumers[0],
        );
        if declaration.shared {
            paths.shared
        } else {
            paths.app
        }
    }

    async fn connect_replica(
        &self,
        id: &str,
        declaration: &Declaration,
    ) -> Result<(Arc<dyn ObjectStore>, ObjectPath, Record), String> {
        let saved = self.replica_record(id)?;
        if saved
            .as_ref()
            .is_some_and(|record| !same_destination(&record.storage, &declaration.binding.storage))
        {
            return Err("Database replica destination conflicts with its registration.".into());
        }
        let recovery = declaration
            .binding
            .backup_keys
            .iter()
            .map(|key| {
                let bytes = STANDARD
                    .decode(&key.key_base64)
                    .map_err(|e| e.to_string())?;
                Key::new(
                    key.id.clone(),
                    bytes.try_into().map_err(|_| "Invalid backup key length")?,
                )
                .map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, String>>()?;
        let saved_key = saved
            .as_ref()
            .and_then(|record| record.data_key.as_ref())
            .map(|data_key| {
                STANDARD
                    .decode(data_key)
                    .map_err(|e| e.to_string())?
                    .try_into()
                    .map_err(|_| "Invalid replica data key length".to_string())
            })
            .transpose()?;
        let network = match std::fs::symlink_metadata(
            self.runtime
                .data_dir
                .join(tako_network::node_config::NODE_CONFIG_FILE),
        ) {
            Ok(_) => tako_network::NodeConfig::load(&self.runtime.data_dir)
                .map_err(|e| e.to_string())?
                .network
                .id
                .to_string(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "standalone".into(),
            Err(error) => return Err(error.to_string()),
        };
        let server = self
            .runtime
            .server_name
            .as_deref()
            .or(self.runtime.server_identity.as_deref())
            .unwrap_or("server");
        let prefix = replica_prefix(&network, server, id);
        let inner = Arc::new(super::objects::backup_store(&declaration.binding.storage)?);
        let (remote, key) = tako_backup::replica::connect(inner, &prefix, &recovery, saved_key)
            .await
            .map_err(|e| e.to_string())?;
        let record = Record {
            storage: declaration.binding.storage.clone(),
            data_key: Some(STANDARD.encode(key.bytes())),
            uploaded_at: saved.and_then(|record| record.uploaded_at),
        };
        self.save_replica(id, &record)?;
        Ok((remote, prefix, record))
    }

    pub(crate) fn start_database_backups(self: Arc<Self>, handle: &tokio::runtime::Handle) {
        handle.spawn(async move {
            let mut ticks = tokio::time::interval(Duration::from_secs(10));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                if let Err(error) = self.reconcile_database_backups().await {
                    tracing::error!(%error, "Could not reconcile database backups");
                }
            }
        });
    }

    async fn reconcile_database_backups(&self) -> Result<(), String> {
        let declarations = self.declarations(None)?;
        let mut runtime = self.database_replicas.lock().await;
        let stale: Vec<_> = runtime
            .running
            .keys()
            .filter(|id| !declarations.contains_key(*id) || self.is_draining())
            .cloned()
            .collect();
        for id in stale {
            runtime.stop(&id).await?;
            if let Some(status) = self.database_replica_status.write().get_mut(&id) {
                status.active = false;
            }
        }
        self.database_replica_status
            .write()
            .retain(|id, _| declarations.contains_key(id));
        if self.is_draining() {
            return Ok(());
        }
        for (id, declaration) in declarations {
            let mut status =
                self.database_replica_status
                    .write()
                    .remove(&id)
                    .unwrap_or(DatabaseBackupStatus {
                        database: declaration.database.clone(),
                        shared: declaration.shared,
                        replica_id: id.clone(),
                        consumers: declaration.consumers.clone(),
                        active: false,
                        captured_txid: None,
                        uploaded_txid: None,
                        uploaded_at_unix_secs: None,
                        error: None,
                    });
            status.consumers = declaration.consumers.clone();
            if runtime
                .retry_at
                .get(&id)
                .is_some_and(|(at, _)| Instant::now() < *at)
            {
                self.database_replica_status.write().insert(id, status);
                continue;
            }
            let result = tokio::time::timeout(
                Duration::from_secs(120),
                self.replica_tick(&id, &declaration, &mut runtime, &mut status),
            )
            .await
            .map_err(|_| "Database backup operation timed out".to_string())
            .and_then(|result| result);
            match result {
                Ok(()) => {
                    status.error = None;
                    runtime.retry_at.remove(&id);
                }
                Err(error) => {
                    let delay = runtime
                        .retry_at
                        .get(&id)
                        .map_or(tako_backup::BACKUP_INTERVAL, |(_, delay)| {
                            (*delay * 2).min(tako_backup::MAX_RETRY_INTERVAL)
                        });
                    runtime
                        .retry_at
                        .insert(id.clone(), (Instant::now() + delay, delay));
                    status.error = Some(error);
                }
            }
            status.active = runtime.running.contains_key(&id);
            self.database_replica_status.write().insert(id, status);
        }
        Ok(())
    }

    async fn replica_tick(
        &self,
        id: &str,
        declaration: &Declaration,
        runtime: &mut DatabaseReplicas,
        status: &mut DatabaseBackupStatus,
    ) -> Result<(), String> {
        if declaration
            .consumers
            .iter()
            .any(|app| self.unresolved_restore_block(app).is_some())
        {
            runtime.stop(id).await?;
            return Err("Database backup paused by an unfinished restore".into());
        }
        let spool = Spool::new(self.spool(id));
        let changed = runtime
            .running
            .get(id)
            .is_some_and(|running| running.binding != declaration.binding);
        if changed {
            runtime.stop(id).await?;
        }
        if let Some(running) = runtime.running.get_mut(id) {
            if let Some(exit) = running.child.try_wait().map_err(|e| e.to_string())? {
                let error =
                    std::fs::read_to_string(spool.root().join("worker-error")).unwrap_or_default();
                runtime.stop(id).await?;
                return Err(format!("SQLite capture exited ({exit}): {}", error.trim()));
            }
        }
        if !runtime.running.contains_key(id) {
            let lease = lease(spool.root())?;
            crate::replica_worker::validate_source(
                &self.source_root(declaration),
                &declaration.database,
            )?;
            let (remote, prefix, record) = self.connect_replica(id, declaration).await?;
            // Download history only when bootstrapping a spool. Normal idle ticks do no remote reads.
            if !spool.root().join("initialized").exists() {
                if !tako_backup::recovery_points(remote.as_ref(), &prefix)
                    .await
                    .map_err(|e| e.to_string())?
                    .is_empty()
                {
                    return Err("Remote database history exists but the local spool is missing. Restore a capture with tako backups restore --database before resuming backups.".into());
                }
                File::create(spool.root().join("initialized"))
                    .and_then(|file| file.sync_all())
                    .map_err(|e| e.to_string())?;
                File::open(spool.root())
                    .and_then(|file| file.sync_all())
                    .map_err(|e| e.to_string())?;
            }
            let child = crate::replica_worker::spawn(
                &self.source_root(declaration),
                &declaration.database,
                spool.root(),
            )?;
            runtime.running.insert(
                id.to_owned(),
                Running {
                    child,
                    _lease: lease,
                    remote,
                    prefix,
                    record,
                    binding: declaration.binding.clone(),
                    next_upload: Instant::now(),
                    next_prune: Instant::now(),
                },
            );
        }
        let running = runtime.running.get_mut(id).unwrap();
        (status.captured_txid, status.uploaded_txid) =
            tako_backup::positions(&spool).map_err(|e| e.to_string())?;
        status.uploaded_at_unix_secs = running.record.uploaded_at;
        if Instant::now() < running.next_upload {
            return Ok(());
        }
        let result = async {
            let uploaded =
                tako_backup::upload_pending(&spool, running.remote.as_ref(), &running.prefix)
                    .await
                    .map_err(|e| e.to_string())?;
            if uploaded > 0 {
                running.record.uploaded_at = Some(super::now_unix_secs());
                self.save_replica(id, &running.record)?;
            }
            if Instant::now() >= running.next_prune {
                tako_backup::prune_expired(
                    &spool,
                    running.remote.as_ref(),
                    &running.prefix,
                    SystemTime::now(),
                )
                .await
                .map_err(|e| e.to_string())?;
                running.next_prune = Instant::now() + tako_backup::RETENTION_INTERVAL;
            }
            Ok(())
        }
        .await;
        // The outer reconciliation loop owns error backoff. Keep this deadline
        // unchanged on failure so a retry always attempts the failed upload.
        if result.is_ok() {
            running.next_upload = Instant::now() + tako_backup::BACKUP_INTERVAL;
        }
        (status.captured_txid, status.uploaded_txid) =
            tako_backup::positions(&spool).map_err(|e| e.to_string())?;
        status.uploaded_at_unix_secs = running.record.uploaded_at;
        result
    }

    pub(super) async fn database_backup_status(&self, app: &str) -> Vec<DatabaseBackupStatus> {
        self.database_replica_status
            .read()
            .values()
            .filter(|status| status.consumers.iter().any(|consumer| consumer == app))
            .cloned()
            .collect()
    }

    pub(super) async fn pause_app_database_backups(
        &self,
        app: &str,
        runtime: &mut DatabaseReplicas,
    ) -> Result<Vec<(File, tako_backup::CaptureLock)>, String> {
        let mut leases = Vec::new();
        for (id, declaration) in self.declarations(None)? {
            if !declaration.shared && declaration.consumers.iter().any(|consumer| consumer == app) {
                let owner = match runtime.stop(&id).await? {
                    Some(lease) => lease,
                    None => lease(&self.spool(&id))?,
                };
                let capture =
                    tako_backup::lock_capture(&self.spool(&id)).map_err(|e| e.to_string())?;
                leases.push((owner, capture));
                if let Some(status) = self.database_replica_status.write().get_mut(&id) {
                    status.active = false;
                }
            }
        }
        Ok(leases)
    }

    fn declared_database(
        &self,
        app: &str,
        database: &str,
        shared: bool,
    ) -> Result<(String, Declaration), String> {
        tako_core::validate_database_paths(&[database.to_owned()])?;
        let id = id(app, database, shared);
        let declaration = self
            .declarations(None)?
            .remove(&id)
            .filter(|declaration| declaration.consumers.iter().any(|consumer| consumer == app))
            .ok_or("Database is not declared in this app's backup configuration.")?;
        Ok((id, declaration))
    }

    pub(crate) async fn list_database_backups(
        &self,
        app: &str,
        database: &str,
        shared: bool,
    ) -> tako_core::Response {
        let result = async {
            let (id, declaration) = self.declared_database(app, database, shared)?;
            let runtime = self.database_replicas.lock().await;
            let _lease;
            let (remote, prefix) = if let Some(running) = runtime.running.get(&id) {
                (running.remote.clone(), running.prefix.clone())
            } else {
                _lease = lease(&self.spool(&id))?;
                let (remote, prefix, _) = self.connect_replica(&id, &declaration).await?;
                (remote, prefix)
            };
            let points = tako_backup::recovery_points(remote.as_ref(), &prefix)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|point| tako_core::DatabaseRecoveryPoint {
                    txid: point.txid,
                    uploaded_at_unix_secs: point.uploaded_at_unix_secs,
                    size_bytes: point.size_bytes,
                })
                .collect();
            Ok::<_, String>(tako_core::DatabaseBackupsResponse {
                database: database.into(),
                shared,
                replica_id: id,
                points,
            })
        }
        .await;
        match result {
            Ok(result) => tako_core::Response::ok(result),
            Err(error) => tako_core::Response::error(error),
        }
    }

    pub(crate) async fn restore_database_backup(
        &self,
        app: &str,
        database: &str,
        shared: bool,
        target: Option<u64>,
    ) -> tako_core::Response {
        match self
            .restore_database_inner(app, database, shared, target)
            .await
        {
            Ok(result) => tako_core::Response::ok(result),
            Err(error) => tako_core::Response::error(error),
        }
    }

    async fn restore_database_inner(
        &self,
        app: &str,
        database: &str,
        shared: bool,
        target: Option<u64>,
    ) -> Result<tako_core::DatabaseRestoreResponse, String> {
        let (id, declaration) = self.declared_database(app, database, shared)?;
        let _standby = self.lock_out_standby()?;
        let mut runtime = self.database_replicas.lock().await;
        let _lease = match runtime.stop(&id).await? {
            Some(lease) => lease,
            None => lease(&self.spool(&id))?,
        };
        let _capture = tako_backup::lock_capture(&self.spool(&id)).map_err(|e| e.to_string())?;
        if let Some(status) = self.database_replica_status.write().get_mut(&id) {
            status.active = false;
        }
        let _backup = self.acquire_backup_lock(app)?;
        let deploy_lock = self.get_deploy_lock(app).await;
        let _deploy = deploy_lock.lock().await;
        let data_root = if shared {
            self.runtime.data_dir.join("shared/data")
        } else {
            crate::release::app_runtime_data_paths(&self.runtime.data_dir, app).root
        };
        super::journal::refuse_unfinished_restores(&data_root)?;
        super::snapshot::refuse_symlinked_data_root(&data_root)?;
        let (remote, prefix, _) = self.connect_replica(&id, &declaration).await?;
        let temporary = self.runtime.data_dir.join("tmp/restore");
        tokio::fs::create_dir_all(&temporary)
            .await
            .map_err(|e| e.to_string())?;
        let work = tempfile::Builder::new()
            .prefix("sqlite-")
            .tempdir_in(&temporary)
            .map_err(|e| e.to_string())?;
        let _work_lock = super::journal::lock_work_dir(work.path()).map_err(|e| e.to_string())?;
        let relative = Path::new("app").join(database);
        let staged = work
            .path()
            .join(super::DATABASE_SNAPSHOT_ARCHIVE_ROOT)
            .join(&relative);
        std::fs::create_dir_all(staged.parent().unwrap()).map_err(|e| e.to_string())?;
        let txid = tako_backup::restore_remote(remote.as_ref(), &prefix, &staged, target)
            .await
            .map_err(|e| e.to_string())?;
        // Retain the remote high-water mark even when restoring an older point.
        // The next full capture must receive a new, larger TXID.
        let spool = Spool::new(self.spool(&id));
        tako_backup::download_replica(remote.as_ref(), &prefix, &spool)
            .await
            .map_err(|e| e.to_string())?;
        let affected_apps = if shared {
            self.app_manager.list_apps()
        } else {
            vec![app.to_owned()]
        };
        if shared {
            self.apply_shared_database_restore(
                work.path().join(super::DATABASE_SNAPSHOT_ARCHIVE_ROOT),
                relative,
                data_root,
            )
            .await?;
        } else {
            self.apply_staged_restore(
                app,
                super::StagedRestore::Databases(vec![relative]),
                work.path(),
            )
            .await?;
        }
        File::create(spool.root().join("initialized"))
            .and_then(|file| file.sync_all())
            .map_err(|e| e.to_string())?;
        File::open(spool.root())
            .and_then(|file| file.sync_all())
            .map_err(|e| e.to_string())?;
        runtime.retry_at.remove(&id);
        Ok(tako_core::DatabaseRestoreResponse {
            database: database.into(),
            shared,
            replica_id: id,
            txid,
            affected_apps,
        })
    }

    pub(super) async fn apply_shared_database_restore(
        &self,
        staged_root: PathBuf,
        relative: PathBuf,
        data_root: PathBuf,
    ) -> Result<(), String> {
        // The declaration set is not an access-control list. Every app can
        // open shared files, including a process that has not opened one yet.
        let (_operation, deadline) = self.prepare_server_drain(120).map_err(response_error)?;
        let report = self.finish_server_drain(deadline).await;
        if !matches!(&report, tako_core::Response::Ok { data } if data.get("completed").and_then(|v| v.as_bool()) == Some(true))
        {
            return Err(format!(
                "Server remains drained; could not stop every shared-data consumer: {}",
                response_error(report)
            ));
        }
        let _exclusive = self
            .state_store
            .try_drain_activity(true)
            .map_err(|e| e.to_string())?
            .ok_or("Server remains drained; another process still has active apps")?;
        let swap = super::journal::swap_databases_off_runtime(
            staged_root,
            vec![relative],
            data_root,
            super::RESTORE_WORKER_TIMEOUT,
        )
        .await;
        drop(_exclusive);
        let swap = match swap {
            Ok(swap) => swap,
            Err(error) if error.originals_intact => {
                self.restore_drained_runtime(true)
                    .await
                    .map_err(|resume| format!("{}; {resume}", error.message))?;
                return Err(error.message);
            }
            Err(error) => return Err(format!("{}; server left drained", error.message)),
        };
        // Mark a complete swap as safe to open while keeping its originals.
        swap.prepare_restart()?;
        if let Err(restart) = self.restore_drained_runtime(true).await {
            let report = self.retry_runtime_drain().await;
            if !report.completed {
                let kept = swap.abandon(&restart);
                return Err(format!(
                    "{restart}; could not stop restored apps; original database kept in {}",
                    kept.display()
                ));
            }
            swap.rollback()?;
            self.restore_drained_runtime(true)
                .await
                .map_err(|error| format!("{restart}; original restored; {error}"))?;
            return Err(format!("{restart}; original database restored"));
        }
        swap.commit()?;

        Ok(())
    }
}

fn response_error(response: tako_core::Response) -> String {
    match response {
        tako_core::Response::Error { message, .. } => message,
        tako_core::Response::Ok { data } => data.to_string(),
    }
}

impl DatabaseReplicas {
    async fn stop(&mut self, id: &str) -> Result<Option<File>, String> {
        if let Some(mut running) = self.running.remove(id) {
            if running
                .child
                .try_wait()
                .map_err(|e| e.to_string())?
                .is_none()
            {
                running
                    .child
                    .kill()
                    .await
                    .map_err(|e| format!("stop SQLite capture: {e}"))?;
            }
            Ok(Some(running._lease))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replica_namespaces_separate_networks_and_exact_server_names() {
        let first = replica_prefix("network-a", "la.1", "database");
        assert_eq!(first, replica_prefix("network-a", "la.1", "database"));
        assert_ne!(first, replica_prefix("network-b", "la.1", "database"));
        assert_ne!(first, replica_prefix("network-a", "la-1", "database"));
        assert_ne!(
            first,
            replica_prefix("network-a", "la.1", "another-database")
        );
    }
    fn binding(bucket: &str) -> BackupBinding {
        serde_json::from_value(serde_json::json!({
            "storage": {"provider":"s3", "bucket":bucket, "endpoint":"https://s3.example.test", "region":"local", "access_key_id":"key", "secret_access_key":"secret"},
            "backup_keys":[{"id":"backup-key-0123456789abcdef", "key_base64":STANDARD.encode([1;32])}],
            "shared_databases":["shared.sqlite"]
        })).unwrap()
    }
    #[test]
    fn declarations_deduplicate_consumers_and_reject_conflicting_destinations() {
        let mut declarations = BTreeMap::new();
        add(&mut declarations, "a/production", binding("backups")).unwrap();
        let mut second = binding("backups");
        second.backup_keys[0].id = "backup-key-fedcba9876543210".into();
        second.backup_keys[0].key_base64 = STANDARD.encode([2; 32]);
        add(&mut declarations, "b/production", second).unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations.values().next().unwrap().consumers.len(), 2);
        assert_eq!(
            declarations
                .values()
                .next()
                .unwrap()
                .binding
                .backup_keys
                .len(),
            2
        );
        assert!(add(&mut declarations, "c/production", binding("another-bucket")).is_err());
        let mut remaining = BTreeMap::new();
        add(&mut remaining, "b/production", binding("backups")).unwrap();
        assert_eq!(declarations.keys().next(), remaining.keys().next());
        assert_ne!(
            id("a/production", "shared.sqlite", false),
            id("b/production", "shared.sqlite", false)
        );
    }
    #[test]
    fn ownership_lock_excludes_another_server_process() {
        let temp = tempfile::tempdir().unwrap();
        let first = lease(temp.path()).unwrap();
        assert!(lease(temp.path()).is_err());
        drop(first);
        assert!(lease(temp.path()).is_ok());
    }
}

use std::collections::HashMap;

use super::{SqliteStateStore, StateStoreError, encryption};
use rusqlite::{OptionalExtension, TransactionBehavior};
use tako_core::{BackupBinding, StorageBinding, database_replica_id, same_backup_destination};

#[derive(serde::Deserialize)]
struct ReplicaDestination {
    storage: StorageBinding,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(bucket: &str) -> BackupBinding {
        serde_json::from_value(serde_json::json!({
            "storage": {"provider":"s3", "bucket":bucket, "endpoint":"https://s3.example.test", "region":"local", "access_key_id":"key", "secret_access_key":"secret"},
            "backup_keys": [{"id":"backup-key-0123456789abcdef", "key_base64":"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE="}],
            "shared_databases": ["shared.sqlite"]
        })).unwrap()
    }

    #[test]
    fn concurrent_deployments_cannot_register_conflicting_shared_destinations() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.sqlite");
        let store = SqliteStateStore::new(path.clone(), [7; 32]);
        store.init().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|name| {
                let barrier = barrier.clone();
                let store = SqliteStateStore::new(path.clone(), [7; 32]);
                std::thread::spawn(move || {
                    barrier.wait();
                    store.commit_deployment(
                        &crate::instances::AppConfig {
                            name: name.into(),
                            environment: "production".into(),
                            version: "v1".into(),
                            ..Default::default()
                        },
                        &[],
                        &HashMap::new(),
                        None,
                        Some(&binding(name)),
                        None,
                    )
                })
            })
            .collect();
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        assert!(
            results
                .iter()
                .find_map(|r| r.as_ref().err())
                .unwrap()
                .to_string()
                .contains("another backup storage destination")
        );
        assert_eq!(store.load_apps().unwrap().len(), 1);
    }

    #[test]
    fn removing_consumers_preserves_replica_identity_and_destination() {
        let temp = tempfile::tempdir().unwrap();
        let store = SqliteStateStore::new(temp.path().join("state.sqlite"), [7; 32]);
        store.init().unwrap();
        let original = binding("backups");
        store.set_backup("a/production", Some(&original)).unwrap();
        store.set_backup("b/production", Some(&original)).unwrap();
        let id = database_replica_id("a/production", "shared.sqlite", true);
        let record = store.get_database_replica(&id).unwrap().unwrap();
        store.delete_app("a", "production").unwrap();
        assert!(store.get_backup("b/production").unwrap().is_some());
        store.delete_app("b", "production").unwrap();
        assert_eq!(store.get_database_replica(&id).unwrap().unwrap(), record);
        assert!(
            store
                .set_backup("c/production", Some(&binding("elsewhere")))
                .is_err()
        );
        let mut rotated = original;
        rotated.storage.access_key_id = Some("rotated".into());
        store.set_backup("c/production", Some(&rotated)).unwrap();
        assert_eq!(store.get_database_replica(&id).unwrap().unwrap(), record);
    }
}

impl SqliteStateStore {
    pub(crate) fn get_database_replica(
        &self,
        id: &str,
    ) -> Result<Option<Vec<u8>>, StateStoreError> {
        self.get_encrypted_row("database_replicas", id)
    }

    pub(crate) fn set_database_replica(
        &self,
        id: &str,
        json: &[u8],
    ) -> Result<(), StateStoreError> {
        self.set_encrypted_row("database_replicas", id, json)
    }

    pub fn get_storages(
        &self,
        app: &str,
    ) -> Result<HashMap<String, tako_core::StorageBinding>, StateStoreError> {
        match self.get_encrypted_row("app_storages", app)? {
            Some(json) => serde_json::from_slice(&json)
                .map_err(|e| StateStoreError::InvalidData(format!("deserialize storages: {e}"))),
            None => Ok(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub fn set_ssl(&self, app: &str, ssl: &tako_core::SslBinding) -> Result<(), StateStoreError> {
        let json = serde_json::to_vec(ssl)
            .map_err(|e| StateStoreError::InvalidData(format!("serialize ssl: {e}")))?;
        self.set_encrypted_row("app_ssl", app, &json)
    }

    pub fn get_ssl(&self, app: &str) -> Result<Option<tako_core::SslBinding>, StateStoreError> {
        match self.get_encrypted_row("app_ssl", app)? {
            Some(json) => serde_json::from_slice(&json)
                .map(Some)
                .map_err(|e| StateStoreError::InvalidData(format!("deserialize ssl: {e}"))),
            None => Ok(None),
        }
    }

    pub fn set_backup(
        &self,
        app: &str,
        backup: Option<&tako_core::BackupBinding>,
    ) -> Result<(), StateStoreError> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.register_database_destinations_on(&tx, app, backup)?;
        match backup {
            Some(backup) => {
                let json = serde_json::to_vec(backup).map_err(|e| {
                    StateStoreError::InvalidData(format!("serialize backup config: {e}"))
                })?;
                let encrypted = encryption::encrypt_blob(&self.encryption_key, &json)?;
                tx.execute("INSERT INTO app_backups (app, encrypted_data) VALUES (?1, ?2) ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data", (app, encrypted))?;
            }
            None => {
                tx.execute("DELETE FROM app_backups WHERE app = ?1", [app])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Called inside the deployment/config transaction. SQLite's write lock
    /// makes concurrent registrations agree before either can be published.
    pub(super) fn register_database_destinations_on(
        &self,
        conn: &rusqlite::Connection,
        app: &str,
        backup: Option<&BackupBinding>,
    ) -> Result<(), StateStoreError> {
        let Some(backup) = backup else {
            return Ok(());
        };
        tako_core::validate_database_paths(&backup.databases)
            .map_err(StateStoreError::InvalidData)?;
        tako_core::validate_database_paths(&backup.shared_databases)
            .map_err(StateStoreError::InvalidData)?;
        if !backup.shared_databases.is_empty() {
            let mut query =
                conn.prepare("SELECT encrypted_data FROM app_backups WHERE app != ?1")?;
            for row in query.query_map([app], |row| row.get::<_, Vec<u8>>(0))? {
                let bytes = encryption::decrypt_blob(&self.encryption_key, &row?)?;
                let other: BackupBinding = serde_json::from_slice(&bytes)
                    .map_err(|e| StateStoreError::InvalidData(e.to_string()))?;
                if !other
                    .shared_databases
                    .iter()
                    .any(|path| backup.shared_databases.contains(path))
                {
                    continue;
                }
                if !same_backup_destination(&backup.storage, &other.storage) {
                    return Err(StateStoreError::InvalidData(
                        "Shared database already uses another backup storage destination".into(),
                    ));
                }
                for key in &backup.backup_keys {
                    if other
                        .backup_keys
                        .iter()
                        .any(|existing| existing.id == key.id && existing != key)
                    {
                        return Err(StateStoreError::InvalidData(
                            "Backup key id has conflicting values".into(),
                        ));
                    }
                }
            }
        }
        for (shared, databases) in [(false, &backup.databases), (true, &backup.shared_databases)] {
            for database in databases {
                let id = database_replica_id(app, database, shared);
                let previous: Option<Vec<u8>> = conn
                    .query_row(
                        "SELECT encrypted_data FROM database_replicas WHERE app = ?1",
                        [&id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(previous) = previous {
                    let bytes = encryption::decrypt_blob(&self.encryption_key, &previous)?;
                    let previous: ReplicaDestination = serde_json::from_slice(&bytes)
                        .map_err(|e| StateStoreError::InvalidData(e.to_string()))?;
                    if !same_backup_destination(&previous.storage, &backup.storage) {
                        return Err(StateStoreError::InvalidData(format!(
                            "Database {database:?} has an existing replica in another storage destination"
                        )));
                    }
                } else {
                    // Reserve the destination at the successful deployment's
                    // commit point, before any asynchronous remote setup.
                    let record = serde_json::to_vec(&serde_json::json!({"storage": backup.storage, "data_key": null, "uploaded_at": null}))
                        .map_err(|e| StateStoreError::InvalidData(e.to_string()))?;
                    let encrypted = encryption::encrypt_blob(&self.encryption_key, &record)?;
                    conn.execute(
                        "INSERT INTO database_replicas (app, encrypted_data) VALUES (?1, ?2)",
                        (&id, encrypted),
                    )?;
                }
            }
        }
        Ok(())
    }

    pub fn get_backup(
        &self,
        app: &str,
    ) -> Result<Option<tako_core::BackupBinding>, StateStoreError> {
        match self.get_encrypted_row("app_backups", app)? {
            Some(json) => serde_json::from_slice(&json).map(Some).map_err(|e| {
                StateStoreError::InvalidData(format!("deserialize backup config: {e}"))
            }),
            None => Ok(None),
        }
    }
}

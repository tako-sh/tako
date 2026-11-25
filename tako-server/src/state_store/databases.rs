//! Databases (SQLite databases and KV stores) hosted on this server, the
//! database bindings of apps on this server, and grants for apps on other
//! servers.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{bytes, get, put, remove, scan, storage};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

use super::{StateStore, StateStoreError, encryption};

/// Secret settings of a hosted database.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DatabaseSecrets {
    pub backup_key_base64: String,
    #[serde(default)]
    pub backup: Option<tako_core::StorageBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredDatabase {
    /// Unique within the network; identifies the database.
    pub name: String,
    pub engine: tako_core::DatabaseEngine,
    /// Tells this database apart from earlier ones with the same name.
    pub generation: String,
    pub created_at_unix_secs: u64,
    pub secrets: DatabaseSecrets,
}

/// A database binding of an app on this server, with the token its
/// processes present. The token stays the same across deploys while the
/// binding names the same database, so instances of the previous release keep
/// working during a rolling update.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AppDatabase {
    pub binding: tako_core::DatabaseBinding,
    pub token: String,
}

/// The token authorizes SQL as the app, so it is never printed.
impl std::fmt::Debug for AppDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppDatabase")
            .field("binding", &self.binding)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// What this server grants an app environment that runs on other servers:
/// its databases here, and the key those servers present when forwarding.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DatabaseGrant {
    pub databases: BTreeSet<String>,
    pub key: String,
}

impl StateStore {
    pub(crate) fn replication_groups(
        &self,
    ) -> Result<BTreeMap<String, crate::databases::replication::GroupConfig>, StateStoreError> {
        self.all_database_rows("replication_groups")
    }
    pub(crate) fn withdrawal(&self, id: &str) -> Result<Option<bool>, StateStoreError> {
        self.read(|r| get(r, ("withdrawals", id, "")))
    }
    pub(crate) fn mark_withdrawing(&self, id: &str) -> Result<(), StateStoreError> {
        self.write(|r| put(r, ("withdrawals", id, ""), &true))
    }
    pub(crate) fn forget_replication_group(&self, id: &str) -> Result<(), StateStoreError> {
        self.write(|r| {
            remove(r, ("replication_groups", id, ""))?;
            if r.get(("withdrawals", id, "")).map_err(storage)?.is_some() {
                put(r, ("withdrawals", id, ""), &false)?
            }
            remove(r, ("presence", id, ""))
        })
    }
    pub(crate) fn mark_storage_present(&self, id: &str) -> Result<(), StateStoreError> {
        self.write(|r| put(r, ("presence", id, ""), &()))
    }
    pub(crate) fn storage_initialized(&self, id: &str) -> Result<bool, StateStoreError> {
        self.read(|r| Ok(r.get(("presence", id, "")).map_err(storage)?.is_some()))
    }
    pub(crate) fn claim_storage_bootstrap(&self, id: &str) -> Result<bool, StateStoreError> {
        self.write(|r| {
            Ok(r.insert(("bootstraps", id, ""), &[][..])
                .map_err(storage)?
                .is_none())
        })
    }
    pub(crate) fn insert_replication_group(
        &self,
        id: &str,
        config: &crate::databases::replication::GroupConfig,
    ) -> Result<(), StateStoreError> {
        self.insert_record("replication_groups", id, config)
    }
    fn insert_record(
        &self,
        bucket: &str,
        id: &str,
        record: &impl Serialize,
    ) -> Result<(), StateStoreError> {
        let value = self.encrypt_json(record)?;
        self.write(|r| {
            if r.insert((bucket, id, ""), value.as_slice())
                .map_err(storage)?
                .is_some()
            {
                return Err(StateStoreError::InvalidData(format!(
                    "{bucket} entry {id} already exists"
                )));
            }
            Ok(())
        })
    }
    pub(crate) fn release_databases(
        &self,
        app: &str,
        version: &str,
    ) -> Result<Option<HashMap<String, AppDatabase>>, StateStoreError> {
        self.read(|r| bytes(r, ("release_databases", app, version)))?
            .map(|v| self.decrypt_json(&v))
            .transpose()
    }
    pub(crate) fn releases_with_databases(&self) -> Result<Vec<(String, String)>, StateStoreError> {
        self.read(|r| {
            scan(r, "release_databases", |app, version, _| {
                Ok((app.to_owned(), version.to_owned()))
            })
        })
    }
    pub(crate) fn create_database(&self, database: &StoredDatabase) -> Result<(), StateStoreError> {
        self.insert_record("databases", &database.name, database)
    }
    pub(crate) fn list_databases(&self) -> Result<Vec<StoredDatabase>, StateStoreError> {
        self.read(|r| scan(r, "databases", |_, _, v| self.decrypt_json(v)))
    }
    pub(crate) fn get_database(
        &self,
        name: &str,
    ) -> Result<Option<StoredDatabase>, StateStoreError> {
        self.read(|r| bytes(r, ("databases", name, "")))?
            .map(|v| self.decrypt_json(&v))
            .transpose()
    }
    pub(crate) fn set_database_secrets(
        &self,
        name: &str,
        secrets: &DatabaseSecrets,
    ) -> Result<(), StateStoreError> {
        self.write(|r| {
            let key = ("databases", name, "");
            let value = bytes(r, key)?.ok_or_else(|| {
                StateStoreError::InvalidData(format!(
                    "database {name} is not hosted on this server"
                ))
            })?;
            let mut record: StoredDatabase = self.decrypt_json(&value)?;
            record.secrets = secrets.clone();
            r.insert(key, self.encrypt_json(&record)?.as_slice())
                .map_err(storage)?;
            Ok(())
        })
    }
    pub(crate) fn delete_database(&self, name: &str) -> Result<(), StateStoreError> {
        self.write(|r| remove(r, ("databases", name, "")))
    }
    pub(crate) fn get_app_databases(
        &self,
        app: &str,
    ) -> Result<HashMap<String, AppDatabase>, StateStoreError> {
        match self.get_encrypted_row("app_databases", app)? {
            Some(json) => serde_json::from_slice(&json).map_err(|e| {
                StateStoreError::InvalidData(format!("deserialize database bindings: {e}"))
            }),
            None => Ok(HashMap::new()),
        }
    }

    /// The app environment's channel replay binding, which a deploy of a
    /// release that uses channels commits.
    pub(crate) fn channel_credentials(
        &self,
        app: &str,
    ) -> Result<Option<tako_core::DatabaseConnection>, StateStoreError> {
        Ok(self
            .get_app_databases(app)?
            .get(tako_core::CHANNEL_REPLAY_BINDING)
            .map(crate::databases::connection))
    }

    /// Replaces an app environment's grant and returns its key, which stays
    /// the same while the environment keeps a grant. An empty set revokes
    /// the grant.
    pub(crate) fn set_database_grants(
        &self,
        app: &str,
        databases: &BTreeSet<String>,
    ) -> Result<Option<String>, StateStoreError> {
        self.write(|r| {
            let row = ("database_grants", app, "");
            if databases.is_empty() {
                remove(r, row)?;
                return Ok(None);
            }
            let key = bytes(r, row)?
                .map(|v| self.decrypt_json::<DatabaseGrant>(&v))
                .transpose()?
                .map(|v| v.key)
                .unwrap_or_else(tako_db::generate_token);
            let value = self.encrypt_json(&DatabaseGrant {
                databases: databases.clone(),
                key: key.clone(),
            })?;
            r.insert(row, value.as_slice()).map_err(storage)?;
            Ok(Some(key))
        })
    }
    pub(crate) fn all_database_grants(
        &self,
    ) -> Result<BTreeMap<String, DatabaseGrant>, StateStoreError> {
        self.all_database_rows("database_grants")
    }
    pub(crate) fn all_app_databases(
        &self,
    ) -> Result<BTreeMap<String, HashMap<String, AppDatabase>>, StateStoreError> {
        self.all_database_rows("app_databases")
    }
    fn all_database_rows<T: serde::de::DeserializeOwned>(
        &self,
        bucket: &str,
    ) -> Result<BTreeMap<String, T>, StateStoreError> {
        self.read(|r| {
            Ok(scan(r, bucket, |id, _, value| {
                Ok((id.to_owned(), self.decrypt_json(value)?))
            })?
            .into_iter()
            .collect())
        })
    }

    fn encrypt_json(&self, value: &impl Serialize) -> Result<Vec<u8>, StateStoreError> {
        let json =
            serde_json::to_vec(value).map_err(|e| StateStoreError::InvalidData(e.to_string()))?;
        encryption::encrypt_blob(&self.encryption_key, &json)
    }

    fn decrypt_json<T: for<'de> Deserialize<'de>>(
        &self,
        bytes: &[u8],
    ) -> Result<T, StateStoreError> {
        let json = encryption::decrypt_blob(&self.encryption_key, bytes)?;
        serde_json::from_slice(&json).map_err(|e| StateStoreError::InvalidData(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.db"), [3; 32]);
        store.init().unwrap();
        (dir, store)
    }

    #[test]
    fn databases_round_trip_with_encrypted_secrets() {
        let (_dir, store) = store();
        let database = StoredDatabase {
            name: "main".into(),
            engine: tako_core::DatabaseEngine::Sqlite,
            generation: "0123456789abcdef0123456789abcdef".into(),
            created_at_unix_secs: 7,
            secrets: DatabaseSecrets {
                backup_key_base64: "a2V5".into(),
                backup: None,
            },
        };
        store.create_database(&database).unwrap();
        assert!(
            store.create_database(&database).is_err(),
            "names are unique"
        );
        assert_eq!(
            store.get_database(&database.name).unwrap().unwrap(),
            database
        );
        let mut secrets = database.secrets.clone();
        secrets.backup = Some(tako_core::StorageBinding {
            provider: tako_core::StorageProvider::S3,
            resource: None,
            bucket: Some("b".into()),
            endpoint: Some("https://s3.example.test".into()),
            region: Some("auto".into()),
            access_key_id: Some("k".into()),
            secret_access_key: Some("s".into()),
            force_path_style: false,
            public_base_url: None,
            path: None,
            signing_key: None,
        });
        store
            .set_database_secrets(&database.name, &secrets)
            .unwrap();
        assert_eq!(store.list_databases().unwrap()[0].secrets, secrets);
        let raw: Vec<u8> = store
            .read(|r| super::bytes(r, ("databases", &database.name, "")))
            .unwrap()
            .unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("a2V5"));
        let kv = StoredDatabase {
            name: "sessions".into(),
            engine: tako_core::DatabaseEngine::Kv,
            ..database.clone()
        };
        store.create_database(&kv).unwrap();
        assert_eq!(
            store.get_database("sessions").unwrap().unwrap().engine,
            kv.engine
        );
        assert_eq!(store.list_databases().unwrap()[1], kv);
        store.delete_database(&database.name).unwrap();
        assert!(store.get_database(&database.name).unwrap().is_none());
        assert!(
            store
                .set_database_secrets(&database.name, &secrets)
                .is_err()
        );
    }

    #[test]
    fn grants_replace_and_clear_and_keep_their_key() {
        let (_dir, store) = store();
        let ids = BTreeSet::from(["main".to_owned()]);
        let key = store
            .set_database_grants("shop/production", &ids)
            .unwrap()
            .unwrap();
        let grants = store.all_database_grants().unwrap();
        assert_eq!(grants["shop/production"].databases, ids);
        assert_eq!(grants["shop/production"].key, key);

        let more = BTreeSet::from(["main".to_owned(), "orders".to_owned()]);
        let same = store.set_database_grants("shop/production", &more).unwrap();
        assert_eq!(same.as_deref(), Some(key.as_str()), "the key is stable");

        assert_eq!(
            store
                .set_database_grants("shop/production", &BTreeSet::new())
                .unwrap(),
            None
        );
        assert!(store.all_database_grants().unwrap().is_empty());
        let renewed = store
            .set_database_grants("shop/production", &ids)
            .unwrap()
            .unwrap();
        assert_ne!(renewed, key, "a revoked grant's key is not reused");
    }
}

//! Databases hosted on this server, the database bindings of apps on this
//! server, and grants for apps on other servers.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use super::{SqliteStateStore, StateStoreError, encryption};

/// Secret settings of a hosted database.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DatabaseSecrets {
    pub backup_key_base64: String,
    #[serde(default)]
    pub backup: Option<tako_core::StorageBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredDatabase {
    pub id: String,
    pub name: String,
    pub created_at_unix_secs: u64,
    pub secrets: DatabaseSecrets,
}

/// A database binding of an app on this server, with the token its
/// processes present. The token stays the same across deploys while the
/// binding names the same database, so instances of the previous release keep
/// working during a rolling update.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AppDatabase {
    pub binding: tako_core::DatabaseBinding,
    pub token: String,
}

/// What this server grants an app environment that runs on other servers:
/// its databases here, and the key those servers present when forwarding.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DatabaseGrant {
    pub databases: BTreeSet<String>,
    pub key: String,
}

impl SqliteStateStore {
    pub(crate) fn create_database(&self, database: &StoredDatabase) -> Result<(), StateStoreError> {
        let encrypted = self.encrypt_json(&database.secrets)?;
        self.lock_conn()?.execute(
            "INSERT INTO databases (id, name, created_at, encrypted_data) VALUES (?1, ?2, ?3, ?4)",
            (
                &database.id,
                &database.name,
                database.created_at_unix_secs as i64,
                encrypted,
            ),
        )?;
        Ok(())
    }

    pub(crate) fn list_databases(&self) -> Result<Vec<StoredDatabase>, StateStoreError> {
        let conn = self.lock_conn()?;
        let mut statement = conn.prepare(
            "SELECT id, name, created_at, encrypted_data FROM databases ORDER BY name, id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, name, created_at, encrypted)| {
                Ok(StoredDatabase {
                    id,
                    name,
                    created_at_unix_secs: created_at.max(0) as u64,
                    secrets: self.decrypt_json(&encrypted)?,
                })
            })
            .collect()
    }

    pub(crate) fn get_database(&self, id: &str) -> Result<Option<StoredDatabase>, StateStoreError> {
        let row = self
            .lock_conn()?
            .query_row(
                "SELECT name, created_at, encrypted_data FROM databases WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(name, created_at, encrypted)| {
            Ok(StoredDatabase {
                id: id.to_owned(),
                name,
                created_at_unix_secs: created_at.max(0) as u64,
                secrets: self.decrypt_json(&encrypted)?,
            })
        })
        .transpose()
    }

    pub(crate) fn set_database_secrets(
        &self,
        id: &str,
        secrets: &DatabaseSecrets,
    ) -> Result<(), StateStoreError> {
        let encrypted = self.encrypt_json(secrets)?;
        let changed = self.lock_conn()?.execute(
            "UPDATE databases SET encrypted_data = ?2 WHERE id = ?1",
            (id, encrypted),
        )?;
        if changed == 0 {
            return Err(StateStoreError::InvalidData(format!(
                "database {id} is not hosted on this server"
            )));
        }
        Ok(())
    }

    pub(crate) fn delete_database(&self, id: &str) -> Result<(), StateStoreError> {
        self.lock_conn()?
            .execute("DELETE FROM databases WHERE id = ?1", [id])?;
        Ok(())
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

    /// Replaces an app environment's grant and returns its key, which stays
    /// the same while the environment keeps a grant. An empty set revokes
    /// the grant.
    pub(crate) fn set_database_grants(
        &self,
        app: &str,
        databases: &BTreeSet<String>,
    ) -> Result<Option<String>, StateStoreError> {
        if databases.is_empty() {
            self.lock_conn()?
                .execute("DELETE FROM database_grants WHERE app = ?1", [app])?;
            return Ok(None);
        }
        let key = match self.all_database_grants()?.remove(app) {
            Some(existing) => existing.key,
            None => tako_database::generate_token(),
        };
        let encrypted = self.encrypt_json(&DatabaseGrant {
            databases: databases.clone(),
            key: key.clone(),
        })?;
        self.lock_conn()?.execute(
            "INSERT INTO database_grants (app, encrypted_data) VALUES (?1, ?2) ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data",
            (app, encrypted),
        )?;
        Ok(Some(key))
    }

    /// Remote grants of every app environment.
    pub(crate) fn all_database_grants(
        &self,
    ) -> Result<BTreeMap<String, DatabaseGrant>, StateStoreError> {
        let rows = {
            let conn = self.lock_conn()?;
            let mut statement =
                conn.prepare("SELECT app, encrypted_data FROM database_grants ORDER BY app")?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        rows.into_iter()
            .map(|(app, encrypted)| Ok((app, self.decrypt_json(&encrypted)?)))
            .collect()
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

    fn store() -> (tempfile::TempDir, SqliteStateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStateStore::new(dir.path().join("state.db"), [3; 32]);
        store.init().unwrap();
        (dir, store)
    }

    #[test]
    fn databases_round_trip_with_encrypted_secrets() {
        let (_dir, store) = store();
        let database = StoredDatabase {
            id: "0123456789abcdef0123456789abcdef".into(),
            name: "main".into(),
            created_at_unix_secs: 7,
            secrets: DatabaseSecrets {
                backup_key_base64: "a2V5".into(),
                backup: None,
            },
        };
        store.create_database(&database).unwrap();
        assert!(store.create_database(&database).is_err(), "ids are unique");
        assert_eq!(store.get_database(&database.id).unwrap().unwrap(), database);
        let mut secrets = database.secrets.clone();
        secrets.backup = Some(tako_core::StorageBinding {
            provider: tako_core::StorageProvider::S3,
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
        store.set_database_secrets(&database.id, &secrets).unwrap();
        assert_eq!(store.list_databases().unwrap()[0].secrets, secrets);
        let raw: Vec<u8> = store
            .lock_conn()
            .unwrap()
            .query_row("SELECT encrypted_data FROM databases", [], |row| row.get(0))
            .unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("a2V5"));
        store.delete_database(&database.id).unwrap();
        assert!(store.get_database(&database.id).unwrap().is_none());
        assert!(store.set_database_secrets(&database.id, &secrets).is_err());
    }

    #[test]
    fn grants_replace_and_clear_and_keep_their_key() {
        let (_dir, store) = store();
        let ids = BTreeSet::from(["a".repeat(32)]);
        let key = store
            .set_database_grants("shop/production", &ids)
            .unwrap()
            .unwrap();
        let grants = store.all_database_grants().unwrap();
        assert_eq!(grants["shop/production"].databases, ids);
        assert_eq!(grants["shop/production"].key, key);

        let more = BTreeSet::from(["a".repeat(32), "b".repeat(32)]);
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

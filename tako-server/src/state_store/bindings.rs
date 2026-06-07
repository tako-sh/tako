use std::collections::HashMap;

use rusqlite::OptionalExtension;

use super::encryption::{decrypt_blob, encrypt_blob};
use super::{SqliteStateStore, StateStoreError};

impl SqliteStateStore {
    pub fn set_storages(
        &self,
        app: &str,
        storages: &HashMap<String, tako_core::StorageBinding>,
    ) -> Result<(), StateStoreError> {
        let json = serde_json::to_vec(storages)
            .map_err(|e| StateStoreError::InvalidData(format!("serialize storages: {e}")))?;
        let encrypted = encrypt_blob(&self.encryption_key, &json)?;
        let conn = self.open_connection()?;
        conn.execute(
            "INSERT INTO app_storages (app, encrypted_data)
             VALUES (?1, ?2)
             ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data;",
            rusqlite::params![app, encrypted],
        )
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn get_storages(
        &self,
        app: &str,
    ) -> Result<HashMap<String, tako_core::StorageBinding>, StateStoreError> {
        let conn = self.open_connection()?;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT encrypted_data FROM app_storages WHERE app = ?1;",
                [app],
                |row| row.get(0),
            )
            .optional()
            .map_err(StateStoreError::from)?;

        match blob {
            Some(encrypted) => {
                let json = decrypt_blob(&self.encryption_key, &encrypted)?;
                serde_json::from_slice(&json)
                    .map_err(|e| StateStoreError::InvalidData(format!("deserialize storages: {e}")))
            }
            None => Ok(HashMap::new()),
        }
    }

    pub fn set_ssl(&self, app: &str, ssl: &tako_core::SslBinding) -> Result<(), StateStoreError> {
        let json = serde_json::to_vec(ssl)
            .map_err(|e| StateStoreError::InvalidData(format!("serialize ssl: {e}")))?;
        let encrypted = encrypt_blob(&self.encryption_key, &json)?;
        let conn = self.open_connection()?;
        conn.execute(
            "INSERT INTO app_ssl (app, encrypted_data)
             VALUES (?1, ?2)
             ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data;",
            rusqlite::params![app, encrypted],
        )
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn get_ssl(&self, app: &str) -> Result<Option<tako_core::SslBinding>, StateStoreError> {
        let conn = self.open_connection()?;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT encrypted_data FROM app_ssl WHERE app = ?1;",
                [app],
                |row| row.get(0),
            )
            .optional()
            .map_err(StateStoreError::from)?;

        match blob {
            Some(encrypted) => {
                let json = decrypt_blob(&self.encryption_key, &encrypted)?;
                serde_json::from_slice(&json)
                    .map(Some)
                    .map_err(|e| StateStoreError::InvalidData(format!("deserialize ssl: {e}")))
            }
            None => Ok(None),
        }
    }

    pub fn delete_ssl(&self, app: &str) -> Result<(), StateStoreError> {
        let conn = self.open_connection()?;
        conn.execute("DELETE FROM app_ssl WHERE app = ?1;", [app])
            .map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn set_backup(
        &self,
        app: &str,
        backup: Option<&tako_core::BackupBinding>,
    ) -> Result<(), StateStoreError> {
        let conn = self.open_connection()?;
        match backup {
            Some(backup) => {
                let json = serde_json::to_vec(backup).map_err(|e| {
                    StateStoreError::InvalidData(format!("serialize backup config: {e}"))
                })?;
                let encrypted = encrypt_blob(&self.encryption_key, &json)?;
                conn.execute(
                    "INSERT INTO app_backups (app, encrypted_data)
                     VALUES (?1, ?2)
                     ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data;",
                    rusqlite::params![app, encrypted],
                )
                .map_err(StateStoreError::from)?;
            }
            None => {
                conn.execute("DELETE FROM app_backups WHERE app = ?1;", [app])
                    .map_err(StateStoreError::from)?;
            }
        }
        Ok(())
    }

    pub fn get_backup(
        &self,
        app: &str,
    ) -> Result<Option<tako_core::BackupBinding>, StateStoreError> {
        let conn = self.open_connection()?;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT encrypted_data FROM app_backups WHERE app = ?1;",
                [app],
                |row| row.get(0),
            )
            .optional()
            .map_err(StateStoreError::from)?;

        match blob {
            Some(encrypted) => {
                let json = decrypt_blob(&self.encryption_key, &encrypted)?;
                serde_json::from_slice(&json).map(Some).map_err(|e| {
                    StateStoreError::InvalidData(format!("deserialize backup config: {e}"))
                })
            }
            None => Ok(None),
        }
    }
}

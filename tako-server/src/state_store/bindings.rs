use std::collections::HashMap;

use super::{SqliteStateStore, StateStoreError};

impl SqliteStateStore {
    pub fn set_storages(
        &self,
        app: &str,
        storages: &HashMap<String, tako_core::StorageBinding>,
    ) -> Result<(), StateStoreError> {
        let json = serde_json::to_vec(storages)
            .map_err(|e| StateStoreError::InvalidData(format!("serialize storages: {e}")))?;
        self.set_encrypted_row("app_storages", app, &json)
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

    pub fn delete_ssl(&self, app: &str) -> Result<(), StateStoreError> {
        self.delete_row("app_ssl", app)
    }

    pub fn set_backup(
        &self,
        app: &str,
        backup: Option<&tako_core::BackupBinding>,
    ) -> Result<(), StateStoreError> {
        match backup {
            Some(backup) => {
                let json = serde_json::to_vec(backup).map_err(|e| {
                    StateStoreError::InvalidData(format!("serialize backup config: {e}"))
                })?;
                self.set_encrypted_row("app_backups", app, &json)
            }
            None => self.delete_row("app_backups", app),
        }
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

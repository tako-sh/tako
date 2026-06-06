use std::collections::HashMap;

use rusqlite::OptionalExtension;

use super::{StateStoreError, decrypt_blob, encrypt_blob};
use crate::state_store::SqliteStateStore;

impl SqliteStateStore {
    pub fn set_secrets(
        &self,
        app: &str,
        secrets: &HashMap<String, String>,
    ) -> Result<(), StateStoreError> {
        let json = serde_json::to_vec(secrets)
            .map_err(|e| StateStoreError::InvalidData(format!("serialize secrets: {e}")))?;
        let encrypted = encrypt_blob(&self.encryption_key, &json)?;
        let conn = self.open_connection()?;
        conn.execute(
            "INSERT INTO app_secrets (app, encrypted_data)
             VALUES (?1, ?2)
             ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data;",
            rusqlite::params![app, encrypted],
        )
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn get_secrets(&self, app: &str) -> Result<HashMap<String, String>, StateStoreError> {
        let conn = self.open_connection()?;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT encrypted_data FROM app_secrets WHERE app = ?1;",
                [app],
                |row| row.get(0),
            )
            .optional()
            .map_err(StateStoreError::from)?;

        match blob {
            Some(encrypted) => {
                let json = decrypt_blob(&self.encryption_key, &encrypted)?;
                serde_json::from_slice(&json)
                    .map_err(|e| StateStoreError::InvalidData(format!("deserialize secrets: {e}")))
            }
            None => Ok(HashMap::new()),
        }
    }

    pub fn set_runtime_credentials(
        &self,
        app: &str,
        credentials: &HashMap<String, String>,
    ) -> Result<(), StateStoreError> {
        let json = serde_json::to_vec(credentials).map_err(|e| {
            StateStoreError::InvalidData(format!("serialize runtime credentials: {e}"))
        })?;
        let encrypted = encrypt_blob(&self.encryption_key, &json)?;
        let conn = self.open_connection()?;
        conn.execute(
            "INSERT INTO app_runtime_credentials (app, encrypted_data)
             VALUES (?1, ?2)
             ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data;",
            rusqlite::params![app, encrypted],
        )
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn get_runtime_credentials(
        &self,
        app: &str,
    ) -> Result<HashMap<String, String>, StateStoreError> {
        let conn = self.open_connection()?;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT encrypted_data FROM app_runtime_credentials WHERE app = ?1;",
                [app],
                |row| row.get(0),
            )
            .optional()
            .map_err(StateStoreError::from)?;

        match blob {
            Some(encrypted) => {
                let json = decrypt_blob(&self.encryption_key, &encrypted)?;
                serde_json::from_slice(&json).map_err(|e| {
                    StateStoreError::InvalidData(format!("deserialize runtime credentials: {e}"))
                })
            }
            None => Ok(HashMap::new()),
        }
    }
}

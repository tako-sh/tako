use std::collections::HashMap;

use super::StateStoreError;
use crate::state_store::SqliteStateStore;

impl SqliteStateStore {
    pub fn set_secrets(
        &self,
        app: &str,
        secrets: &HashMap<String, String>,
    ) -> Result<(), StateStoreError> {
        let json = serde_json::to_vec(secrets)
            .map_err(|e| StateStoreError::InvalidData(format!("serialize secrets: {e}")))?;
        self.set_encrypted_row("app_secrets", app, &json)
    }

    pub fn get_secrets(&self, app: &str) -> Result<HashMap<String, String>, StateStoreError> {
        match self.get_encrypted_row("app_secrets", app)? {
            Some(json) => serde_json::from_slice(&json)
                .map_err(|e| StateStoreError::InvalidData(format!("deserialize secrets: {e}"))),
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
        self.set_encrypted_row("app_runtime_credentials", app, &json)
    }

    pub fn get_runtime_credentials(
        &self,
        app: &str,
    ) -> Result<HashMap<String, String>, StateStoreError> {
        match self.get_encrypted_row("app_runtime_credentials", app)? {
            Some(json) => serde_json::from_slice(&json).map_err(|e| {
                StateStoreError::InvalidData(format!("deserialize runtime credentials: {e}"))
            }),
            None => Ok(HashMap::new()),
        }
    }
}

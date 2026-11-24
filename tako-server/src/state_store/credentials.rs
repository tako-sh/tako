use std::collections::HashMap;

use super::StateStoreError;
use crate::state_store::SqliteStateStore;

impl SqliteStateStore {
    #[cfg(test)]
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

    /// The Compose group's SDK token, created by its first deploy with an
    /// `sdk: true` service and committed with that deployment.
    pub fn get_compose_token(&self, app: &str) -> Result<Option<String>, StateStoreError> {
        self.get_encrypted_row("app_compose_tokens", app)?
            .map(|token| {
                String::from_utf8(token)
                    .map_err(|e| StateStoreError::InvalidData(format!("decode Compose token: {e}")))
            })
            .transpose()
    }
}

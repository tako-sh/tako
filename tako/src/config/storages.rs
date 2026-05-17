use serde::{Deserialize, Serialize};

use super::error::{ConfigError, Result};
use super::secrets::EncryptedSecretValue;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EncryptedStorageCredentials {
    pub access_key_id: EncryptedSecretValue,
    pub secret_access_key: EncryptedSecretValue,
}

impl EncryptedStorageCredentials {
    pub fn new(
        access_key_id: String,
        secret_access_key: String,
        expires_at: Option<String>,
    ) -> Self {
        Self {
            access_key_id: EncryptedSecretValue::new(access_key_id, expires_at.clone()),
            secret_access_key: EncryptedSecretValue::new(secret_access_key, expires_at),
        }
    }
}

pub fn validate_storage_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(ConfigError::Validation(
            "Storage name cannot be empty".to_string(),
        ));
    }
    for c in name.chars() {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_' && c != '-' {
            return Err(ConfigError::Validation(format!(
                "Storage name can only contain lowercase letters, numbers, hyphens, and underscores. Found: '{}'",
                c
            )));
        }
    }
    Ok(())
}

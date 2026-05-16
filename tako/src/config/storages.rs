use serde::{Deserialize, Serialize};

use super::error::{ConfigError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EncryptedStorageCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
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

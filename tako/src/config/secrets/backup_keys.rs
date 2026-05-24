use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, Result};

use super::{SecretsStore, validate_environment_name};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EncryptedBackupKey {
    pub id: String,
    pub value: String,
}

impl EncryptedBackupKey {
    pub fn new(id: String, value: String) -> Self {
        Self { id, value }
    }
}

impl SecretsStore {
    pub fn get_backup_keys(&self, env: &str) -> Option<&[EncryptedBackupKey]> {
        self.environments
            .get(env)
            .map(|env_secrets| env_secrets.backup_keys.as_slice())
    }

    pub fn active_backup_key(&self, env: &str) -> Option<&EncryptedBackupKey> {
        self.get_backup_keys(env).and_then(|keys| keys.last())
    }

    pub fn push_backup_key(&mut self, env: &str, key: EncryptedBackupKey) -> Result<()> {
        validate_environment_name(env)?;
        validate_backup_key_id(&key.id)?;
        validate_encrypted_backup_key_value(&key)?;

        let env_secrets = self.environments.get_mut(env).ok_or_else(|| {
            ConfigError::Validation(format!(
                "Environment '{}' not initialized. Call ensure_env_key_id first.",
                env
            ))
        })?;

        env_secrets.backup_keys.push(key);
        Ok(())
    }
}

pub fn validate_backup_key_id(id: &str) -> Result<()> {
    let Some(suffix) = id.strip_prefix("backup-key-") else {
        return Err(ConfigError::Validation(
            "Backup key id must start with 'backup-key-'.".to_string(),
        ));
    };
    if suffix.len() != 16 || !suffix.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ConfigError::Validation(
            "Backup key id must end with 16 hex characters.".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn validate_encrypted_backup_key_value(key: &EncryptedBackupKey) -> Result<()> {
    if key.value.trim().is_empty() {
        return Err(ConfigError::Validation(
            "Backup key value cannot be empty".to_string(),
        ));
    }
    Ok(())
}

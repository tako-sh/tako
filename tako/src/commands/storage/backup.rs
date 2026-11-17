use std::path::Path;

use crate::config::{EncryptedBackupKey, TakoToml};

pub(crate) fn decrypt_backup_binding(
    env: &str,
    config: &TakoToml,
    secrets: &crate::config::SecretsStore,
    usage_path: Option<&Path>,
) -> Result<Option<tako_core::BackupBinding>, Box<dyn std::error::Error>> {
    let Some(backup) = config
        .envs
        .get(env)
        .and_then(|env_config| env_config.backup.as_ref())
    else {
        return Ok(None);
    };
    let resource_name = backup.storage.as_str();
    let Some(resource) = config.storages.get(resource_name) else {
        return Err(
            format!("Backup storage references missing resource '{resource_name}'.").into(),
        );
    };
    if resource.provider != tako_core::StorageProvider::S3 {
        return Err("Backup storage must be S3-compatible.".into());
    }
    if resource.public_base_url.is_some() {
        return Err("Backup storage must be private.".into());
    }

    let mut key_cache = None;
    let mut storage = super::decrypt_s3_storage_binding(
        env,
        resource_name,
        resource,
        secrets,
        usage_path,
        &mut key_cache,
    )?;
    storage.public_base_url = None;
    let backup_keys = decrypt_backup_keys(env, secrets, usage_path)?;
    if backup_keys.is_empty() {
        return Err(
            "Backup encryption key is missing. Deploy or run `tako backups now` to create it."
                .into(),
        );
    }
    Ok(Some(tako_core::BackupBinding {
        storage,
        backup_keys,
        retention_days: tako_core::DEFAULT_BACKUP_RETENTION_DAYS,
    }))
}

pub(crate) fn ensure_backup_keys_for_env(
    project_dir: &Path,
    env: &str,
    config: &TakoToml,
    secrets: &mut crate::config::SecretsStore,
) -> Result<bool, Box<dyn std::error::Error>> {
    let backup_enabled = config
        .envs
        .get(env)
        .and_then(|env_config| env_config.backup.as_ref())
        .is_some();
    if !backup_enabled || secrets.active_backup_key(env).is_some() {
        return Ok(false);
    }

    secrets.ensure_env_key_id(env)?;
    let env_key =
        crate::commands::secret::load_or_create_key_for_set(env, secrets, Some(project_dir))?;
    let backup_key = crate::crypto::EncryptionKey::generate()?;
    let encrypted_key = crate::crypto::encrypt(&backup_key.to_base64(), &env_key)?;
    secrets.push_backup_key(
        env,
        EncryptedBackupKey::new(generate_backup_key_id()?, encrypted_key),
    )?;
    secrets.save_to_dir(project_dir)?;
    Ok(true)
}

fn decrypt_backup_keys(
    env: &str,
    secrets: &crate::config::SecretsStore,
    usage_path: Option<&Path>,
) -> Result<Vec<tako_core::BackupKeyBinding>, Box<dyn std::error::Error>> {
    let Some(encrypted_keys) = secrets.get_backup_keys(env) else {
        return Ok(Vec::new());
    };
    if encrypted_keys.is_empty() {
        return Ok(Vec::new());
    }

    let env_key = crate::commands::secret::load_secret_key(env, secrets, usage_path)?;
    let mut keys = Vec::with_capacity(encrypted_keys.len());
    for encrypted_key in encrypted_keys {
        let key_base64 = crate::crypto::decrypt(&encrypted_key.value, &env_key)?;
        crate::crypto::EncryptionKey::from_base64(&key_base64)?;
        keys.push(tako_core::BackupKeyBinding {
            id: encrypted_key.id.clone(),
            key_base64,
        });
    }
    Ok(keys)
}

fn generate_backup_key_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes)?;
    Ok(format!("backup-key-{}", hex::encode(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    use crate::commands::storage::{StorageCredentialsInput, set_storage_credentials};

    fn with_temp_tako_home<T>(f: impl FnOnce() -> T) -> T {
        let _lock = crate::paths::test_tako_home_env_lock();
        let home = tempfile::TempDir::new().unwrap();
        let previous = std::env::var_os("TAKO_HOME");
        unsafe {
            std::env::set_var("TAKO_HOME", home.path());
        }

        struct ResetEnv(Option<OsString>);
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("TAKO_HOME", value) },
                    None => unsafe { std::env::remove_var("TAKO_HOME") },
                }
            }
        }

        let _reset = ResetEnv(previous);
        f()
    }

    #[test]
    fn decrypt_backup_binding_uses_storage_resource_without_app_binding() {
        with_temp_tako_home(|| {
            let temp = tempfile::TempDir::new().unwrap();
            let config_path = temp.path().join("tako.toml");
            std::fs::write(
                &config_path,
                r#"
name = "demo"

[storages.r2]
provider = "s3"
bucket = "demo-backups"
endpoint = "https://s3.example.com"
region = "auto"

[envs.production]
route = "demo.example.com"
backup = { storage = "r2" }
"#,
            )
            .unwrap();

            set_storage_credentials(StorageCredentialsInput {
                project_dir: temp.path(),
                config_path: &config_path,
                resource: "r2".to_string(),
                env: "production".to_string(),
                access_key_id: Some("key-id".to_string()),
                secret_access_key: Some("secret".to_string()),
                expires_on: None,
            })
            .unwrap();

            let config = TakoToml::load_from_file(&config_path).unwrap();
            let mut secrets = crate::config::SecretsStore::load_from_dir(temp.path()).unwrap();
            assert!(
                ensure_backup_keys_for_env(temp.path(), "production", &config, &mut secrets)
                    .unwrap()
            );
            let backup = decrypt_backup_binding("production", &config, &secrets, Some(temp.path()))
                .unwrap()
                .expect("backup binding");

            assert!(config.envs["production"].storages.is_empty());
            assert_eq!(
                backup.retention_days,
                tako_core::DEFAULT_BACKUP_RETENTION_DAYS
            );
            assert_eq!(backup.storage.bucket.as_deref(), Some("demo-backups"));
            assert_eq!(
                backup.storage.endpoint.as_deref(),
                Some("https://s3.example.com")
            );
            assert_eq!(backup.storage.access_key_id.as_deref(), Some("key-id"));
            assert_eq!(backup.storage.secret_access_key.as_deref(), Some("secret"));
            assert_eq!(backup.storage.public_base_url, None);
            assert_eq!(backup.backup_keys.len(), 1);
            assert!(backup.backup_keys[0].id.starts_with("backup-key-"));
            assert!(
                crate::crypto::EncryptionKey::from_base64(&backup.backup_keys[0].key_base64)
                    .is_ok()
            );

            let saved = crate::config::SecretsStore::load_from_dir(temp.path()).unwrap();
            assert_eq!(saved.get_backup_keys("production").unwrap().len(), 1);
        });
    }

    #[test]
    fn ensure_backup_keys_for_env_is_idempotent() {
        with_temp_tako_home(|| {
            let temp = tempfile::TempDir::new().unwrap();
            let config = TakoToml::parse(
                r#"
name = "demo"

[storages.r2]
provider = "s3"
bucket = "demo-backups"
endpoint = "https://s3.example.com"
region = "auto"

[envs.production]
route = "demo.example.com"
backup = { storage = "r2" }
"#,
            )
            .unwrap();
            let mut secrets = crate::config::SecretsStore::default();
            secrets.ensure_env_key_id("production").unwrap();

            assert!(
                ensure_backup_keys_for_env(temp.path(), "production", &config, &mut secrets)
                    .unwrap()
            );
            let first_id = secrets.active_backup_key("production").unwrap().id.clone();
            assert!(
                !ensure_backup_keys_for_env(temp.path(), "production", &config, &mut secrets)
                    .unwrap()
            );

            let keys = secrets.get_backup_keys("production").unwrap();
            assert_eq!(keys.len(), 1);
            assert_eq!(keys[0].id, first_id);
        });
    }

    #[test]
    fn decrypt_backup_binding_returns_none_when_backup_is_absent() {
        let mut config = TakoToml::default();
        config.envs.insert(
            "production".to_string(),
            crate::config::EnvConfig::default(),
        );
        let secrets = crate::config::SecretsStore::default();

        let binding = decrypt_backup_binding("production", &config, &secrets, None).unwrap();

        assert!(binding.is_none());
    }
}

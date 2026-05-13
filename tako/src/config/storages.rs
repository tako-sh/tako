use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use super::error::{ConfigError, Result};

const STORAGES_FILE_NAME: &str = "storages.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentStorages {
    pub key_id: String,
    #[serde(default)]
    pub storages: HashMap<String, EncryptedStorageBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EncryptedStorageBinding {
    pub provider: tako_core::StorageProvider,
    pub bucket: String,
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default)]
    pub force_path_style: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StoragesStore {
    #[serde(flatten)]
    pub environments: HashMap<String, EnvironmentStorages>,
}

impl StoragesStore {
    pub fn default_path<P: AsRef<Path>>(project_dir: P) -> PathBuf {
        project_dir.as_ref().join(".tako").join(STORAGES_FILE_NAME)
    }

    pub fn load_from_dir<P: AsRef<Path>>(project_dir: P) -> Result<Self> {
        let path = Self::default_path(&project_dir);
        if path.exists() {
            Self::load_from_file(&path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())
            .map_err(|e| ConfigError::FileRead(path.as_ref().to_path_buf(), e))?;
        Self::parse(&content)
    }

    pub fn parse(content: &str) -> Result<Self> {
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        let store: Self = serde_json::from_str(content)?;
        store.validate()?;
        Ok(store)
    }

    pub fn validate(&self) -> Result<()> {
        for (env_name, env_storages) in &self.environments {
            super::validate_environment_name(env_name)?;
            crate::crypto::KeyStore::for_key_id(&env_storages.key_id)?;
            for (name, storage) in &env_storages.storages {
                validate_storage_name(name)?;
                validate_storage_plain_fields(storage)?;
            }
        }
        Ok(())
    }

    pub fn save_to_dir<P: AsRef<Path>>(&self, project_dir: P) -> Result<()> {
        let path = Self::default_path(&project_dir);
        self.save_to_file(&path)
    }

    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        use std::io::Write;
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ConfigError::FileWrite(parent.to_path_buf(), e))?;
        }
        let content = serde_json::to_string_pretty(&sorted_environments(&self.environments))?;
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(path.as_ref())
            .map_err(|e| ConfigError::FileWrite(path.as_ref().to_path_buf(), e))?;
        file.write_all(content.as_bytes())
            .map_err(|e| ConfigError::FileWrite(path.as_ref().to_path_buf(), e))?;
        Ok(())
    }

    pub fn ensure_env_key_id(&mut self, env: &str) -> Result<String> {
        super::validate_environment_name(env)?;
        let env_storages =
            self.environments
                .entry(env.to_string())
                .or_insert_with(|| EnvironmentStorages {
                    key_id: crate::crypto::generate_key_id(),
                    storages: HashMap::new(),
                });
        Ok(env_storages.key_id.clone())
    }

    pub fn get_key_id(&self, env: &str) -> Option<&str> {
        self.environments
            .get(env)
            .map(|value| value.key_id.as_str())
    }

    pub fn set_env_key_id(&mut self, env: &str, key_id: &str) -> Result<()> {
        super::validate_environment_name(env)?;
        crate::crypto::KeyStore::for_key_id(key_id)?;
        self.environments
            .entry(env.to_string())
            .and_modify(|entry| entry.key_id = key_id.to_string())
            .or_insert_with(|| EnvironmentStorages {
                key_id: key_id.to_string(),
                storages: HashMap::new(),
            });
        Ok(())
    }

    pub fn get_env(&self, env: &str) -> Option<&HashMap<String, EncryptedStorageBinding>> {
        self.environments.get(env).map(|value| &value.storages)
    }

    pub fn set(&mut self, env: &str, name: &str, value: EncryptedStorageBinding) -> Result<()> {
        super::validate_environment_name(env)?;
        validate_storage_name(name)?;
        validate_storage_plain_fields(&value)?;
        let env_storages = self.environments.get_mut(env).ok_or_else(|| {
            ConfigError::Validation(format!(
                "Environment '{}' not initialized. Call ensure_env_key_id first.",
                env
            ))
        })?;
        env_storages.storages.insert(name.to_string(), value);
        Ok(())
    }
}

fn sorted_environments(
    environments: &HashMap<String, EnvironmentStorages>,
) -> BTreeMap<&String, SortedEnvironmentStorages<'_>> {
    environments
        .iter()
        .map(|(env_name, env_storages)| {
            (
                env_name,
                SortedEnvironmentStorages {
                    key_id: &env_storages.key_id,
                    storages: env_storages.storages.iter().collect(),
                },
            )
        })
        .collect()
}

#[derive(Serialize)]
struct SortedEnvironmentStorages<'a> {
    key_id: &'a str,
    storages: BTreeMap<&'a String, &'a EncryptedStorageBinding>,
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

fn validate_storage_plain_fields(storage: &EncryptedStorageBinding) -> Result<()> {
    if storage.bucket.trim().is_empty() {
        return Err(ConfigError::Validation(
            "Storage bucket cannot be empty".to_string(),
        ));
    }
    if storage.endpoint.trim().is_empty() {
        return Err(ConfigError::Validation(
            "Storage endpoint cannot be empty".to_string(),
        ));
    }
    if storage.region.trim().is_empty() {
        return Err(ConfigError::Validation(
            "Storage region cannot be empty".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> EncryptedStorageBinding {
        EncryptedStorageBinding {
            provider: tako_core::StorageProvider::R2,
            bucket: "app-uploads".to_string(),
            endpoint: "https://abc.r2.cloudflarestorage.com".to_string(),
            region: "auto".to_string(),
            access_key_id: "encrypted-key-id".to_string(),
            secret_access_key: "encrypted-secret".to_string(),
            force_path_style: false,
            public_base_url: Some("https://cdn.example.com".to_string()),
        }
    }

    #[test]
    fn parse_reads_environment_storage_bindings() {
        let store = StoragesStore::parse(
            r#"{
              "production": {
                "key_id": "0123456789abcdef",
                "storages": {
                  "uploads": {
                    "provider": "r2",
                    "bucket": "app-uploads",
                    "endpoint": "https://abc.r2.cloudflarestorage.com",
                    "region": "auto",
                    "access_key_id": "encrypted-key-id",
                    "secret_access_key": "encrypted-secret",
                    "public_base_url": "https://cdn.example.com"
                  }
                }
              }
            }"#,
        )
        .unwrap();

        let production = store.get_env("production").unwrap();
        let uploads = production.get("uploads").unwrap();
        assert_eq!(uploads.provider, tako_core::StorageProvider::R2);
        assert_eq!(uploads.bucket, "app-uploads");
        assert_eq!(
            uploads.public_base_url.as_deref(),
            Some("https://cdn.example.com")
        );
    }

    #[test]
    fn set_requires_initialized_environment() {
        let mut store = StoragesStore::default();
        let err = store.set("production", "uploads", binding()).unwrap_err();
        assert!(err.to_string().contains("not initialized"));
    }

    #[test]
    fn set_stores_binding_after_key_id_is_initialized() {
        let mut store = StoragesStore::default();
        store
            .set_env_key_id("production", "0123456789abcdef")
            .unwrap();
        store.set("production", "uploads", binding()).unwrap();

        assert!(store.get_env("production").unwrap().contains_key("uploads"));
        assert_eq!(store.get_key_id("production"), Some("0123456789abcdef"));
    }

    #[test]
    fn validate_storage_name_rejects_uppercase() {
        let err = validate_storage_name("Uploads").unwrap_err();
        assert!(err.to_string().contains("lowercase"));
    }
}

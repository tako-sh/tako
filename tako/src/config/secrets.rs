use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use super::error::{ConfigError, Result};

const SECRETS_FILE_NAME: &str = "secrets.json";

/// Per-environment secrets with a random key id.
///
/// The key id is generated once per app-environment and stored here so every
/// machine can address the same local key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSecrets {
    /// Local key id under Tako's data directory.
    pub key_id: String,
    /// App secret name to encrypted value.
    #[serde(default)]
    pub app: HashMap<String, String>,
    /// Storage resource name to encrypted credentials.
    #[serde(default)]
    pub storages: HashMap<String, super::EncryptedStorageCredentials>,
}

/// Secrets storage from .tako/secrets.json
///
/// Format:
/// ```json
/// {
///   "production": {
///     "key_id": "0123456789abcdef",
///     "app": {
///       "DATABASE_URL": "encrypted_base64_value",
///       "API_KEY": "encrypted_base64_value"
///     },
///     "storages": {
///       "prod_uploads": {
///         "access_key_id": "encrypted_base64_value",
///         "secret_access_key": "encrypted_base64_value"
///       }
///     }
///   }
/// }
/// ```
///
/// App secret names and storage resource names are plaintext (allows listing
/// without decryption). Secret values and storage credentials are encrypted
/// with AES-256-GCM.
/// Keys are random per environment and stored locally under the environment key id.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SecretsStore {
    /// Map of environment name to environment secrets
    #[serde(flatten)]
    pub environments: HashMap<String, EnvironmentSecrets>,
}

impl SecretsStore {
    /// Get the default path for secrets (.tako/secrets.json in project root)
    pub fn default_path<P: AsRef<Path>>(project_dir: P) -> PathBuf {
        project_dir.as_ref().join(".tako").join(SECRETS_FILE_NAME)
    }

    /// Load secrets from a project directory
    pub fn load_from_dir<P: AsRef<Path>>(project_dir: P) -> Result<Self> {
        let path = Self::default_path(&project_dir);
        if path.exists() {
            Self::load_from_file(&path)
        } else {
            Ok(Self::default())
        }
    }

    /// Load secrets from a specific file
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())
            .map_err(|e| ConfigError::FileRead(path.as_ref().to_path_buf(), e))?;
        Self::parse(&content)
    }

    /// Parse secrets from JSON content
    pub fn parse(content: &str) -> Result<Self> {
        if content.trim().is_empty() {
            return Ok(Self::default());
        }

        let store: Self = serde_json::from_str(content)?;
        store.validate()?;
        Ok(store)
    }

    /// Validate secrets
    pub fn validate(&self) -> Result<()> {
        for (env_name, env_secrets) in &self.environments {
            // Validate environment name
            validate_environment_name(env_name)?;
            crate::crypto::KeyStore::for_key_id(&env_secrets.key_id)?;

            // Validate secret names
            for secret_name in env_secrets.app.keys() {
                validate_secret_name(secret_name)?;
            }
            for (storage_name, credentials) in &env_secrets.storages {
                super::validate_storage_name(storage_name)?;
                if credentials.access_key_id.trim().is_empty() {
                    return Err(ConfigError::Validation(
                        "Storage access key id cannot be empty".to_string(),
                    ));
                }
                if credentials.secret_access_key.trim().is_empty() {
                    return Err(ConfigError::Validation(
                        "Storage secret access key cannot be empty".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Save secrets to a project directory
    pub fn save_to_dir<P: AsRef<Path>>(&self, project_dir: P) -> Result<()> {
        let path = Self::default_path(&project_dir);
        self.save_to_file(&path)
    }

    /// Save secrets to a specific file
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        use std::io::Write;

        // Ensure parent directory exists
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ConfigError::FileWrite(parent.to_path_buf(), e))?;
        }

        let content = serde_json::to_string_pretty(&sorted_environments(&self.environments))?;

        // Write with restrictive permissions to protect encrypted secrets
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(path.as_ref())
            .map_err(|e| ConfigError::FileWrite(path.as_ref().to_path_buf(), e))?;
        f.write_all(content.as_bytes())
            .map_err(|e| ConfigError::FileWrite(path.as_ref().to_path_buf(), e))?;

        Ok(())
    }

    /// Get a secret value for an environment
    pub fn get(&self, env: &str, name: &str) -> Option<&String> {
        self.environments
            .get(env)
            .and_then(|env_secrets| env_secrets.app.get(name))
    }

    /// Get the key_id for an environment
    pub fn get_key_id(&self, env: &str) -> Option<&str> {
        self.environments
            .get(env)
            .map(|env_secrets| env_secrets.key_id.as_str())
    }

    /// Set a secret value for an environment (key_id must already exist)
    pub fn set(&mut self, env: &str, name: &str, value: String) -> Result<()> {
        validate_environment_name(env)?;
        validate_secret_name(name)?;

        let env_secrets = self.environments.get_mut(env).ok_or_else(|| {
            ConfigError::Validation(format!(
                "Environment '{}' not initialized. Call ensure_env_key_id first.",
                env
            ))
        })?;

        env_secrets.app.insert(name.to_string(), value);
        Ok(())
    }

    /// Ensure an environment exists with a key_id, generating one if needed.
    /// Returns the key_id.
    pub fn ensure_env_key_id(&mut self, env: &str) -> Result<String> {
        validate_environment_name(env)?;

        let env_secrets =
            self.environments
                .entry(env.to_string())
                .or_insert_with(|| EnvironmentSecrets {
                    key_id: crate::crypto::generate_key_id(),
                    app: HashMap::new(),
                    storages: HashMap::new(),
                });

        Ok(env_secrets.key_id.clone())
    }

    /// Initialize an environment with an existing key id.
    pub fn set_env_key_id(&mut self, env: &str, key_id: &str) -> Result<()> {
        validate_environment_name(env)?;
        crate::crypto::KeyStore::for_key_id(key_id)?;

        self.environments.insert(
            env.to_string(),
            EnvironmentSecrets {
                key_id: key_id.to_string(),
                app: HashMap::new(),
                storages: HashMap::new(),
            },
        );
        Ok(())
    }

    /// Remove a secret from an environment
    pub fn remove(&mut self, env: &str, name: &str) -> Result<()> {
        let env_secrets = self
            .environments
            .get_mut(env)
            .ok_or_else(|| ConfigError::EnvironmentNotFound(env.to_string()))?;

        if env_secrets.app.remove(name).is_none() {
            return Err(ConfigError::SecretNotFound(name.to_string()));
        }

        // Remove environment if no secrets remain.
        if env_secrets.app.is_empty() && env_secrets.storages.is_empty() {
            self.environments.remove(env);
        }

        Ok(())
    }

    /// Remove a secret from all environments
    pub fn remove_all(&mut self, name: &str) -> Result<Vec<String>> {
        let mut removed_from = Vec::new();

        for (env_name, env_secrets) in &mut self.environments {
            if env_secrets.app.remove(name).is_some() {
                removed_from.push(env_name.clone());
            }
        }

        // Remove empty environments
        self.environments.retain(|_, env_secrets| {
            !env_secrets.app.is_empty() || !env_secrets.storages.is_empty()
        });

        if removed_from.is_empty() {
            return Err(ConfigError::SecretNotFound(name.to_string()));
        }

        Ok(removed_from)
    }

    /// Check if a secret exists in an environment
    pub fn contains(&self, env: &str, name: &str) -> bool {
        self.environments
            .get(env)
            .map(|env_secrets| env_secrets.app.contains_key(name))
            .unwrap_or(false)
    }

    /// Get all secret names across all environments
    pub fn all_secret_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .environments
            .values()
            .flat_map(|env_secrets| env_secrets.app.keys().cloned())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Get all environment names
    pub fn environment_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.environments.keys().cloned().collect();
        names.sort();
        names
    }

    /// Get secrets map for an environment
    pub fn get_env(&self, env: &str) -> Option<&HashMap<String, String>> {
        self.environments
            .get(env)
            .map(|env_secrets| &env_secrets.app)
    }

    pub fn get_storage_credentials(
        &self,
        env: &str,
        resource: &str,
    ) -> Option<&super::EncryptedStorageCredentials> {
        self.environments
            .get(env)
            .and_then(|env_secrets| env_secrets.storages.get(resource))
    }

    pub fn get_storage_credentials_env(
        &self,
        env: &str,
    ) -> Option<&HashMap<String, super::EncryptedStorageCredentials>> {
        self.environments
            .get(env)
            .map(|env_secrets| &env_secrets.storages)
    }

    pub fn set_storage_credentials(
        &mut self,
        env: &str,
        resource: &str,
        value: super::EncryptedStorageCredentials,
    ) -> Result<()> {
        validate_environment_name(env)?;
        super::validate_storage_name(resource)?;
        if value.access_key_id.trim().is_empty() {
            return Err(ConfigError::Validation(
                "Storage access key id cannot be empty".to_string(),
            ));
        }
        if value.secret_access_key.trim().is_empty() {
            return Err(ConfigError::Validation(
                "Storage secret access key cannot be empty".to_string(),
            ));
        }

        let env_secrets = self.environments.get_mut(env).ok_or_else(|| {
            ConfigError::Validation(format!(
                "Environment '{}' not initialized. Call ensure_env_key_id first.",
                env
            ))
        })?;

        env_secrets.storages.insert(resource.to_string(), value);
        Ok(())
    }

    /// Check for discrepancies (secrets missing in some environments)
    pub fn find_discrepancies(&self) -> Vec<SecretDiscrepancy> {
        let all_names = self.all_secret_names();
        let all_envs = self.environment_names();

        let mut discrepancies = Vec::new();

        for name in &all_names {
            let mut present_in = Vec::new();
            let mut missing_in = Vec::new();

            for env in &all_envs {
                if self.contains(env, name) {
                    present_in.push(env.clone());
                } else {
                    missing_in.push(env.clone());
                }
            }

            if !missing_in.is_empty() {
                discrepancies.push(SecretDiscrepancy {
                    name: name.clone(),
                    present_in,
                    missing_in,
                });
            }
        }

        discrepancies
    }

    /// Check if all secrets are present in all environments
    pub fn is_consistent(&self) -> bool {
        self.find_discrepancies().is_empty()
    }

    /// Get secrets count per environment
    pub fn count_by_env(&self) -> HashMap<String, usize> {
        self.environments
            .iter()
            .map(|(env, env_secrets)| (env.clone(), env_secrets.app.len()))
            .collect()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.environments.is_empty()
    }

    /// Total number of secrets (across all environments)
    pub fn total_count(&self) -> usize {
        self.environments
            .values()
            .map(|env_secrets| env_secrets.app.len())
            .sum()
    }
}

/// Sorted representation for deterministic JSON output
fn sorted_environments(
    environments: &HashMap<String, EnvironmentSecrets>,
) -> BTreeMap<&String, SortedEnvironmentSecrets<'_>> {
    environments
        .iter()
        .map(|(env_name, env_secrets)| {
            let sorted_app = env_secrets.app.iter().collect::<BTreeMap<_, _>>();
            let sorted_storages = env_secrets.storages.iter().collect::<BTreeMap<_, _>>();
            (
                env_name,
                SortedEnvironmentSecrets {
                    key_id: &env_secrets.key_id,
                    app: sorted_app,
                    storages: sorted_storages,
                },
            )
        })
        .collect()
}

#[derive(Serialize)]
struct SortedEnvironmentSecrets<'a> {
    key_id: &'a str,
    app: BTreeMap<&'a String, &'a String>,
    storages: BTreeMap<&'a String, &'a super::EncryptedStorageCredentials>,
}

/// Represents a secret that is missing in some environments
#[derive(Debug, Clone, PartialEq)]
pub struct SecretDiscrepancy {
    pub name: String,
    pub present_in: Vec<String>,
    pub missing_in: Vec<String>,
}

/// Validate environment name format
pub fn validate_environment_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(ConfigError::Validation(
            "Environment name cannot be empty".to_string(),
        ));
    }

    // Only lowercase letters, numbers, and hyphens
    for c in name.chars() {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '-' {
            return Err(ConfigError::Validation(format!(
                "Environment name can only contain lowercase letters, numbers, and hyphens. Found: '{}'",
                c
            )));
        }
    }

    Ok(())
}

/// Validate secret name format (uppercase, underscores, numbers)
fn validate_secret_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(ConfigError::Validation(
            "Secret name cannot be empty".to_string(),
        ));
    }

    // Must start with uppercase letter
    if !name
        .chars()
        .next()
        .map(|c| c.is_ascii_uppercase())
        .unwrap_or(false)
    {
        return Err(ConfigError::Validation(
            "Secret name must start with an uppercase letter".to_string(),
        ));
    }

    // Only uppercase letters, numbers, and underscores
    for c in name.chars() {
        if !c.is_ascii_uppercase() && !c.is_ascii_digit() && c != '_' {
            return Err(ConfigError::Validation(format!(
                "Secret name can only contain uppercase letters, numbers, and underscores. Found: '{}'",
                c
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests;

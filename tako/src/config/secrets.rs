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
    /// Map of secret name to encrypted value
    pub secrets: HashMap<String, String>,
}

/// Secrets storage from .tako/secrets.json
///
/// Format:
/// ```json
/// {
///   "production": {
///     "key_id": "0123456789abcdef",
///     "secrets": {
///       "DATABASE_URL": "encrypted_base64_value",
///       "API_KEY": "encrypted_base64_value"
///     }
///   }
/// }
/// ```
///
/// Secret names are plaintext (allows listing without decryption).
/// Secret values are encrypted with AES-256-GCM.
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
            for secret_name in env_secrets.secrets.keys() {
                validate_secret_name(secret_name)?;
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
            .and_then(|env_secrets| env_secrets.secrets.get(name))
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

        env_secrets.secrets.insert(name.to_string(), value);
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
                    secrets: HashMap::new(),
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
                secrets: HashMap::new(),
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

        if env_secrets.secrets.remove(name).is_none() {
            return Err(ConfigError::SecretNotFound(name.to_string()));
        }

        // Remove environment if no secrets remain.
        if env_secrets.secrets.is_empty() {
            self.environments.remove(env);
        }

        Ok(())
    }

    /// Remove a secret from all environments
    pub fn remove_all(&mut self, name: &str) -> Result<Vec<String>> {
        let mut removed_from = Vec::new();

        for (env_name, env_secrets) in &mut self.environments {
            if env_secrets.secrets.remove(name).is_some() {
                removed_from.push(env_name.clone());
            }
        }

        // Remove empty environments
        self.environments
            .retain(|_, env_secrets| !env_secrets.secrets.is_empty());

        if removed_from.is_empty() {
            return Err(ConfigError::SecretNotFound(name.to_string()));
        }

        Ok(removed_from)
    }

    /// Check if a secret exists in an environment
    pub fn contains(&self, env: &str, name: &str) -> bool {
        self.environments
            .get(env)
            .map(|env_secrets| env_secrets.secrets.contains_key(name))
            .unwrap_or(false)
    }

    /// Get all secret names across all environments
    pub fn all_secret_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .environments
            .values()
            .flat_map(|env_secrets| env_secrets.secrets.keys().cloned())
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
            .map(|env_secrets| &env_secrets.secrets)
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
            .map(|(env, env_secrets)| (env.clone(), env_secrets.secrets.len()))
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
            .map(|env_secrets| env_secrets.secrets.len())
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
            let sorted_secrets = env_secrets.secrets.iter().collect::<BTreeMap<_, _>>();
            (
                env_name,
                SortedEnvironmentSecrets {
                    key_id: &env_secrets.key_id,
                    secrets: sorted_secrets,
                },
            )
        })
        .collect()
}

#[derive(Serialize)]
struct SortedEnvironmentSecrets<'a> {
    key_id: &'a str,
    secrets: BTreeMap<&'a String, &'a String>,
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
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ==================== Parsing Tests ====================

    #[test]
    fn test_parse_empty() {
        let store = SecretsStore::parse("").unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn test_parse_empty_object() {
        let store = SecretsStore::parse("{}").unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn test_parse_new_format() {
        let json = r#"{
            "production": {
                "key_id": "0123456789abcdef",
                "secrets": {
                    "DATABASE_URL": "encrypted_value_1",
                    "API_KEY": "encrypted_value_2"
                }
            }
        }"#;

        let store = SecretsStore::parse(json).unwrap();
        assert_eq!(store.environment_names(), vec!["production"]);
        assert_eq!(
            store.get("production", "DATABASE_URL"),
            Some(&"encrypted_value_1".to_string())
        );
        assert_eq!(
            store.get("production", "API_KEY"),
            Some(&"encrypted_value_2".to_string())
        );
        assert_eq!(store.get_key_id("production"), Some("0123456789abcdef"));
    }

    #[test]
    fn test_parse_multiple_environments() {
        let json = r#"{
            "production": {
                "key_id": "1111111111111111",
                "secrets": {
                    "DATABASE_URL": "prod_db"
                }
            },
            "staging": {
                "key_id": "2222222222222222",
                "secrets": {
                    "DATABASE_URL": "staging_db",
                    "DEBUG": "true"
                }
            }
        }"#;

        let store = SecretsStore::parse(json).unwrap();

        let mut envs = store.environment_names();
        envs.sort();
        assert_eq!(envs, vec!["production", "staging"]);

        assert_eq!(
            store.get("production", "DATABASE_URL"),
            Some(&"prod_db".to_string())
        );
        assert_eq!(
            store.get("staging", "DATABASE_URL"),
            Some(&"staging_db".to_string())
        );
        assert_eq!(store.get("staging", "DEBUG"), Some(&"true".to_string()));
    }

    // ==================== Validation Tests ====================

    #[test]
    fn test_validate_secret_name_valid() {
        assert!(validate_secret_name("DATABASE_URL").is_ok());
        assert!(validate_secret_name("API_KEY").is_ok());
        assert!(validate_secret_name("SECRET123").is_ok());
        assert!(validate_secret_name("A").is_ok());
        assert!(validate_secret_name("MY_SECRET_KEY_123").is_ok());
    }

    #[test]
    fn test_validate_secret_name_empty() {
        assert!(validate_secret_name("").is_err());
    }

    #[test]
    fn test_validate_secret_name_must_start_uppercase() {
        assert!(validate_secret_name("database_url").is_err());
        assert!(validate_secret_name("1SECRET").is_err());
        assert!(validate_secret_name("_SECRET").is_err());
    }

    #[test]
    fn test_validate_secret_name_invalid_chars() {
        assert!(validate_secret_name("DATABASE-URL").is_err());
        assert!(validate_secret_name("DATABASE.URL").is_err());
        assert!(validate_secret_name("database_url").is_err());
    }

    #[test]
    fn test_validate_environment_name_valid() {
        assert!(validate_environment_name("production").is_ok());
        assert!(validate_environment_name("staging").is_ok());
        assert!(validate_environment_name("prod-1").is_ok());
    }

    #[test]
    fn test_validate_environment_name_invalid() {
        assert!(validate_environment_name("").is_err());
        assert!(validate_environment_name("Production").is_err());
        assert!(validate_environment_name("prod_1").is_err());
    }

    // ==================== CRUD Operation Tests ====================

    #[test]
    fn test_set_secret() {
        let mut store = SecretsStore::default();

        store.ensure_env_key_id("production").unwrap();
        store
            .set("production", "API_KEY", "secret123".to_string())
            .unwrap();

        assert_eq!(
            store.get("production", "API_KEY"),
            Some(&"secret123".to_string())
        );
    }

    #[test]
    fn test_set_secret_requires_initialized_env() {
        let mut store = SecretsStore::default();

        let result = store.set("production", "API_KEY", "secret123".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_ensure_env_key_id_creates_environment() {
        let mut store = SecretsStore::default();

        let key_id1 = store.ensure_env_key_id("production").unwrap();
        let key_id2 = store.ensure_env_key_id("staging").unwrap();

        assert_eq!(store.environment_names().len(), 2);
        // Different environments get different key_ids
        assert_ne!(key_id1, key_id2);
    }

    #[test]
    fn test_ensure_env_key_id_is_idempotent() {
        let mut store = SecretsStore::default();

        let key_id1 = store.ensure_env_key_id("production").unwrap();
        let key_id2 = store.ensure_env_key_id("production").unwrap();

        // Same key_id returned on repeated calls
        assert_eq!(key_id1, key_id2);
    }

    #[test]
    fn test_set_overwrites_existing() {
        let mut store = SecretsStore::default();

        store.ensure_env_key_id("production").unwrap();
        store
            .set("production", "API_KEY", "old_value".to_string())
            .unwrap();
        store
            .set("production", "API_KEY", "new_value".to_string())
            .unwrap();

        assert_eq!(
            store.get("production", "API_KEY"),
            Some(&"new_value".to_string())
        );
    }

    #[test]
    fn test_remove_secret() {
        let mut store = SecretsStore::default();

        store.ensure_env_key_id("production").unwrap();
        store
            .set("production", "API_KEY", "secret".to_string())
            .unwrap();
        store
            .set("production", "DATABASE_URL", "db".to_string())
            .unwrap();

        store.remove("production", "API_KEY").unwrap();

        assert!(!store.contains("production", "API_KEY"));
        assert!(store.contains("production", "DATABASE_URL"));
    }

    #[test]
    fn test_remove_last_secret_removes_environment() {
        let mut store = SecretsStore::default();

        store.ensure_env_key_id("production").unwrap();
        store
            .set("production", "API_KEY", "secret".to_string())
            .unwrap();
        store.remove("production", "API_KEY").unwrap();

        assert!(!store.environments.contains_key("production"));
    }

    #[test]
    fn test_remove_nonexistent_fails() {
        let mut store = SecretsStore::default();
        store.ensure_env_key_id("production").unwrap();
        store
            .set("production", "API_KEY", "secret".to_string())
            .unwrap();

        let result = store.remove("production", "NONEXISTENT");
        assert!(matches!(result, Err(ConfigError::SecretNotFound(_))));
    }

    #[test]
    fn test_remove_from_nonexistent_env_fails() {
        let mut store = SecretsStore::default();

        let result = store.remove("production", "API_KEY");
        assert!(matches!(result, Err(ConfigError::EnvironmentNotFound(_))));
    }

    #[test]
    fn test_remove_all() {
        let mut store = SecretsStore::default();

        store.ensure_env_key_id("production").unwrap();
        store
            .set("production", "API_KEY", "prod".to_string())
            .unwrap();
        store.ensure_env_key_id("staging").unwrap();
        store
            .set("staging", "API_KEY", "staging".to_string())
            .unwrap();
        store
            .set("staging", "DATABASE_URL", "db".to_string())
            .unwrap();

        let removed_from = store.remove_all("API_KEY").unwrap();

        assert_eq!(removed_from.len(), 2);
        assert!(!store.contains("production", "API_KEY"));
        assert!(!store.contains("staging", "API_KEY"));
        assert!(store.contains("staging", "DATABASE_URL"));

        // production environment should be removed (was only API_KEY)
        assert!(!store.environments.contains_key("production"));
    }

    // ==================== Discrepancy Tests ====================

    #[test]
    fn test_find_discrepancies_none() {
        let mut store = SecretsStore::default();

        store.ensure_env_key_id("production").unwrap();
        store
            .set("production", "API_KEY", "prod".to_string())
            .unwrap();
        store
            .set("production", "DATABASE_URL", "prod_db".to_string())
            .unwrap();
        store.ensure_env_key_id("staging").unwrap();
        store
            .set("staging", "API_KEY", "staging".to_string())
            .unwrap();
        store
            .set("staging", "DATABASE_URL", "staging_db".to_string())
            .unwrap();

        assert!(store.is_consistent());
        assert!(store.find_discrepancies().is_empty());
    }

    #[test]
    fn test_find_discrepancies_some() {
        let mut store = SecretsStore::default();

        store.ensure_env_key_id("production").unwrap();
        store
            .set("production", "API_KEY", "prod".to_string())
            .unwrap();
        store
            .set("production", "DATABASE_URL", "prod_db".to_string())
            .unwrap();
        store.ensure_env_key_id("staging").unwrap();
        store
            .set("staging", "API_KEY", "staging".to_string())
            .unwrap();
        // DATABASE_URL missing in staging

        let discrepancies = store.find_discrepancies();
        assert_eq!(discrepancies.len(), 1);
        assert_eq!(discrepancies[0].name, "DATABASE_URL");
        assert_eq!(discrepancies[0].missing_in, vec!["staging"]);
    }

    #[test]
    fn test_all_secret_names() {
        let mut store = SecretsStore::default();

        store.ensure_env_key_id("production").unwrap();
        store.set("production", "API_KEY", "1".to_string()).unwrap();
        store
            .set("production", "DATABASE_URL", "2".to_string())
            .unwrap();
        store.ensure_env_key_id("staging").unwrap();
        store.set("staging", "API_KEY", "3".to_string()).unwrap();
        store.set("staging", "REDIS_URL", "4".to_string()).unwrap();

        let names = store.all_secret_names();
        assert_eq!(names, vec!["API_KEY", "DATABASE_URL", "REDIS_URL"]);
    }

    // ==================== File I/O Tests ====================

    #[test]
    fn test_save_and_load() {
        let temp_dir = TempDir::new().unwrap();

        let mut store = SecretsStore::default();
        store.ensure_env_key_id("production").unwrap();
        store
            .set("production", "API_KEY", "secret123".to_string())
            .unwrap();
        store.ensure_env_key_id("staging").unwrap();
        store
            .set("staging", "API_KEY", "secret456".to_string())
            .unwrap();

        store.save_to_dir(&temp_dir).unwrap();

        let loaded = SecretsStore::load_from_dir(&temp_dir).unwrap();

        assert_eq!(
            loaded.get("production", "API_KEY"),
            Some(&"secret123".to_string())
        );
        assert_eq!(
            loaded.get("staging", "API_KEY"),
            Some(&"secret456".to_string())
        );
        // Key ids are preserved
        assert_eq!(
            loaded.get_key_id("production"),
            store.get_key_id("production")
        );
    }

    #[test]
    fn test_default_path_uses_secrets_json() {
        let temp_dir = TempDir::new().unwrap();
        assert_eq!(
            SecretsStore::default_path(temp_dir.path()),
            temp_dir.path().join(".tako").join("secrets.json")
        );
    }

    #[test]
    fn test_load_nonexistent_returns_default() {
        let temp_dir = TempDir::new().unwrap();
        let store = SecretsStore::load_from_dir(&temp_dir).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn test_save_to_dir_writes_new_secrets_json_path() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = SecretsStore::default();
        store.ensure_env_key_id("production").unwrap();
        store
            .set("production", "API_KEY", "secret123".to_string())
            .unwrap();

        store.save_to_dir(temp_dir.path()).unwrap();

        assert!(temp_dir.path().join(".tako").join("secrets.json").exists());
        assert!(!temp_dir.path().join(".tako").join("secrets").exists());
    }

    #[test]
    fn test_save_to_file_orders_environments_and_secret_names_stably() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join(".tako").join("secrets.json");
        let mut store = SecretsStore::default();
        store.ensure_env_key_id("staging").unwrap();
        store.set("staging", "Z_KEY", "z".to_string()).unwrap();
        store.ensure_env_key_id("production").unwrap();
        store.set("production", "B_KEY", "b".to_string()).unwrap();
        store.set("production", "A_KEY", "a".to_string()).unwrap();

        store.save_to_file(&path).unwrap();

        let raw = fs::read_to_string(path).unwrap();
        let production_pos = raw.find("\"production\"").unwrap();
        let staging_pos = raw.find("\"staging\"").unwrap();
        let a_key_pos = raw.find("\"A_KEY\"").unwrap();
        let b_key_pos = raw.find("\"B_KEY\"").unwrap();

        assert!(
            production_pos < staging_pos,
            "expected sorted environments: {raw}"
        );
        assert!(a_key_pos < b_key_pos, "expected sorted secret names: {raw}");
    }

    #[test]
    fn test_creates_parent_directory() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir
            .path()
            .join("subdir")
            .join(".tako")
            .join("secrets.json");

        let mut store = SecretsStore::default();
        store.ensure_env_key_id("production").unwrap();
        store
            .set("production", "API_KEY", "secret".to_string())
            .unwrap();
        store.save_to_file(&path).unwrap();

        assert!(path.exists());
    }

    // ==================== Utility Tests ====================

    #[test]
    fn test_count_by_env() {
        let mut store = SecretsStore::default();

        store.ensure_env_key_id("production").unwrap();
        store.set("production", "API_KEY", "1".to_string()).unwrap();
        store
            .set("production", "DATABASE_URL", "2".to_string())
            .unwrap();
        store.ensure_env_key_id("staging").unwrap();
        store.set("staging", "API_KEY", "3".to_string()).unwrap();

        let counts = store.count_by_env();
        assert_eq!(counts.get("production"), Some(&2));
        assert_eq!(counts.get("staging"), Some(&1));
    }

    #[test]
    fn test_total_count() {
        let mut store = SecretsStore::default();

        store.ensure_env_key_id("production").unwrap();
        store.set("production", "API_KEY", "1".to_string()).unwrap();
        store
            .set("production", "DATABASE_URL", "2".to_string())
            .unwrap();
        store.ensure_env_key_id("staging").unwrap();
        store.set("staging", "API_KEY", "3".to_string()).unwrap();

        assert_eq!(store.total_count(), 3);
    }
}

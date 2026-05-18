use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use time::{Date, Duration, Month, OffsetDateTime};

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
    pub app: HashMap<String, EncryptedSecretValue>,
    /// Storage resource name to encrypted credentials.
    #[serde(default)]
    pub storages: HashMap<String, super::EncryptedStorageCredentials>,
    /// DNS credentials for wildcard certificate issuance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<EncryptedDnsCredentials>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EncryptedSecretValue {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_on: Option<String>,
}

impl EncryptedSecretValue {
    pub fn new(value: String, expires_on: Option<String>) -> Self {
        Self { value, expires_on }
    }

    pub fn is_expired(&self) -> Result<bool> {
        self.is_expired_at(current_unix_timestamp())
    }

    pub fn is_expired_at(&self, now_unix_secs: i64) -> Result<bool> {
        let Some(expires_on) = &self.expires_on else {
            return Ok(false);
        };
        Ok(parse_secret_expires_on_unix(expires_on)? <= now_unix_secs)
    }

    pub fn is_expiring_within_days(&self, days: i64) -> Result<bool> {
        self.is_expiring_within_days_at(current_unix_timestamp(), days)
    }

    pub fn is_expiring_within_days_at(&self, now_unix_secs: i64, days: i64) -> Result<bool> {
        let Some(expires_on) = &self.expires_on else {
            return Ok(false);
        };
        let expires_on_unix = parse_secret_expires_on_unix(expires_on)?;
        let window_secs = days.max(0).saturating_mul(24 * 60 * 60);
        Ok(expires_on_unix > now_unix_secs
            && expires_on_unix <= now_unix_secs.saturating_add(window_secs))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EncryptedDnsCredentials {
    pub cloudflare_api_token: EncryptedSecretValue,
}

pub fn secret_expires_on_prompt_hint() -> &'static str {
    "Optional. Use YYYY-MM-DD, in 30 days, never, or leave blank."
}

pub fn normalize_secret_expires_on(input: &str) -> Result<Option<String>> {
    normalize_secret_expires_on_at(input, OffsetDateTime::now_utc())
}

fn normalize_secret_expires_on_at(input: &str, now: OffsetDateTime) -> Result<Option<String>> {
    let trimmed = input.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("never")
        || trimmed.eq_ignore_ascii_case("none")
    {
        return Ok(None);
    }

    if trimmed.len() == 10 && trimmed.as_bytes().get(4) == Some(&b'-') {
        let (year, month, day) = parse_secret_expiry_date(trimmed)?;
        return Ok(Some(format!("{year:04}-{month:02}-{day:02}")));
    }

    if let Some(days) = parse_relative_secret_expiry_days(trimmed)? {
        let target = now
            .checked_add(Duration::days(days))
            .ok_or_else(|| invalid_secret_expires_on(trimmed))?;
        let date = target.date();
        return Ok(Some(format!(
            "{:04}-{:02}-{:02}",
            date.year(),
            u8::from(date.month()),
            date.day()
        )));
    }

    Err(invalid_secret_expires_on(trimmed))
}

pub fn current_unix_timestamp() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// Secrets storage from .tako/secrets.json
///
/// Format:
/// ```json
/// {
///   "production": {
///     "key_id": "0123456789abcdef",
///     "app": {
///       "DATABASE_URL": {
///         "value": "encrypted_base64_value",
///         "expires_on": "2026-12-31"
///       },
///       "API_KEY": {
///         "value": "encrypted_base64_value"
///       }
///     },
///     "storages": {
///       "prod_uploads": {
///         "access_key_id": {
///           "value": "encrypted_base64_value",
///           "expires_on": "2026-12-31"
///         },
///         "secret_access_key": {
///           "value": "encrypted_base64_value",
///           "expires_on": "2026-12-31"
///         }
///       }
///     },
///     "dns": {
///       "cloudflare_api_token": {
///         "value": "encrypted_base64_value",
///         "expires_on": "2026-12-31"
///       }
///     }
///   }
/// }
/// ```
///
/// App secret names and storage resource names are plaintext (allows listing
/// without decryption). Secret values, storage credentials, and DNS credentials
/// are encrypted with AES-256-GCM.
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
            for (secret_name, secret) in &env_secrets.app {
                validate_secret_name(secret_name)?;
                validate_encrypted_secret_value(&format!("Secret '{secret_name}' value"), secret)?;
            }
            for (storage_name, credentials) in &env_secrets.storages {
                super::validate_storage_name(storage_name)?;
                validate_encrypted_secret_value(
                    &format!("Storage '{storage_name}' access key id"),
                    &credentials.access_key_id,
                )?;
                validate_encrypted_secret_value(
                    &format!("Storage '{storage_name}' secret access key"),
                    &credentials.secret_access_key,
                )?;
            }
            if let Some(credentials) = &env_secrets.dns {
                validate_encrypted_secret_value(
                    "Cloudflare API token",
                    &credentials.cloudflare_api_token,
                )?;
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
        self.get_secret(env, name).map(|secret| &secret.value)
    }

    /// Get a secret entry for an environment
    pub fn get_secret(&self, env: &str, name: &str) -> Option<&EncryptedSecretValue> {
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
        self.set_with_expires_on(env, name, value, None)
    }

    /// Set a secret value and expiry for an environment (key_id must already exist).
    pub fn set_with_expires_on(
        &mut self,
        env: &str,
        name: &str,
        value: String,
        expires_on: Option<String>,
    ) -> Result<()> {
        validate_environment_name(env)?;
        validate_secret_name(name)?;
        let secret = EncryptedSecretValue::new(value, normalize_optional_expires_on(expires_on)?);
        validate_encrypted_secret_value(&format!("Secret '{name}' value"), &secret)?;

        let env_secrets = self.environments.get_mut(env).ok_or_else(|| {
            ConfigError::Validation(format!(
                "Environment '{}' not initialized. Call ensure_env_key_id first.",
                env
            ))
        })?;

        env_secrets.app.insert(name.to_string(), secret);
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
                    dns: None,
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
                dns: None,
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
        if env_secrets.app.is_empty()
            && env_secrets.storages.is_empty()
            && env_secrets.dns.is_none()
        {
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
            !env_secrets.app.is_empty()
                || !env_secrets.storages.is_empty()
                || env_secrets.dns.is_some()
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
    pub fn get_env(&self, env: &str) -> Option<&HashMap<String, EncryptedSecretValue>> {
        self.get_env_secret_entries(env)
    }

    pub fn get_env_secret_entries(
        &self,
        env: &str,
    ) -> Option<&HashMap<String, EncryptedSecretValue>> {
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
        validate_encrypted_secret_value("Storage access key id", &value.access_key_id)?;
        validate_encrypted_secret_value("Storage secret access key", &value.secret_access_key)?;

        let env_secrets = self.environments.get_mut(env).ok_or_else(|| {
            ConfigError::Validation(format!(
                "Environment '{}' not initialized. Call ensure_env_key_id first.",
                env
            ))
        })?;

        env_secrets.storages.insert(resource.to_string(), value);
        Ok(())
    }

    pub fn get_dns_credentials(&self, env: &str) -> Option<&EncryptedDnsCredentials> {
        self.environments
            .get(env)
            .and_then(|env_secrets| env_secrets.dns.as_ref())
    }

    pub fn set_dns_credentials(&mut self, env: &str, value: EncryptedDnsCredentials) -> Result<()> {
        validate_environment_name(env)?;
        validate_encrypted_secret_value("Cloudflare API token", &value.cloudflare_api_token)?;

        let env_secrets = self.environments.get_mut(env).ok_or_else(|| {
            ConfigError::Validation(format!(
                "Environment '{}' not initialized. Call ensure_env_key_id first.",
                env
            ))
        })?;

        env_secrets.dns = Some(value);
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
                    dns: env_secrets.dns.as_ref(),
                },
            )
        })
        .collect()
}

#[derive(Serialize)]
struct SortedEnvironmentSecrets<'a> {
    key_id: &'a str,
    app: BTreeMap<&'a String, &'a EncryptedSecretValue>,
    storages: BTreeMap<&'a String, &'a super::EncryptedStorageCredentials>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dns: Option<&'a EncryptedDnsCredentials>,
}

fn normalize_optional_expires_on(expires_on: Option<String>) -> Result<Option<String>> {
    match expires_on {
        Some(value) => normalize_secret_expires_on(&value),
        None => Ok(None),
    }
}

fn parse_relative_secret_expiry_days(value: &str) -> Result<Option<i64>> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 3
        || !parts[0].eq_ignore_ascii_case("in")
        || !(parts[2].eq_ignore_ascii_case("day") || parts[2].eq_ignore_ascii_case("days"))
    {
        return Ok(None);
    }

    let days = parse_secret_expiry_number::<i64>(Some(parts[1]), value)?;
    if days <= 0 {
        return Err(invalid_secret_expires_on(value));
    }
    Ok(Some(days))
}

fn validate_encrypted_secret_value(label: &str, secret: &EncryptedSecretValue) -> Result<()> {
    if secret.value.trim().is_empty() {
        return Err(ConfigError::Validation(format!("{label} cannot be empty")));
    }
    if let Some(expires_on) = &secret.expires_on {
        parse_secret_expires_on_unix(expires_on)?;
    }
    Ok(())
}

fn parse_secret_expires_on_unix(value: &str) -> Result<i64> {
    let (year, month, day) = parse_secret_expiry_date(value)?;
    let month = Month::try_from(month).map_err(|_| invalid_secret_expires_on(value))?;
    let date =
        Date::from_calendar_date(year, month, day).map_err(|_| invalid_secret_expires_on(value))?;
    Ok(date.midnight().assume_utc().unix_timestamp())
}

fn parse_secret_expiry_date(value: &str) -> Result<(i32, u8, u8)> {
    if value.len() != "YYYY-MM-DD".len()
        || value.as_bytes().get(4) != Some(&b'-')
        || value.as_bytes().get(7) != Some(&b'-')
    {
        return Err(invalid_secret_expires_on(value));
    }
    let mut parts = value.split('-');
    let year = parse_secret_expiry_number::<i32>(parts.next(), value)?;
    let month = parse_secret_expiry_number::<u8>(parts.next(), value)?;
    let day = parse_secret_expiry_number::<u8>(parts.next(), value)?;
    if parts.next().is_some() {
        return Err(invalid_secret_expires_on(value));
    }
    let month_value = Month::try_from(month).map_err(|_| invalid_secret_expires_on(value))?;
    Date::from_calendar_date(year, month_value, day)
        .map_err(|_| invalid_secret_expires_on(value))?;
    Ok((year, month, day))
}

fn parse_secret_expiry_number<T: std::str::FromStr>(
    value: Option<&str>,
    full_value: &str,
) -> Result<T> {
    value
        .filter(|part| !part.is_empty())
        .ok_or_else(|| invalid_secret_expires_on(full_value))?
        .parse::<T>()
        .map_err(|_| invalid_secret_expires_on(full_value))
}

fn invalid_secret_expires_on(value: &str) -> ConfigError {
    ConfigError::Validation(format!(
        "Invalid secret expiry '{value}'. {}",
        secret_expires_on_prompt_hint()
    ))
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

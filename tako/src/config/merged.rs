use std::collections::HashMap;
use std::path::Path;

use crate::app::require_app_name_from_config;

use super::error::{ConfigError, Result};
use super::secrets::SecretsStore;
use super::servers_toml::{ServerEntry, ServersToml};
use super::tako_toml::TakoToml;

/// Merged configuration from all sources
#[derive(Debug, Clone)]
pub struct MergedConfig {
    /// Project configuration from tako.toml
    pub project: TakoToml,

    /// Global servers from config.toml [[servers]]
    pub global_servers: ServersToml,

    /// Secrets from project .tako/secrets.json
    pub secrets: SecretsStore,

    /// Resolved app name (top-level `name` when set, otherwise directory fallback)
    pub app_name: String,
}

/// Resolved environment configuration with all values merged
#[derive(Debug, Clone)]
pub struct ResolvedEnv {
    /// Environment name
    pub name: String,

    /// Routes for this environment
    pub routes: Vec<String>,

    /// Merged variables (global + per-env from tako.toml)
    pub vars: HashMap<String, String>,

    /// Secret names required for this environment
    pub secret_names: Vec<String>,

    /// Servers assigned to this environment
    pub servers: Vec<ResolvedServer>,
}

/// Resolved server configuration with defaults applied
#[derive(Debug, Clone)]
pub struct ResolvedServer {
    /// Server name referenced from `[envs.<name>].servers`.
    pub name: String,

    /// SSH connection details from global config
    pub connection: ServerEntry,
}

impl MergedConfig {
    /// Load configuration from a project directory
    pub fn load_from_dir<P: AsRef<Path>>(dir: P) -> Result<Self> {
        let dir = dir.as_ref();

        // Load tako.toml
        let project = TakoToml::load_from_dir(dir)?;

        let app_name = require_app_name_from_config(dir)
            .map_err(|e| ConfigError::Validation(e.to_string()))?;

        // Load global servers
        let global_servers = ServersToml::load()?;

        // Load secrets from project directory
        let secrets = SecretsStore::load_from_dir(dir)?;

        Ok(Self {
            project,
            global_servers,
            secrets,
            app_name,
        })
    }

    /// Load configuration with explicit paths (for testing)
    pub fn load_with_paths<P: AsRef<Path>>(
        project_dir: P,
        servers_path: Option<&Path>,
        secrets_path: Option<&Path>,
    ) -> Result<Self> {
        let dir = project_dir.as_ref();

        // Load tako.toml
        let project = TakoToml::load_from_dir(dir)?;

        let app_name = require_app_name_from_config(dir)
            .map_err(|e| ConfigError::Validation(e.to_string()))?;

        // Load global servers
        let global_servers = if let Some(path) = servers_path {
            if path.exists() {
                ServersToml::load_from_file(path)?
            } else {
                ServersToml::default()
            }
        } else {
            ServersToml::load()?
        };

        // Load secrets
        let secrets = if let Some(path) = secrets_path {
            if path.exists() {
                SecretsStore::load_from_file(path)?
            } else {
                SecretsStore::default()
            }
        } else {
            SecretsStore::load_from_dir(dir)?
        };

        Ok(Self {
            project,
            global_servers,
            secrets,
            app_name,
        })
    }

    /// Get all environment names
    pub fn get_environment_names(&self) -> Vec<String> {
        self.project.get_environment_names()
    }

    /// Resolve a specific environment with all values merged
    pub fn resolve_env(&self, env_name: &str) -> Result<ResolvedEnv> {
        // Check environment exists
        if !self.project.envs.contains_key(env_name) {
            return Err(ConfigError::EnvironmentNotFound(env_name.to_string()));
        }

        // Get routes
        let routes = self.project.get_routes(env_name).unwrap_or_default();

        // Get merged vars
        let vars = self.project.get_merged_vars(env_name);

        // Get secret names for this environment
        let secret_names = self
            .secrets
            .get_env(env_name)
            .map(|secrets| secrets.keys().cloned().collect())
            .unwrap_or_default();

        // Get and resolve servers for this environment
        let server_names = self.project.get_servers_for_env(env_name);
        let mut servers = Vec::new();

        for server_name in server_names {
            let server = self.resolve_server(server_name)?;
            servers.push(server);
        }

        Ok(ResolvedEnv {
            name: env_name.to_string(),
            routes,
            vars,
            secret_names,
            servers,
        })
    }

    /// Resolve a specific server with defaults applied
    pub fn resolve_server(&self, server_name: &str) -> Result<ResolvedServer> {
        if !self
            .project
            .envs
            .values()
            .any(|env| env.servers.iter().any(|name| name == server_name))
        {
            return Err(ConfigError::ServerNotFound(server_name.to_string()));
        }

        // Get connection details from global config
        let connection = self
            .global_servers
            .get(server_name)
            .ok_or_else(|| {
                ConfigError::Validation(format!(
                    "Server '{}' is configured in tako.toml but not found in config.toml [[servers]]. \
                      Run 'tako servers add --name {} <host>' to add it.",
                    server_name, server_name
                ))
            })?
            .clone();

        Ok(ResolvedServer {
            name: server_name.to_string(),
            connection,
        })
    }

    /// Validate that all secrets are consistent across environments
    pub fn validate_secrets(&self) -> Result<()> {
        let discrepancies = self.secrets.find_discrepancies();
        if !discrepancies.is_empty() {
            let missing_list: Vec<String> = discrepancies
                .iter()
                .map(|d| format!("{} (missing in: {})", d.name, d.missing_in.join(", ")))
                .collect();
            return Err(ConfigError::Validation(format!(
                "Secret discrepancies found:\n  {}",
                missing_list.join("\n  ")
            )));
        }
        Ok(())
    }

    /// Validate that all configured servers exist in global config
    pub fn validate_servers(&self) -> Result<()> {
        let mut missing = Vec::new();

        for server_name in self
            .project
            .envs
            .values()
            .flat_map(|env| env.servers.iter())
        {
            if !self.global_servers.contains(server_name) {
                missing.push(server_name.clone());
            }
        }

        if !missing.is_empty() {
            return Err(ConfigError::Validation(format!(
                "Servers configured in tako.toml but not found in config.toml [[servers]]: {}",
                missing.join(", ")
            )));
        }

        Ok(())
    }

    /// Validate that all secrets required for deployment exist
    pub fn validate_secrets_for_env(&self, env_name: &str) -> Result<()> {
        // For now, we just check that the environment has secrets if other envs do
        // This can be extended to check against a required secrets list
        if !self.secrets.is_consistent() {
            let discrepancies = self.secrets.find_discrepancies();
            for discrepancy in &discrepancies {
                if discrepancy.missing_in.contains(&env_name.to_string()) {
                    return Err(ConfigError::Validation(format!(
                        "Secret '{}' is missing for environment '{}'. \
                         Run 'tako secret set --env {} {}' to set it.",
                        discrepancy.name, env_name, env_name, discrepancy.name
                    )));
                }
            }
        }
        Ok(())
    }

    /// Full validation before deployment
    pub fn validate_for_deployment(&self, env_name: &str) -> Result<()> {
        // Environment must exist
        if !self.project.envs.contains_key(env_name) {
            return Err(ConfigError::EnvironmentNotFound(env_name.to_string()));
        }

        // Servers must exist
        self.validate_servers()?;

        // Secrets must be consistent
        self.validate_secrets_for_env(env_name)?;

        // At least one server must be assigned to this environment
        let servers = self.project.get_servers_for_env(env_name);
        if servers.is_empty() {
            return Err(ConfigError::Validation(format!(
                "No servers configured for environment '{}'. \
                 Add `servers = [\"<name>\"]` under [envs.{}] in tako.toml.",
                env_name, env_name
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_project() -> TempDir {
        let temp_dir = TempDir::new().unwrap();

        // Create tako.toml
        let tako_toml = r#"
name = "test-app"

[vars]
API_URL = "https://example.com"

[vars.production]
DATABASE_URL = "postgres://prod"

[envs.production]
route = "api.example.com"
servers = ["prod-1"]

[envs.staging]
route = "staging.example.com"
servers = ["staging-1"]
"#;
        fs::write(temp_dir.path().join("tako.toml"), tako_toml).unwrap();

        temp_dir
    }

    fn setup_servers_toml(temp_dir: &TempDir) -> std::path::PathBuf {
        let servers_path = temp_dir.path().join("config.toml");
        let servers_toml = r#"
[[servers]]
name = "prod-1"
host = "1.2.3.4"

[[servers]]
name = "staging-1"
host = "5.6.7.8"
"#;
        fs::write(&servers_path, servers_toml).unwrap();
        servers_path
    }

    fn setup_secrets(temp_dir: &TempDir) -> std::path::PathBuf {
        let secrets_path = temp_dir.path().join("secrets.json");
        let secrets_json = r#"{
            "production": {
                "key_id": "0123456789abcdef",
                "secrets": {
                    "API_KEY": "prod-key",
                    "DB_URL": "postgres://prod"
                }
            },
            "staging": {
                "key_id": "fedcba9876543210",
                "secrets": {
                    "API_KEY": "staging-key",
                    "DB_URL": "postgres://staging"
                }
            }
        }"#;
        fs::write(&secrets_path, secrets_json).unwrap();
        secrets_path
    }

    #[test]
    fn test_load_merged_config() {
        let temp_dir = setup_test_project();
        let servers_path = setup_servers_toml(&temp_dir);
        let secrets_path = setup_secrets(&temp_dir);

        let config = MergedConfig::load_with_paths(
            temp_dir.path(),
            Some(&servers_path),
            Some(&secrets_path),
        )
        .unwrap();

        assert_eq!(config.app_name, "test-app");
        assert_eq!(config.get_environment_names().len(), 2);
    }

    #[test]
    fn test_resolve_env() {
        let temp_dir = setup_test_project();
        let servers_path = setup_servers_toml(&temp_dir);
        let secrets_path = setup_secrets(&temp_dir);

        let config = MergedConfig::load_with_paths(
            temp_dir.path(),
            Some(&servers_path),
            Some(&secrets_path),
        )
        .unwrap();

        let prod = config.resolve_env("production").unwrap();
        assert_eq!(prod.name, "production");
        assert_eq!(prod.routes, vec!["api.example.com"]);
        assert_eq!(
            prod.vars.get("DATABASE_URL"),
            Some(&"postgres://prod".to_string())
        );
        assert_eq!(prod.servers.len(), 1);
        assert_eq!(prod.servers[0].name, "prod-1");
    }

    #[test]
    fn test_resolve_server() {
        let temp_dir = setup_test_project();
        let servers_path = setup_servers_toml(&temp_dir);
        let secrets_path = setup_secrets(&temp_dir);

        let config = MergedConfig::load_with_paths(
            temp_dir.path(),
            Some(&servers_path),
            Some(&secrets_path),
        )
        .unwrap();

        let server = config.resolve_server("prod-1").unwrap();
        assert_eq!(server.name, "prod-1");
        assert_eq!(server.connection.host, "1.2.3.4");
        assert_eq!(server.connection.port, 22);
    }

    #[test]
    fn test_resolve_server_with_defaults() {
        let temp_dir = setup_test_project();
        let servers_path = setup_servers_toml(&temp_dir);
        let secrets_path = setup_secrets(&temp_dir);

        let config = MergedConfig::load_with_paths(
            temp_dir.path(),
            Some(&servers_path),
            Some(&secrets_path),
        )
        .unwrap();

        let server = config.resolve_server("staging-1").unwrap();
        assert_eq!(server.connection.host, "5.6.7.8");
        assert_eq!(server.connection.port, 22);
    }

    #[test]
    fn test_resolve_nonexistent_env_fails() {
        let temp_dir = setup_test_project();
        let servers_path = setup_servers_toml(&temp_dir);
        let secrets_path = setup_secrets(&temp_dir);

        let config = MergedConfig::load_with_paths(
            temp_dir.path(),
            Some(&servers_path),
            Some(&secrets_path),
        )
        .unwrap();

        let result = config.resolve_env("nonexistent");
        assert!(matches!(result, Err(ConfigError::EnvironmentNotFound(_))));
    }

    #[test]
    fn test_validate_servers_missing() {
        let temp_dir = setup_test_project();
        let secrets_path = setup_secrets(&temp_dir);

        // Don't create any [[servers]] entries - should fail validation
        let empty_servers_path = temp_dir.path().join("empty_config.toml");
        fs::write(&empty_servers_path, "").unwrap();

        let config = MergedConfig::load_with_paths(
            temp_dir.path(),
            Some(&empty_servers_path),
            Some(&secrets_path),
        )
        .unwrap();

        let result = config.validate_servers();
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_secrets_consistent() {
        let temp_dir = setup_test_project();
        let servers_path = setup_servers_toml(&temp_dir);
        let secrets_path = setup_secrets(&temp_dir);

        let config = MergedConfig::load_with_paths(
            temp_dir.path(),
            Some(&servers_path),
            Some(&secrets_path),
        )
        .unwrap();

        // Secrets are consistent in our test setup
        assert!(config.validate_secrets().is_ok());
    }

    #[test]
    fn test_validate_secrets_inconsistent() {
        let temp_dir = setup_test_project();
        let servers_path = setup_servers_toml(&temp_dir);

        // Create inconsistent secrets
        let secrets_path = temp_dir.path().join("secrets.json");
        let secrets_json = r#"{
            "production": {
                "key_id": "0123456789abcdef",
                "secrets": {
                    "API_KEY": "prod-key",
                    "DB_URL": "postgres://prod"
                }
            },
            "staging": {
                "key_id": "fedcba9876543210",
                "secrets": {
                    "API_KEY": "staging-key"
                }
            }
        }"#;
        fs::write(&secrets_path, secrets_json).unwrap();

        let config = MergedConfig::load_with_paths(
            temp_dir.path(),
            Some(&servers_path),
            Some(&secrets_path),
        )
        .unwrap();

        let result = config.validate_secrets();
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_for_deployment() {
        let temp_dir = setup_test_project();
        let servers_path = setup_servers_toml(&temp_dir);
        let secrets_path = setup_secrets(&temp_dir);

        let config = MergedConfig::load_with_paths(
            temp_dir.path(),
            Some(&servers_path),
            Some(&secrets_path),
        )
        .unwrap();

        // Should pass for production
        assert!(config.validate_for_deployment("production").is_ok());

        // Should pass for staging
        assert!(config.validate_for_deployment("staging").is_ok());

        // Should fail for nonexistent
        assert!(config.validate_for_deployment("nonexistent").is_err());
    }

    #[test]
    fn test_load_fails_when_tako_toml_name_is_missing() {
        let temp_dir = TempDir::new().unwrap();
        let project_dir = temp_dir.path().join("sample-app");
        std::fs::create_dir_all(&project_dir).unwrap();

        // Create tako.toml without name
        let tako_toml = r#"
[envs.production]
route = "api.example.com"
"#;
        fs::write(project_dir.join("tako.toml"), tako_toml).unwrap();

        let err = MergedConfig::load_with_paths(&project_dir, None, None).unwrap_err();
        assert!(
            err.to_string().contains("name"),
            "expected error about missing name, got: {}",
            err
        );
    }

    #[test]
    fn test_merged_vars_in_resolved_env() {
        let temp_dir = setup_test_project();
        let servers_path = setup_servers_toml(&temp_dir);
        let secrets_path = setup_secrets(&temp_dir);

        let config = MergedConfig::load_with_paths(
            temp_dir.path(),
            Some(&servers_path),
            Some(&secrets_path),
        )
        .unwrap();

        let prod = config.resolve_env("production").unwrap();
        assert_eq!(
            prod.vars.get("DATABASE_URL"),
            Some(&"postgres://prod".to_string())
        );

        let staging = config.resolve_env("staging").unwrap();
        assert_eq!(
            staging.vars.get("API_URL"),
            Some(&"https://example.com".to_string())
        );
    }
}

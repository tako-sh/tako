use std::collections::{BTreeMap, BTreeSet};

use crate::config::{ConfigError, Result, ServerInventory, TakoToml};

/// Validation result with warnings
#[derive(Debug, Default)]
pub struct ValidationResult {
    /// Critical errors that prevent operation
    pub errors: Vec<String>,
    /// Warnings that should be shown to user
    pub warnings: Vec<String>,
}

impl ValidationResult {
    /// Create a new empty result
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an error
    pub fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }

    /// Add a warning
    pub fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }

    /// Check if there are any errors
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Check if there are any warnings
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// Convert to Result, failing if there are errors
    pub fn into_result(self) -> Result<Vec<String>> {
        if self.has_errors() {
            Err(ConfigError::Validation(self.errors.join("\n")))
        } else {
            Ok(self.warnings)
        }
    }

    /// Merge another result into this one
    pub fn merge(&mut self, other: ValidationResult) {
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
    }
}

/// Validate tako.toml configuration
pub fn validate_tako_toml(config: &TakoToml) -> ValidationResult {
    let mut result = ValidationResult::new();

    // Name is required
    if config.name.as_ref().is_none_or(|n| n.trim().is_empty()) {
        result.error("tako.toml must have a `name` field. Run `tako init` to set it.".to_string());
    }

    // Check for environments without routes
    for (env_name, env_config) in &config.envs {
        let is_development = env_name == "development";

        if config.compose.is_none()
            && !is_development
            && env_config.route.is_none()
            && env_config.routes.is_none()
        {
            result.error(format!(
                "Environment '{}' must define either 'route' or 'routes'",
                env_name
            ));
        }
        if let Some(routes) = &env_config.routes
            && routes.is_empty()
            && !is_development
        {
            result.error(format!(
                "Environment '{}' has empty 'routes'; define at least one route",
                env_name
            ));
        }
    }

    // Check for environments without servers (development is exempt: servers are ignored there)
    for env_name in config.envs.keys() {
        let servers = config.get_servers_for_env(env_name);
        if env_name == "development" {
            if !servers.is_empty() {
                result.warn(
                    "Servers configured for 'development' are ignored; \
                     'development' is only used by 'tako dev'"
                        .to_string(),
                );
            }
        }
    }

    for warning in config.ignored_reserved_var_warnings() {
        result.warn(warning);
    }
    for warning in database_binding_gaps(config) {
        result.warn(warning);
    }
    for warning in databases_without_backups(config) {
        result.warn(warning);
    }

    result
}

/// Databases that deployed environments bind without a backup storage, so
/// their deploys configure no snapshots.
fn databases_without_backups(config: &TakoToml) -> Vec<String> {
    let mut envs: Vec<_> = config
        .envs
        .iter()
        .filter(|(name, _)| name.as_str() != "development")
        .collect();
    envs.sort_by(|a, b| a.0.cmp(b.0));
    let mut warnings = Vec::new();
    for (env, env_config) in envs {
        let mut databases: Vec<&String> = env_config.databases.values().collect();
        databases.sort();
        databases.dedup();
        for database in databases {
            if crate::commands::storage::database_backup_setting(config, env, database).is_none() {
                warnings.push(format!(
                    "Database '{database}' gets no snapshots in '{env}': set [envs.{env}].backup.storage, or `backup = \"<storage>\"` in [databases.{database}], to a private S3 storage."
                ));
            }
        }
        let mut stores: Vec<&String> = env_config.kv.values().collect();
        stores.sort();
        stores.dedup();
        for store in stores {
            if crate::commands::storage::database_backup_setting(config, env, store).is_none() {
                warnings.push(format!(
                    "KV store '{store}' gets no snapshots in '{env}': set [envs.{env}].backup.storage, or `backup = \"<storage>\"` in [kv.{store}], to a private S3 storage."
                ));
            }
        }
    }
    warnings
}

/// Database bindings that some environments declare and others do not. App
/// code reads databases by binding name, so one missing from an environment
/// fails only once the app runs there. Development counts only when it lists
/// its own bindings; otherwise it binds every name the others use.
fn database_binding_gaps(config: &TakoToml) -> Vec<String> {
    let envs: BTreeMap<&str, BTreeSet<&str>> = config
        .envs
        .iter()
        .filter(|(name, env)| name.as_str() != "development" || !env.databases.is_empty())
        .map(|(name, env)| {
            (
                name.as_str(),
                env.databases.keys().map(String::as_str).collect(),
            )
        })
        .collect();
    let all: BTreeSet<&str> = envs.values().flatten().copied().collect();
    let mut warnings = Vec::new();
    for (env, bound) in &envs {
        for binding in all.difference(bound) {
            let others: Vec<String> = envs
                .iter()
                .filter(|(_, other)| other.contains(binding))
                .map(|(name, _)| format!("'{name}'"))
                .collect();
            warnings.push(format!(
                "Environment '{env}' does not bind database '{binding}', which {} {}; the app has no '{binding}' database in '{env}'. Bind one with `{binding} = \"<database>\"` in [envs.{env}].databases",
                others.join(", "),
                if others.len() == 1 { "binds" } else { "bind" },
            ));
        }
    }
    warnings
}

/// Validate global server inventory configuration.
pub fn validate_server_inventory(config: &ServerInventory) -> ValidationResult {
    let mut result = ValidationResult::new();

    // Check for empty host
    for (name, entry) in &config.servers {
        if entry.host.is_empty() {
            result.error(format!("Server '{}' has empty host", name));
        }

        if entry.port == 0 {
            result.error(format!("Server '{}' has invalid SSH port 0", name));
        }
    }

    result
}

/// Full configuration validation
pub fn validate_full_config(
    tako_config: &TakoToml,
    servers_config: &ServerInventory,
    deploy_env: Option<&str>,
) -> ValidationResult {
    let mut result = ValidationResult::new();

    result.merge(validate_tako_toml(tako_config));
    result.merge(validate_server_inventory(servers_config));
    // Placement is checked against the network's trusted membership when the
    // deploy resolves its targets.
    let _ = deploy_env;

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EnvConfig, TakoToml};

    fn env_with_databases(bindings: &[(&str, &str)]) -> EnvConfig {
        EnvConfig {
            databases: bindings
                .iter()
                .map(|(binding, database)| (binding.to_string(), database.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn binding_warnings(config: &TakoToml) -> Vec<String> {
        validate_tako_toml(config)
            .warnings
            .into_iter()
            .filter(|w| w.contains("does not bind database"))
            .collect()
    }

    #[test]
    fn warns_when_an_environment_lacks_a_database_binding_others_have() {
        let mut config = TakoToml::default();
        config.envs.insert(
            "production".into(),
            env_with_databases(&[("main", "shop"), ("analytics", "events")]),
        );
        config.envs.insert(
            "staging".into(),
            env_with_databases(&[("main", "shop_staging")]),
        );
        config.envs.insert("preview".into(), EnvConfig::default());

        assert_eq!(
            binding_warnings(&config),
            [
                "Environment 'preview' does not bind database 'analytics', which 'production' binds; the app has no 'analytics' database in 'preview'. Bind one with `analytics = \"<database>\"` in [envs.preview].databases",
                "Environment 'preview' does not bind database 'main', which 'production', 'staging' bind; the app has no 'main' database in 'preview'. Bind one with `main = \"<database>\"` in [envs.preview].databases",
                "Environment 'staging' does not bind database 'analytics', which 'production' binds; the app has no 'analytics' database in 'staging'. Bind one with `analytics = \"<database>\"` in [envs.staging].databases",
            ]
        );
    }

    #[test]
    fn different_databases_under_the_same_binding_are_expected() {
        let mut config = TakoToml::default();
        config
            .envs
            .insert("production".into(), env_with_databases(&[("main", "shop")]));
        config.envs.insert(
            "staging".into(),
            env_with_databases(&[("main", "shop_staging")]),
        );
        // Development without its own bindings binds every name.
        config
            .envs
            .insert("development".into(), EnvConfig::default());
        assert!(binding_warnings(&config).is_empty());

        // With its own bindings, development must cover the same names.
        config.envs.insert(
            "development".into(),
            env_with_databases(&[("cache", "cache")]),
        );
        let warnings = binding_warnings(&config);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].starts_with("Environment 'development' does not bind database 'main'"));
    }

    #[test]
    fn databases_bound_without_a_backup_storage_warn() {
        let backup_warnings = |config: &TakoToml| -> Vec<String> {
            validate_tako_toml(config)
                .warnings
                .into_iter()
                .filter(|w| w.contains("gets no snapshots"))
                .collect()
        };
        let mut config = TakoToml::default();
        config
            .envs
            .insert("production".into(), env_with_databases(&[("main", "shop")]));
        // Development databases are local files with no snapshots.
        config.envs.insert(
            "development".into(),
            env_with_databases(&[("main", "shop")]),
        );
        assert_eq!(
            backup_warnings(&config),
            [
                "Database 'shop' gets no snapshots in 'production': set [envs.production].backup.storage, or `backup = \"<storage>\"` in [databases.shop], to a private S3 storage."
            ]
        );
        config.envs.get_mut("production").unwrap().backup = Some(crate::config::BackupConfig {
            storage: "private_backups".into(),
        });
        assert!(backup_warnings(&config).is_empty());
    }

    #[test]
    fn validate_tako_toml_rejects_environment_without_routes() {
        let mut config = TakoToml::default();
        config
            .envs
            .insert("production".to_string(), EnvConfig::default());

        let result = validate_tako_toml(&config);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("must define either 'route' or 'routes'"))
        );
    }

    #[test]
    fn validate_tako_toml_allows_development_environment_without_routes() {
        let mut config = TakoToml::default();
        config
            .envs
            .insert("development".to_string(), EnvConfig::default());

        let result = validate_tako_toml(&config);
        assert!(
            result
                .errors
                .iter()
                .all(|e| !e.contains("must define either 'route' or 'routes'"))
        );
    }

    #[test]
    fn validate_tako_toml_no_warning_for_development_without_servers() {
        let mut config = TakoToml::default();
        config
            .envs
            .insert("development".to_string(), EnvConfig::default());

        let result = validate_tako_toml(&config);
        assert!(
            result
                .warnings
                .iter()
                .all(|w| !w.contains("no servers configured"))
        );
    }

    #[test]
    fn validate_tako_toml_warns_when_servers_configured_for_development() {
        let mut config = TakoToml::default();
        config.envs.insert(
            "development".to_string(),
            EnvConfig {
                servers: vec!["dev-server".to_string()],
                ..Default::default()
            },
        );

        let result = validate_tako_toml(&config);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("'development'") && w.contains("ignored"))
        );
    }

    #[test]
    fn validate_tako_toml_warns_when_global_env_var_is_reserved() {
        let mut config = TakoToml {
            name: Some("demo".to_string()),
            ..Default::default()
        };
        config.vars.insert("ENV".to_string(), "custom".to_string());
        config.envs.insert(
            "production".to_string(),
            EnvConfig {
                route: Some("demo.example.com".to_string()),
                servers: vec!["prod".to_string()],
                ..Default::default()
            },
        );

        let result = validate_tako_toml(&config);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("[vars].ENV") && w.contains("ignored"))
        );
    }

    #[test]
    fn validate_tako_toml_warns_when_per_env_var_is_reserved() {
        let mut config = TakoToml {
            name: Some("demo".to_string()),
            ..Default::default()
        };
        config.vars_per_env.insert(
            "production".to_string(),
            std::collections::HashMap::from([("ENV".to_string(), "custom".to_string())]),
        );
        config.envs.insert(
            "production".to_string(),
            EnvConfig {
                route: Some("demo.example.com".to_string()),
                servers: vec!["prod".to_string()],
                ..Default::default()
            },
        );

        let result = validate_tako_toml(&config);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("[vars.production].ENV") && w.contains("ignored"))
        );
    }

    #[test]
    fn validate_tako_toml_does_not_warn_when_user_sets_log_level_var() {
        let mut config = TakoToml {
            name: Some("demo".to_string()),
            ..Default::default()
        };
        config
            .vars
            .insert("LOG_LEVEL".to_string(), "warn".to_string());
        config.envs.insert(
            "production".to_string(),
            EnvConfig {
                route: Some("demo.example.com".to_string()),
                servers: vec!["prod".to_string()],
                ..Default::default()
            },
        );

        let result = validate_tako_toml(&config);
        assert!(!result.warnings.iter().any(|w| w.contains("LOG_LEVEL")));
    }

    #[test]
    fn validate_tako_toml_allows_duplicate_server_membership_across_non_development_envs() {
        let mut tako_config = TakoToml::default();
        tako_config.envs.insert(
            "production".to_string(),
            EnvConfig {
                route: Some("prod.example.com".to_string()),
                servers: vec!["shared".to_string()],
                ..Default::default()
            },
        );
        tako_config.envs.insert(
            "staging".to_string(),
            EnvConfig {
                route: Some("staging.example.com".to_string()),
                servers: vec!["shared".to_string()],
                ..Default::default()
            },
        );

        let result = validate_tako_toml(&tako_config);
        assert!(result.errors.iter().all(|error| !error.contains("shared")));
    }
}

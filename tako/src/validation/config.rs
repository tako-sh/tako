use crate::config::{ConfigError, Result, ServersToml, TakoToml};

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

        if !is_development && env_config.route.is_none() && env_config.routes.is_none() {
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
        } else if servers.is_empty() {
            result.warn(format!(
                "Environment '{}' has no servers configured",
                env_name
            ));
        }
    }

    for warning in config.ignored_reserved_var_warnings() {
        result.warn(warning);
    }

    result
}

/// Validate global server inventory configuration.
pub fn validate_servers_toml(config: &ServersToml) -> ValidationResult {
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

/// Validate that tako.toml server references exist in global server inventory.
/// If `deploy_env` is set, only validates servers in that environment.
pub fn validate_server_references(
    tako_config: &TakoToml,
    servers_config: &ServersToml,
    deploy_env: Option<&str>,
) -> ValidationResult {
    let mut result = ValidationResult::new();

    for (env_name, env_config) in &tako_config.envs {
        if let Some(env) = deploy_env
            && env_name != env
        {
            continue;
        }
        for server_name in &env_config.servers {
            if !servers_config.contains(server_name) {
                result.error(format!(
                    "Server '{}' is configured in tako.toml but not found in config.toml [[servers]]. \
                      Run 'tako servers add --name {} <host>' to add it.",
                    server_name, server_name
                ));
            }
        }
    }

    result
}

/// Full configuration validation
pub fn validate_full_config(
    tako_config: &TakoToml,
    servers_config: &ServersToml,
    deploy_env: Option<&str>,
) -> ValidationResult {
    let mut result = ValidationResult::new();

    result.merge(validate_tako_toml(tako_config));
    result.merge(validate_servers_toml(servers_config));
    result.merge(validate_server_references(
        tako_config,
        servers_config,
        deploy_env,
    ));

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EnvConfig, ServerEntry, ServersToml, TakoToml};

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

    #[test]
    fn validate_server_references_skips_servers_for_other_envs() {
        let mut tako_config = TakoToml::default();
        tako_config.envs.insert(
            "production".to_string(),
            EnvConfig {
                route: Some("prod.example.com".to_string()),
                servers: vec!["prod-server".to_string()],
                ..Default::default()
            },
        );
        tako_config.envs.insert(
            "staging".to_string(),
            EnvConfig {
                route: Some("staging.example.com".to_string()),
                servers: vec!["staging-server".to_string()],
                ..Default::default()
            },
        );

        // Only prod-server is in global config
        let mut servers_config = ServersToml::default();
        servers_config.servers.insert(
            "prod-server".to_string(),
            ServerEntry {
                host: "prod.example.com".to_string(),
                port: 22,
                description: None,
                ..Default::default()
            },
        );

        // Deploying to production: staging-server missing from global config is not an error
        let result = validate_server_references(&tako_config, &servers_config, Some("production"));
        assert!(result.errors.is_empty());

        // Without env filter: staging-server missing is an error
        let result = validate_server_references(&tako_config, &servers_config, None);
        assert!(result.errors.iter().any(|e| e.contains("staging-server")));
    }
}

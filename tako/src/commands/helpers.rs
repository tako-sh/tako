use crate::config::{ServersToml, TakoToml};
use crate::output;

/// Resolve which servers to target for a given environment.
///
/// Returns explicitly mapped servers from `[envs.<name>].servers`.
pub fn resolve_servers_for_env(
    tako_config: &TakoToml,
    servers: &ServersToml,
    env: &str,
) -> Result<Vec<String>, String> {
    let mapped: Vec<String> = tako_config
        .get_servers_for_env(env)
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    if !mapped.is_empty() {
        return Ok(mapped);
    }

    if servers.is_empty() {
        return Err(format!(
            "No servers have been added. Run 'tako servers add <host>' first, \
             then add it under [envs.{}].servers in tako.toml.",
            env
        ));
    }

    Err(format!(
        "No servers configured for environment '{}'. \
         Add `servers = [\"<name>\"]` under [envs.{}] in tako.toml.",
        env, env
    ))
}

/// Resolve the target environment name, defaulting to "production".
/// When the default is used (no explicit `--env`), prints a warning.
pub fn resolve_env(requested: Option<&str>) -> String {
    let env = resolve_env_silent(requested);
    if requested.is_none() {
        output::warning(&format_environment_notice(&env));
    }
    env
}

pub fn resolve_env_silent(requested: Option<&str>) -> String {
    if let Some(env) = requested {
        env.to_string()
    } else {
        "production".to_string()
    }
}

pub(crate) fn format_environment_notice(env: &str) -> String {
    format!("Using {env} environment")
}

/// Validate that all resolved server names exist in the global servers config.
pub fn validate_server_names(names: &[String], servers: &ServersToml) -> Result<(), String> {
    for name in names {
        if !servers.contains(name) {
            return Err(format!("Server '{}' not found in config.toml", name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EnvConfig, ServerEntry};

    fn one_server_config() -> ServersToml {
        let mut servers = ServersToml::default();
        servers.servers.insert(
            "solo".to_string(),
            ServerEntry {
                host: "127.0.0.1".to_string(),
                port: 22,
                description: None,
            },
        );
        servers
    }

    #[test]
    fn explicit_mapping_returns_mapped_servers() {
        let mut tako_config = TakoToml::default();
        tako_config.envs.insert(
            "staging".to_string(),
            EnvConfig {
                route: Some("staging.example.com".to_string()),
                servers: vec!["web1".to_string()],
                ..Default::default()
            },
        );
        let servers = one_server_config();

        let resolved = resolve_servers_for_env(&tako_config, &servers, "staging").unwrap();
        assert_eq!(resolved, vec!["web1"]);
    }

    #[test]
    fn non_production_without_mapping_errors() {
        let tako_config = TakoToml::default();
        let servers = one_server_config();

        let err =
            resolve_servers_for_env(&tako_config, &servers, "staging").expect_err("should fail");
        assert!(err.contains("No servers configured for environment 'staging'"));
    }

    #[test]
    fn no_servers_at_all_errors() {
        let tako_config = TakoToml::default();
        let servers = ServersToml::default();

        let err =
            resolve_servers_for_env(&tako_config, &servers, "production").expect_err("should fail");
        assert!(err.contains("No servers have been added"));
    }

    #[test]
    fn validate_server_names_passes_for_known_servers() {
        let servers = one_server_config();
        assert!(validate_server_names(&["solo".to_string()], &servers).is_ok());
    }

    #[test]
    fn validate_server_names_fails_for_unknown_server() {
        let servers = one_server_config();
        let err =
            validate_server_names(&["missing".to_string()], &servers).expect_err("should fail");
        assert!(err.contains("missing"));
    }

    #[test]
    fn environment_notice_has_plain_text_environment_name() {
        assert_eq!(
            format_environment_notice("production"),
            "Using production environment"
        );
    }

    #[test]
    fn resolve_env_silent_defaults_to_production_without_output_side_effect() {
        assert_eq!(resolve_env_silent(None), "production");
        assert_eq!(resolve_env_silent(Some("staging")), "staging");
    }
}

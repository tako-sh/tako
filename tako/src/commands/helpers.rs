use crate::config::{ServersToml, TakoToml};
use crate::output;

/// Resolve which servers to target for a given environment.
///
/// Uses trusted native network membership or explicit legacy server mappings.
pub fn resolve_servers_for_env(
    tako_config: &TakoToml,
    servers: &ServersToml,
    env: &str,
) -> Result<Vec<String>, String> {
    if let Some(network) = tako_config
        .envs
        .get(env)
        .and_then(|env| env.network.as_deref())
    {
        let mut names: Vec<_> = servers
            .servers
            .iter()
            .filter(|(_, entry)| {
                entry
                    .native
                    .as_ref()
                    .is_some_and(|target| target.network.id == network)
            })
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        if names.is_empty() {
            return Err(format!(
                "Network '{network}' has no trusted servers for environment '{env}'"
            ));
        }
        return Ok(names);
    }
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
                ..Default::default()
            },
        );
        servers
    }

    fn native_entry(network_id: &str) -> ServerEntry {
        ServerEntry {
            native: Some(crate::management_http::NativeTarget {
                network: crate::config::NetworkAlias {
                    name: "fleet".into(),
                    id: network_id.into(),
                    device: "1".repeat(64),
                },
                server_id: "2".repeat(64).parse().unwrap(),
            }),
            host: "192.0.2.10".into(),
            ..Default::default()
        }
    }

    #[test]
    fn network_selection_uses_stable_id_and_never_falls_back_to_legacy() {
        let id = "a".repeat(32);
        let mut project = TakoToml::default();
        project.envs.insert(
            "production".into(),
            EnvConfig {
                network: Some(id.clone()),
                ..Default::default()
            },
        );
        let mut servers = one_server_config();
        servers
            .servers
            .insert("fleet/node".into(), native_entry(&id));
        servers
            .servers
            .insert("other/node".into(), native_entry(&"b".repeat(32)));
        assert_eq!(
            resolve_servers_for_env(&project, &servers, "production").unwrap(),
            vec!["fleet/node"]
        );
        servers.servers.remove("fleet/node");
        assert!(
            resolve_servers_for_env(&project, &servers, "production")
                .unwrap_err()
                .contains("no trusted servers")
        );
    }

    #[test]
    fn saving_inventory_never_persists_native_members_as_legacy_servers() {
        let mut servers = one_server_config();
        servers
            .servers
            .insert("fleet/node".into(), native_entry(&"a".repeat(32)));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "theme = 'dark'\n[[networks]]\nname = 'fleet'\nid = '{}'\ndevice = '{}'\n",
                "a".repeat(32),
                "1".repeat(64)
            ),
        )
        .unwrap();
        servers.save_to_file(&path).unwrap();
        let saved = ServersToml::load_from_file(&path).unwrap();
        assert_eq!(saved, one_server_config());
        assert_eq!(
            crate::config::NetworksToml::load_from_file(&path)
                .unwrap()
                .networks
                .len(),
            1
        );
        assert!(std::fs::read_to_string(path).unwrap().contains("dark"));
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

pub(crate) async fn load_management_servers(
    project: Option<&TakoToml>,
    env: Option<&str>,
) -> Result<ServersToml, String> {
    let mut servers = ServersToml::load().map_err(|error| error.to_string())?;
    let aliases = crate::config::NetworksToml::load().map_err(|error| error.to_string())?;
    let selected = if let Some(project) = project {
        let ids: std::collections::BTreeSet<_> = project
            .envs
            .iter()
            .filter(|(name, _)| env.is_none_or(|env| env == name.as_str()))
            .filter_map(|(_, config)| config.network.as_deref())
            .collect();
        ids.into_iter()
            .map(|id| aliases.select(Some(id)).cloned())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
    } else {
        aliases.networks.into_values().collect()
    };
    if env.is_some_and(|env| {
        project
            .and_then(|project| project.envs.get(env))
            .is_some_and(|env| env.network.is_some())
    }) {
        servers = ServersToml::default();
    }
    let refresh = !output::is_dry_run();
    for alias in selected {
        let entries = tokio::task::spawn_blocking(move || {
            let profile = if refresh {
                crate::network::client::refresh(&alias)
            } else {
                crate::network::load_profile(&alias)
            }
            .map_err(|error| error.to_string())?;
            native_server_entries(&alias, &profile)
        })
        .await
        .map_err(|error| error.to_string())??;
        for (name, entry) in entries {
            servers.servers.insert(name, entry);
        }
    }
    Ok(servers)
}

fn native_server_entries(
    alias: &crate::config::NetworkAlias,
    profile: &crate::network::Profile,
) -> Result<Vec<(String, crate::config::ServerEntry)>, String> {
    let members = crate::network::server_members(profile).map_err(|error| error.to_string())?;
    let mut names = std::collections::BTreeMap::new();
    for member in members.values() {
        *names
            .entry(member.request.name.to_string())
            .or_insert(0usize) += 1;
    }
    members
        .into_iter()
        .map(|(server_id, member)| {
            let tako_network::MemberKind::Server(info) = member.kind else {
                unreachable!()
            };
            let name = if names[&member.request.name.to_string()] == 1 {
                format!("{}/{}", alias.name, member.request.name)
            } else {
                format!("{}/{}/{}", alias.name, member.request.name, server_id)
            };
            Ok((
                name,
                crate::config::ServerEntry {
                    native: Some(crate::management_http::NativeTarget {
                        network: alias.clone(),
                        server_id,
                    }),
                    host: info.wireguard_endpoint.ip().to_string(),
                    port: info.ssh_port,
                    https_port: info.https_port,
                    ..Default::default()
                },
            ))
        })
        .collect()
}

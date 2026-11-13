use crate::config::{ServerInventory, TakoToml};
use crate::output;

/// Resolve which servers to target for a given environment.
///
/// Uses trusted native network membership and project placement selectors.
pub fn resolve_servers_for_env(
    tako_config: &TakoToml,
    servers: &ServerInventory,
    env: &str,
) -> Result<Vec<String>, String> {
    if let Some(config) = tako_config.envs.get(env)
        && let Some(network) = &config.network
    {
        return resolve_network_servers(servers, network, &config.servers, env);
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
            "No servers have been added. Run 'tako network server add <host>' first, \
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

fn resolve_network_servers(
    servers: &ServerInventory,
    network: &str,
    selectors: &[String],
    env: &str,
) -> Result<Vec<String>, String> {
    let members: std::collections::BTreeMap<_, _> = servers
        .servers
        .iter()
        .filter_map(|(name, entry)| {
            let target = entry.native.as_ref()?;
            (target.network.id == network).then_some((name, target))
        })
        .collect();
    if members.is_empty() {
        return Err(format!(
            "Network '{network}' has no trusted servers for environment '{env}'"
        ));
    }
    if selectors.is_empty() {
        return Ok(members.into_keys().cloned().collect());
    }

    let mut selected = std::collections::BTreeSet::new();
    for selector in selectors {
        let device_id = selector.parse::<tako_network::DeviceId>().ok();
        let mut matches = members.iter().filter(|(_, target)| {
            target.server_name.as_str() == selector || Some(&target.server_id) == device_id.as_ref()
        });
        let Some((name, _)) = matches.next() else {
            return Err(format!(
                "Server '{selector}' is not a trusted server in network '{network}' for environment '{env}'"
            ));
        };
        if matches.next().is_some() {
            return Err(format!(
                "Server name '{selector}' is ambiguous in network '{network}'. Use a server device ID in [envs.{env}].servers."
            ));
        }
        selected.insert((*name).clone());
    }
    Ok(selected.into_iter().collect())
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

/// Validate that all resolved server names exist in enrolled network membership.
pub fn validate_server_names(names: &[String], servers: &ServerInventory) -> Result<(), String> {
    for name in names {
        if !servers.contains(name) {
            return Err(format!(
                "Server '{}' not found in enrolled network membership",
                name
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EnvConfig, ServerEntry};

    #[test]
    fn native_membership_restores_local_bootstrap_access_for_maintenance() {
        let _home = crate::network::tests::TestHome::new();
        let fixture = crate::network::tests::fixture();
        crate::network::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
        let alias = crate::network::save_profile(&fixture.profile).unwrap();
        let entries = native_server_entries(&alias, &fixture.profile).unwrap();
        assert!(entries[0].1.key_path.is_none());
        let tako_network::MemberKind::Server(info) =
            &crate::network::server_members(&fixture.profile).unwrap()[&fixture.server].kind
        else {
            unreachable!()
        };
        assert_eq!(entries[0].1.host, info.wireguard_endpoint.ip().to_string());

        let relative = std::path::Path::new("keys/custom-bootstrap-key");
        crate::network::server_access::save(
            &alias,
            &fixture.server,
            "ssh.private.example",
            Some(relative),
        )
        .unwrap();
        // Refreshing public membership must not erase workstation-local SSH choices.
        crate::network::save_profile(&fixture.profile).unwrap();
        let refreshed = crate::network::load_profile(&alias).unwrap();
        let entries = native_server_entries(&alias, &refreshed).unwrap();
        let config = crate::ssh::SshConfig::from_entry(&entries[0].1);
        let expected = std::env::current_dir().unwrap().join(relative);
        assert_eq!(config.key_path.as_ref(), Some(&expected));
        assert_eq!(config.host, "ssh.private.example");
        assert_eq!(config.user, "tako");
        assert!(
            !serde_json::to_string(&refreshed)
                .unwrap()
                .contains("ssh.private.example")
        );
        assert!(
            !serde_json::to_string(&refreshed)
                .unwrap()
                .contains("custom-bootstrap-key")
        );

        // Re-enrollment without an explicit key returns to default keys / agent auth.
        crate::network::server_access::save(&alias, &fixture.server, "new-ssh.private", None)
            .unwrap();
        assert_eq!(
            native_server_entries(&alias, &refreshed).unwrap()[0].1.host,
            "new-ssh.private"
        );
        assert!(
            native_server_entries(&alias, &refreshed).unwrap()[0]
                .1
                .key_path
                .is_none()
        );
    }

    #[test]
    fn native_maintenance_access_is_scoped_to_server_and_local_device() {
        let _home = crate::network::tests::TestHome::new();
        let fixture = crate::network::tests::fixture();
        crate::network::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
        let alias = crate::network::save_profile(&fixture.profile).unwrap();
        let key = std::path::Path::new("/keys/custom-bootstrap-key");
        crate::network::server_access::save(
            &alias,
            &fixture.server,
            "ssh.private.example",
            Some(key),
        )
        .unwrap();

        let mut renamed = alias.clone();
        renamed.name = "renamed".into();
        assert_eq!(
            crate::network::server_access::load(&renamed, &fixture.server)
                .unwrap()
                .host
                .as_deref(),
            Some("ssh.private.example")
        );
        assert_eq!(
            crate::network::server_access::load(&renamed, &fixture.server)
                .unwrap()
                .key_path
                .as_deref(),
            Some(key)
        );
        let another_server = fixture
            .profile
            .request()
            .device_id_for(&fixture.profile.snapshot().network.id)
            .unwrap();
        let other = crate::network::server_access::load(&alias, &another_server).unwrap();
        assert!(other.host.is_none());
        assert!(other.key_path.is_none());
        let mut another_device = alias;
        another_device.device = fixture.server.to_string();
        let other = crate::network::server_access::load(&another_device, &fixture.server).unwrap();
        assert!(other.host.is_none());
        assert!(other.key_path.is_none());
    }

    #[test]
    fn native_maintenance_retains_key_only_metadata_without_a_host_override() {
        let _home = crate::network::tests::TestHome::new();
        let fixture = crate::network::tests::fixture();
        crate::network::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
        let alias = crate::network::save_profile(&fixture.profile).unwrap();
        let entries = native_server_entries(&alias, &fixture.profile).unwrap();
        let public_host = entries[0].1.host.clone();
        crate::network::write_json(
            &alias
                .profile_path()
                .unwrap()
                .with_file_name(format!("ssh-{}.json", fixture.server)),
            &serde_json::json!({"key_path": "/keys/existing-key"}),
            false,
        )
        .unwrap();
        let entries = native_server_entries(&alias, &fixture.profile).unwrap();
        let config = crate::ssh::SshConfig::from_entry(&entries[0].1);
        assert_eq!(config.host, public_host);
        assert_eq!(
            config.key_path.as_deref(),
            Some(std::path::Path::new("/keys/existing-key"))
        );
    }

    fn one_server_config() -> ServerInventory {
        let mut servers = ServerInventory::default();
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

    fn native_entry(network_id: &str, name: &str, id: &str) -> ServerEntry {
        ServerEntry {
            native: Some(crate::management_http::NativeTarget {
                network: crate::config::LocalNetwork {
                    name: "fleet".into(),
                    id: network_id.into(),
                    device: "1".repeat(64),
                },
                server_id: id.parse().unwrap(),
                server_name: name.parse().unwrap(),
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
        servers.servers.insert(
            "fleet/node".into(),
            native_entry(&id, "node", &"2".repeat(64)),
        );
        servers.servers.insert(
            "other/node".into(),
            native_entry(&"b".repeat(32), "node", &"3".repeat(64)),
        );
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
    fn native_placement_resolves_each_apps_servers_within_the_same_network() {
        let id = "a".repeat(32);
        let mut servers = one_server_config();
        for (name, key) in [("web-a", "1"), ("web-b", "2"), ("billing", "3")] {
            servers.servers.insert(
                format!("local-alias/{name}"),
                native_entry(&id, name, &key.repeat(64)),
            );
        }
        servers.servers.insert(
            "foreign/other".into(),
            native_entry(&"b".repeat(32), "other", &"4".repeat(64)),
        );

        let resolve = |list: &str| {
            let config = TakoToml::parse(&format!(
                "[envs.production]\nroute = 'example.com'\nnetwork = '{id}'\n{list}"
            ))
            .unwrap();
            resolve_servers_for_env(&config, &servers, "production")
        };
        assert_eq!(resolve("").unwrap().len(), 3);
        assert_eq!(
            resolve("servers = ['web-b', 'web-a']").unwrap(),
            ["local-alias/web-a", "local-alias/web-b"]
        );
        assert_eq!(
            resolve("servers = ['billing']").unwrap(),
            ["local-alias/billing"]
        );
        for invalid in ["other", "solo", "missing"] {
            assert!(
                resolve(&format!("servers = ['web-a', '{invalid}']"))
                    .unwrap_err()
                    .contains(invalid)
            );
        }
    }

    #[test]
    fn ambiguous_native_names_require_a_device_id() {
        let network = "a".repeat(32);
        let mut servers = ServerInventory::default();
        for key in ["1".repeat(64), "2".repeat(64)] {
            servers.servers.insert(
                format!("fleet/edge/{key}"),
                native_entry(&network, "edge", &key),
            );
        }
        assert!(
            resolve_network_servers(&servers, &network, &["edge".into()], "production")
                .unwrap_err()
                .contains("ambiguous")
        );
        let key = "2".repeat(64);
        assert_eq!(
            resolve_network_servers(&servers, &network, std::slice::from_ref(&key), "production")
                .unwrap(),
            [format!("fleet/edge/{key}")]
        );
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
        let servers = ServerInventory::default();

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
) -> Result<ServerInventory, String> {
    let mut servers = ServerInventory::default();
    let aliases = crate::config::LocalNetworks::load().map_err(|error| error.to_string())?;
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
        servers = ServerInventory::default();
    }
    let refresh = !output::is_dry_run();
    // Without a project every configured network is only listed, so one
    // unreachable network must not fail the whole command.
    let targeted = project.is_some();
    for alias in selected {
        let network = alias.name.clone();
        let entries = tokio::task::spawn_blocking(move || {
            let profile = match if refresh {
                crate::network::client::refresh(&alias)
            } else {
                crate::network::load_profile(&alias)
            } {
                Ok(profile) => profile,
                // Listing only: the saved membership still names the servers, and
                // the status call itself reports one that stays unreachable. A
                // targeted environment keeps requiring refreshed membership.
                Err(error) if refresh && !targeted => match crate::network::load_profile(&alias) {
                    Ok(profile) => {
                        warn_network(&format!(
                            "Network {}: using saved membership ({error})",
                            alias.name
                        ));
                        profile
                    }
                    Err(_) => return Err(error.to_string()),
                },
                Err(error) => return Err(error.to_string()),
            };
            native_server_entries(&alias, &profile)
        })
        .await
        .map_err(|error| error.to_string())?;
        match entries {
            Ok(entries) => {
                for (name, entry) in entries {
                    servers.servers.insert(name, entry);
                }
            }
            Err(error) if !targeted => {
                warn_network(&format!("Skipping network {network}: {error}"));
            }
            Err(error) => return Err(format!("Network {network}: {error}")),
        }
    }
    Ok(servers)
}

fn warn_network(message: &str) {
    if output::is_pretty() {
        output::warning(message);
    } else {
        tracing::warn!("{message}");
    }
}

pub(crate) fn native_server_entries(
    alias: &crate::config::LocalNetwork,
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
            let access = crate::network::server_access::load(alias, &server_id)
                .map_err(|error| error.to_string())?;
            Ok((
                name,
                crate::config::ServerEntry {
                    native: Some(crate::management_http::NativeTarget {
                        network: alias.clone(),
                        server_id,
                        server_name: member.request.name.clone(),
                    }),
                    host: access
                        .host
                        .unwrap_or_else(|| info.wireguard_endpoint.ip().to_string()),
                    port: info.ssh_port,
                    https_port: info.https_port,
                    key_path: access.key_path,
                    ..Default::default()
                },
            ))
        })
        .collect()
}

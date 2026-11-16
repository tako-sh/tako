//! Resolving databases and their home servers for CLI commands and deploys.

use std::collections::HashMap;

use crate::config::{DatabaseResourceConfig, ServerEntry, ServerInventory, TakoToml};

/// A database's home server, resolved against network membership.
#[derive(Clone)]
pub(crate) struct Home {
    /// Inventory key, used to reach the server.
    pub key: String,
    pub entry: ServerEntry,
    /// The server's device ID.
    pub device_id: String,
    /// The server's enrolled name.
    pub name: String,
}

/// The network ID of an environment.
pub(crate) fn env_network<'a>(config: &'a TakoToml, env: &str) -> Result<&'a str, String> {
    config
        .envs
        .get(env)
        .and_then(|env| env.network.as_deref())
        .ok_or_else(|| {
            format!(
                "Environment '{env}' has no network. Databases are hosted on native network servers; set [envs.{env}].network in tako.toml."
            )
        })
}

/// Resolves a server name or device ID to a trusted server of the network.
pub(crate) fn resolve_home(
    servers: &ServerInventory,
    network: &str,
    selector: &str,
    env: &str,
) -> Result<Home, String> {
    let keys = crate::commands::helpers::resolve_network_servers(
        servers,
        network,
        &[selector.to_owned()],
        env,
    )?;
    let key = keys
        .into_iter()
        .next()
        .ok_or_else(|| format!("Server '{selector}' is not a trusted server of this network"))?;
    let entry = servers
        .get(&key)
        .ok_or_else(|| format!("Server '{selector}' is not enrolled"))?
        .clone();
    let native = entry
        .native
        .as_ref()
        .ok_or_else(|| format!("Server '{selector}' is not a native network server"))?;
    Ok(Home {
        device_id: native.server_id.to_string(),
        name: native.server_name.to_string(),
        key,
        entry,
    })
}

/// One database binding of an environment, ready to deploy.
#[derive(Clone)]
pub(crate) struct PlannedBinding {
    pub database_name: String,
    pub database: DatabaseResourceConfig,
    pub home: Home,
    pub binding: tako_core::DatabaseBinding,
}

/// Resolves an environment's database bindings for a deploy to
/// `target_servers` (inventory keys). A database with local access must be
/// hosted on every target server, so only single-server placements on its
/// home server qualify.
pub(crate) fn plan_bindings(
    config: &TakoToml,
    servers: &ServerInventory,
    env: &str,
    target_servers: &[String],
) -> Result<HashMap<String, PlannedBinding>, String> {
    let Some(env_config) = config.envs.get(env) else {
        return Ok(HashMap::new());
    };
    if env_config.databases.is_empty() {
        return Ok(HashMap::new());
    }
    let network = env_network(config, env)?;
    let mut planned = HashMap::new();
    for (binding, database_name) in &env_config.databases {
        let database = config.databases.get(database_name).ok_or_else(|| {
            format!("Database binding '{binding}' references missing [databases.{database_name}]")
        })?;
        let home = resolve_home(servers, network, &database.server, env)
            .map_err(|error| format!("[databases.{database_name}].server: {error}"))?;
        if database.access == tako_core::DatabaseAccess::Local
            && let Some(other) = target_servers.iter().find(|target| **target != home.key)
        {
            return Err(format!(
                "Database '{database_name}' is hosted on {}, but {env} also deploys to {other}. Set access = \"network\" in [databases.{database_name}] to let apps on other servers use it, or deploy {env} only to {}.",
                home.name, home.name
            ));
        }
        planned.insert(
            binding.clone(),
            PlannedBinding {
                database_name: database_name.clone(),
                database: database.clone(),
                binding: tako_core::DatabaseBinding {
                    id: database.id.clone(),
                    server: home.device_id.clone(),
                    access: database.access,
                    // Filled from the home server's grant during the deploy.
                    peer_key: None,
                },
                home,
            },
        );
    }
    Ok(planned)
}

/// The bindings to send with `RunRelease` and `Deploy`.
pub(crate) fn wire_bindings(
    planned: &HashMap<String, PlannedBinding>,
) -> HashMap<String, tako_core::DatabaseBinding> {
    planned
        .iter()
        .map(|(name, planned)| (name.clone(), planned.binding.clone()))
        .collect()
}

/// A database declared in this project.
pub(crate) fn declared<'a>(
    config: &'a TakoToml,
    name: &str,
) -> Result<&'a DatabaseResourceConfig, String> {
    config.databases.get(name).ok_or_else(|| {
        format!(
            "Database '{name}' is not declared in tako.toml. Run `tako sqlite list` to see the network's databases, or `tako sqlite link {name}` to add one."
        )
    })
}

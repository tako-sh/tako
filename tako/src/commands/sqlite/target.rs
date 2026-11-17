//! Resolving databases and their home servers for CLI commands and deploys.

use std::collections::HashMap;

use crate::config::{ServerEntry, ServerInventory, TakoToml};
use crate::network::databases::DatabaseRecord;

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

/// The network's database records, for an environment that binds databases.
/// Empty when it binds none and has no network.
pub(crate) fn env_records(
    config: &TakoToml,
    env: &str,
) -> Result<Vec<DatabaseRecord>, Box<dyn std::error::Error>> {
    let binds = config
        .envs
        .get(env)
        .is_some_and(|env| !env.databases.is_empty());
    let Some(network) = config.envs.get(env).and_then(|env| env.network.as_deref()) else {
        if binds {
            env_network(config, env)?;
        }
        return Ok(Vec::new());
    };
    let alias = crate::config::LocalNetworks::load()?
        .select(Some(network))?
        .clone();
    tokio::task::block_in_place(|| crate::network::databases::list(&alias))
}

/// One database binding of an environment, ready to deploy.
#[derive(Clone)]
pub(crate) struct PlannedBinding {
    pub database_name: String,
    pub home: Home,
    pub binding: tako_core::DatabaseBinding,
}

/// Resolves an environment's database bindings for a deploy, using the
/// network's `records`. Apps on other servers than a database's reach it
/// over the private network.
pub(crate) fn plan_bindings(
    config: &TakoToml,
    servers: &ServerInventory,
    env: &str,
    records: &[DatabaseRecord],
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
        let record = records
            .iter()
            .find(|record| &record.name == database_name)
            .ok_or_else(|| {
                format!(
                    "Database '{database_name}', bound as '{binding}' in {env}, does not exist in this network. Create it with `tako sqlite create {database_name}`, or run `tako sqlite list` to see the network's databases."
                )
            })?;
        let home = resolve_home(servers, network, &record.server, env)
            .map_err(|error| format!("The server of database '{database_name}': {error}"))?;
        planned.insert(
            binding.clone(),
            PlannedBinding {
                database_name: database_name.clone(),
                binding: tako_core::DatabaseBinding {
                    name: database_name.clone(),
                    server: home.device_id.clone(),
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

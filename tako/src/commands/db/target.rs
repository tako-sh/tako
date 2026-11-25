//! Resolving databases and their home servers for CLI commands and deploys.

use std::collections::HashMap;

use crate::config::{ServerEntry, ServerInventory};

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
    inventory_home(servers, &key)
}

pub(crate) fn inventory_home(servers: &ServerInventory, key: &str) -> Result<Home, String> {
    let entry = servers
        .get(key)
        .ok_or_else(|| format!("Server '{key}' is not enrolled"))?
        .clone();
    let native = entry
        .native
        .as_ref()
        .ok_or_else(|| format!("Server '{key}' is not a native network server"))?;
    Ok(Home {
        device_id: native.server_id.to_string(),
        name: native.server_name.to_string(),
        key: key.to_owned(),
        entry,
    })
}

/// One database binding of an environment, ready to deploy.
#[derive(Clone)]
pub(crate) struct PlannedBinding {
    pub database_name: String,
    pub home: Home,
    pub engine: tako_core::DatabaseEngine,
    pub generation: Option<String>,
    pub peer_key: Option<String>,
}

impl PlannedBinding {
    pub(crate) fn new(name: &str, home: Home, engine: tako_core::DatabaseEngine) -> Self {
        Self {
            database_name: name.to_owned(),
            home,
            engine,
            generation: None,
            peer_key: None,
        }
    }

    pub(crate) fn wire(&self) -> Result<tako_core::DatabaseBinding, String> {
        Ok(tako_core::DatabaseBinding {
            name: self.database_name.clone(),
            generation: self
                .generation
                .clone()
                .ok_or_else(|| format!("No catalog generation for {}", self.database_name))?,
            engine: self.engine,
            peer_key: self.peer_key.clone(),
        })
    }
}

/// The bindings to send with `RunRelease` and `Deploy`.
pub(crate) fn wire_bindings(
    planned: &HashMap<String, PlannedBinding>,
) -> Result<HashMap<String, tako_core::DatabaseBinding>, String> {
    planned
        .iter()
        .map(|(name, planned)| Ok((name.clone(), planned.wire()?)))
        .collect()
}

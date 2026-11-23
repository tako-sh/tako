//! What a project's app environments hold in the network state and this
//! worktree's local state, for `tako status` and `tako doctor`. Never decrypts
//! or shows a value: secrets appear as counts, credentials by name.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::config::{
    LocalNetworks, SSL_CLOUDFLARE_CREDENTIAL_NAME, SecretsStore, ServerInventory, TakoToml,
};
use crate::network::databases::DatabaseRecord;

/// Why a deploy client sees no secrets.
pub(crate) const DEPLOY_CLIENT_NOTE: &str =
    "Secrets, credentials and backup keys need a full client.";

/// This device's access to an environment's network state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Access {
    /// The environment has no network: its values live in local state only.
    Local,
    Full,
    Deploy,
    /// This device could not tell, usually because it is not a member.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct StorageBinding {
    pub binding: String,
    pub resource: String,
    /// Whether an S3 resource has credentials; `None` for local storage or
    /// when this device can't see them.
    pub credentials: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "usage", rename_all = "snake_case")]
pub(crate) enum DatabaseUsage {
    Binding { binding: String },
    ChannelReplay,
}

impl DatabaseUsage {
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Binding { binding } => binding,
            Self::ChannelReplay => "channel replay",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ProjectDatabase {
    #[serde(flatten)]
    pub usage: DatabaseUsage,
    pub database: String,
    /// The server the database lives on, when its network record is known.
    pub server: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct EnvResources {
    pub name: String,
    /// The network's local name, or its ID when this device has no alias.
    pub network: Option<String>,
    #[serde(skip)]
    pub network_id: Option<String>,
    pub access: Access,
    pub servers: Vec<String>,
    /// Number of app secrets; `None` when this device can't read them.
    pub secrets: Option<usize>,
    pub credentials: Option<Vec<String>>,
    pub storages: Vec<StorageBinding>,
    pub backup_key: Option<bool>,
    pub databases: Vec<ProjectDatabase>,
    pub note: Option<String>,
}

impl EnvResources {
    pub(crate) fn can_read_secrets(&self) -> bool {
        matches!(self.access, Access::Local | Access::Full)
    }
}

/// A project's app and the resources of each environment it configures.
pub(crate) struct ProjectResources {
    pub app: String,
    pub config: TakoToml,
    pub secrets: SecretsStore,
    pub envs: Vec<EnvResources>,
    /// The databases of each network the project uses, by network ID.
    pub databases: BTreeMap<String, Result<Vec<DatabaseRecord>, String>>,
}

impl ProjectResources {
    /// Databases homed on `server_id`, with the app environments of this
    /// project that bind each one.
    pub(crate) fn databases_on(&self, server_id: &str) -> Vec<(String, Vec<String>)> {
        let mut homed = Vec::new();
        for (network_id, records) in &self.databases {
            let Ok(records) = records else {
                continue;
            };
            for record in records.iter().filter(|record| record.server == server_id) {
                // Database names are unique within a network, not across them.
                let mut users = BTreeSet::new();
                for env in self
                    .envs
                    .iter()
                    .filter(|env| env.network_id.as_ref() == Some(network_id))
                {
                    for db in env.databases.iter().filter(|db| db.database == record.name) {
                        let scope = tako_core::deployment_app_id(&self.app, &env.name);
                        users.insert(match db.usage {
                            DatabaseUsage::Binding { .. } => scope,
                            DatabaseUsage::ChannelReplay => {
                                tako_core::channel_replay_grant_scope(&scope)
                            }
                        });
                    }
                }
                homed.push((record.name.clone(), users.into_iter().collect()));
            }
        }
        homed.sort();
        homed
    }
}

/// Load the project's resources. Network failures become notes, not errors.
pub(crate) fn load(
    context: &crate::commands::project_context::ProjectContext,
    inventory: &ServerInventory,
) -> Result<ProjectResources, Box<dyn std::error::Error>> {
    let config = TakoToml::load_from_file(&context.config_path)?;
    let app = config.name.clone().unwrap_or_else(|| "app".to_string());
    let secrets = context.load_secrets().unwrap_or_default();
    let aliases = LocalNetworks::load().ok();
    let needs_replay = super::deploy::project_needs_channel_replay(&context.project_dir, &config);

    let mut databases = BTreeMap::new();
    let mut envs = Vec::new();
    for name in environment_names(&config, &secrets) {
        let network_id = super::env_network::current_id(&config, &name);
        let alias = network_id.as_deref().and_then(|id| {
            aliases
                .as_ref()
                .and_then(|aliases| aliases.networks.get(id))
                .cloned()
        });
        let access = match network_id.as_deref() {
            None => Access::Local,
            Some(id) => match super::helpers::is_full_client(id) {
                Ok(true) => Access::Full,
                Ok(false) => Access::Deploy,
                Err(_) => Access::Unknown,
            },
        };
        let servers = match network_id.as_deref() {
            Some(id) => super::helpers::resolve_network_servers(
                inventory,
                id,
                &config
                    .envs
                    .get(&name)
                    .map(|env| env.servers.clone())
                    .unwrap_or_default(),
                &name,
            ),
            None => Err(format!("Environment '{name}' has no network yet")),
        };
        if let (Some(id), Some(alias)) = (network_id.as_ref(), alias.as_ref())
            && !databases.contains_key(id)
        {
            let records = crate::network::databases::list(alias).map_err(|error| error.to_string());
            databases.insert(id.clone(), records);
        }
        let homes = network_id
            .as_ref()
            .and_then(|id| databases.get(id))
            .and_then(|records| records.as_ref().ok());
        envs.push(env_resources(
            &name,
            &config,
            &secrets,
            needs_replay,
            access,
            servers,
            alias.map(|alias| alias.alias).or(network_id.clone()),
            network_id,
            homes.map(Vec::as_slice).unwrap_or_default(),
            inventory,
        ));
    }
    Ok(ProjectResources {
        app,
        config,
        secrets,
        envs,
        databases,
    })
}

/// Every environment named in `tako.toml` or holding local values.
fn environment_names(config: &TakoToml, secrets: &SecretsStore) -> Vec<String> {
    let mut names: BTreeSet<String> = config.envs.keys().cloned().collect();
    names.extend(secrets.environment_names());
    names.into_iter().collect()
}

/// Each database's server name, by database name.
fn server_names(
    records: &[DatabaseRecord],
    inventory: &ServerInventory,
) -> BTreeMap<String, String> {
    records
        .iter()
        .map(|record| {
            let server = inventory
                .servers
                .iter()
                .find(|(_, entry)| {
                    entry
                        .native
                        .as_ref()
                        .is_some_and(|target| target.server_id.to_string() == record.server)
                })
                .map(|(name, _)| name.clone())
                .unwrap_or_else(|| record.server.chars().take(12).collect());
            (record.name.clone(), server)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn env_resources(
    name: &str,
    config: &TakoToml,
    secrets: &SecretsStore,
    needs_replay: bool,
    access: Access,
    servers: Result<Vec<String>, String>,
    network: Option<String>,
    network_id: Option<String>,
    records: &[DatabaseRecord],
    inventory: &ServerInventory,
) -> EnvResources {
    let env = config.envs.get(name);
    let readable = matches!(access, Access::Local | Access::Full);
    let mut notes = Vec::new();
    let servers = match servers {
        Ok(servers) => servers,
        Err(error) => {
            if network_id.is_some() {
                notes.push(error);
            }
            Vec::new()
        }
    };
    match access {
        Access::Deploy => notes.push(DEPLOY_CLIENT_NOTE.to_string()),
        Access::Unknown => {
            notes.push("This device is not a member of the environment's network.".to_string())
        }
        Access::Local | Access::Full => {}
    }

    let mut storages: Vec<StorageBinding> = env
        .map(|env| {
            env.storages
                .iter()
                .map(|(binding, resource)| StorageBinding {
                    binding: binding.clone(),
                    resource: resource.clone(),
                    credentials: (readable && config.storages.contains_key(resource))
                        .then(|| secrets.get_storage_credentials(name, resource).is_some()),
                })
                .collect()
        })
        .unwrap_or_default();
    storages.sort_by(|a, b| a.binding.cmp(&b.binding));

    let database_servers = server_names(records, inventory);
    let mut databases: Vec<ProjectDatabase> = env
        .map(|env| {
            env.databases
                .iter()
                .map(|(binding, database)| ProjectDatabase {
                    usage: DatabaseUsage::Binding {
                        binding: binding.clone(),
                    },
                    database: database.clone(),
                    server: database_servers.get(database).cloned(),
                })
                .collect()
        })
        .unwrap_or_default();
    databases.sort_by(|a, b| a.usage.label().cmp(b.usage.label()));
    if let Some(database) = replay_database(config, name, records, secrets, needs_replay) {
        databases.push(ProjectDatabase {
            usage: DatabaseUsage::ChannelReplay,
            server: database_servers.get(database).cloned(),
            database: database.to_owned(),
        });
    }

    let credentials = readable.then(|| {
        let mut names: Vec<String> = secrets
            .get_env_credentials(name)
            .map(|credentials| credentials.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    });

    EnvResources {
        name: name.to_string(),
        network,
        network_id,
        access,
        servers,
        secrets: readable.then(|| secrets.get_env(name).map_or(0, |values| values.len())),
        credentials,
        storages,
        backup_key: readable.then(|| {
            secrets
                .environments
                .get(name)
                .is_some_and(|env| !env.backup_keys.is_empty())
        }),
        databases,
        note: (!notes.is_empty()).then(|| notes.join(" ")),
    }
}

/// Hosted replay uses a database without creating an app SQL binding. Like
/// deploy planning, the generated database is selected only while the
/// project's code uses channels (`needs_replay`) and no `postgres_url` is set;
/// otherwise it stays in the directory unused.
pub(crate) fn replay_database<'a>(
    config: &'a TakoToml,
    env: &str,
    records: &'a [DatabaseRecord],
    secrets: &SecretsStore,
    needs_replay: bool,
) -> Option<&'a str> {
    if env == "development" {
        return None;
    }
    if let Some(channels) = &config.envs.get(env)?.channels {
        return Some(&channels.database);
    }
    if !needs_replay
        || secrets
            .get_credential(env, crate::config::POSTGRES_CREDENTIAL_NAME)
            .is_some()
    {
        return None;
    }
    let owner = tako_core::deployment_app_id(config.name.as_deref()?, env);
    records.iter().find_map(|record| {
        (record.channel_owner.as_deref() == Some(owner.as_str())).then_some(record.name.as_str())
    })
}

/// Display a server-reported SQL or internal channel grant scope.
pub(crate) fn database_user_label(scope: &str) -> String {
    scope
        .strip_suffix("#channels")
        .map_or_else(|| scope.to_owned(), |app| format!("{app} (channel replay)"))
}

/// Whether the environment's routes need `ssl.cloudflare` and it isn't set.
pub(crate) fn missing_cloudflare_token(
    config: &TakoToml,
    secrets: &SecretsStore,
    env: &str,
) -> bool {
    let provider = config.get_ssl_provider(env);
    let routes = config.get_routes(env).unwrap_or_default();
    let needs = provider == tako_core::SslProvider::Cloudflare
        || (provider == tako_core::SslProvider::LetsEncrypt
            && crate::validation::letsencrypt_routes_need_cloudflare_token(&routes));
    needs && !secrets.contains_credential(env, SSL_CLOUDFLARE_CREDENTIAL_NAME)
}

/// Names of the environment's secrets and credentials that expired or expire
/// within `days`, with whether each already expired.
pub(crate) fn expiring_values(secrets: &SecretsStore, env: &str, days: i64) -> Vec<(String, bool)> {
    let Some(values) = secrets.environments.get(env) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut check = |label: String, value: &crate::config::EncryptedSecretValue| {
        if value.is_expired().unwrap_or(false) {
            found.push((label, true));
        } else if value.is_expiring_within_days(days).unwrap_or(false) {
            found.push((label, false));
        }
    };
    for (name, value) in &values.app {
        check(name.clone(), value);
    }
    for (name, value) in &values.credentials {
        check(name.clone(), value);
    }
    for (name, storage) in &values.storages {
        check(format!("{name} access key"), &storage.access_key_id);
        check(format!("{name} secret key"), &storage.secret_access_key);
    }
    found.sort();
    found.dedup();
    found
}

/// Target servers that haven't received key grants for some of the
/// environment's current secret records, with how many each lacks.
pub(crate) fn missing_key_grants(
    env: &EnvResources,
    app: &str,
    inventory: &ServerInventory,
) -> Result<Vec<(String, usize)>, String> {
    let id = env.network_id.as_deref().ok_or("no network")?;
    let aliases = LocalNetworks::load().map_err(|error| error.to_string())?;
    let alias = aliases
        .select(Some(id))
        .map_err(|error| error.to_string())?;
    let scope = format!("{app}/{}", env.name)
        .parse::<tako_network::AppEnv>()
        .map_err(|error| error.to_string())?;
    let (_, cache) = crate::network::state::pull(alias, Some(scope.clone()))
        .map_err(|error| error.to_string())?;
    let latest = cache.latest().map_err(|error| error.to_string())?;
    let holders: Vec<BTreeSet<String>> = latest
        .values()
        .filter(|record| {
            record.address.scope == scope
                && record.address.kind.is_secret()
                && record.value.is_some()
        })
        .map(|record| record.keys.keys().map(ToString::to_string).collect())
        .collect();
    let servers: Vec<(String, String)> = env
        .servers
        .iter()
        .filter_map(|name| {
            let target = inventory.servers.get(name)?.native.as_ref()?;
            Some((name.clone(), target.server_id.to_string()))
        })
        .collect();
    Ok(count_missing_grants(&holders, &servers))
}

/// For each `(server name, device ID)`, how many records' key holders lack it.
pub(crate) fn count_missing_grants(
    holders: &[BTreeSet<String>],
    servers: &[(String, String)],
) -> Vec<(String, usize)> {
    servers
        .iter()
        .filter_map(|(name, id)| {
            let missing = holders.iter().filter(|keys| !keys.contains(id)).count();
            (missing > 0).then(|| (name.clone(), missing))
        })
        .collect()
}

#[cfg(test)]
mod tests;

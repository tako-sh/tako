//! Preparing an environment's databases before its release command and
//! rollout: backup destinations and grants on the databases' servers (for
//! administrators), and a snapshot before the release command.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use tako_core::{Command, SnapshotReason};

use crate::commands::sqlite::target::{Home, PlannedBinding, resolve_home};
use crate::config::{SecretsStore, ServerInventory, TakoToml};
use crate::network::databases::DatabaseRecord;
use crate::output;

pub(super) struct Preparation<'a> {
    pub plan: &'a HashMap<String, PlannedBinding>,
    /// The network's database records.
    pub records: &'a [DatabaseRecord],
    pub config: &'a TakoToml,
    pub servers: &'a ServerInventory,
    pub secrets: &'a SecretsStore,
    pub project_dir: &'a Path,
    pub env: &'a str,
    /// The deployment's `app/env` identity.
    pub app: &'a str,
    /// Inventory keys of the servers this deploy targets.
    pub targets: &'a [String],
    pub administrator: bool,
    pub release: bool,
}

/// Prepares the environment's databases and returns the peer keys their
/// servers issued with grants, by server device ID.
pub(super) async fn prepare(
    prep: Preparation<'_>,
) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let _t = output::timed("Prepare databases");
    let (keys, warnings) =
        output::with_spinner_async("Preparing databases", "Prepared databases", async {
            run(&prep).await.map_err(|error| error.to_string())
        })
        .await?;
    for warning in warnings {
        output::warning(&warning);
    }
    Ok(keys)
}

/// Sets each binding's peer key from the grant of its database's server.
pub(super) fn apply_peer_keys(
    plan: &mut HashMap<String, PlannedBinding>,
    keys: &HashMap<String, String>,
) {
    for planned in plan.values_mut() {
        if planned.access == tako_core::DatabaseAccess::Network {
            planned.binding.peer_key = keys.get(&planned.home.device_id).cloned();
        }
    }
}

type Prepared = (HashMap<String, String>, Vec<String>);

async fn run(prep: &Preparation<'_>) -> Result<Prepared, Box<dyn std::error::Error>> {
    // Distinct databases bound by this environment.
    let mut bound: BTreeMap<String, &PlannedBinding> = BTreeMap::new();
    for planned in prep.plan.values() {
        bound
            .entry(planned.database_name.clone())
            .or_insert(planned);
    }

    if prep.administrator {
        for planned in bound.values() {
            // Without a backup storage the server stops snapshotting the
            // database.
            let storage = crate::commands::storage::decrypt_database_backup_storage(
                prep.env,
                prep.config,
                &planned.database_name,
                prep.secrets,
                Some(prep.project_dir),
            )?;
            send(
                &planned.home,
                Command::ConfigureDatabaseBackup {
                    name: planned.database_name.clone(),
                    backup: storage,
                },
            )
            .await
            .map_err(|e| format!("Configure backups of {}: {e}", planned.database_name))?;
        }
    }
    let (keys, warnings) = if prep.administrator {
        grant(prep, &bound).await?
    } else {
        (HashMap::new(), Vec::new())
    };

    if prep.release {
        for planned in bound.values() {
            send(
                &planned.home,
                Command::SnapshotDatabase {
                    name: planned.database_name.clone(),
                    reason: SnapshotReason::Release,
                    // Administrators may snapshot any database, including
                    // one this deploy binds for the first time; deployers
                    // without administration name their app.
                    app: (!prep.administrator).then(|| prep.app.to_owned()),
                },
            )
            .await
            .map_err(|e| {
                format!(
                    "Snapshot {} before the release command: {e}",
                    planned.database_name
                )
            })?;
        }
    }
    Ok((keys, warnings))
}

/// Grants the environment its network-access databases on their servers when
/// it deploys elsewhere, and clears its grants on the other servers that host
/// network-access databases. Returns the peer keys of the grants, and
/// warnings for servers that could not clear a grant this deploy does not
/// depend on.
async fn grant(
    prep: &Preparation<'_>,
    bound: &BTreeMap<String, &PlannedBinding>,
) -> Result<Prepared, Box<dyn std::error::Error>> {
    let network = crate::commands::sqlite::target::env_network(prep.config, prep.env)?;
    let mut homes: BTreeMap<String, (Home, BTreeSet<String>)> = BTreeMap::new();
    for record in prep.records {
        if record.access != tako_core::DatabaseAccess::Network {
            continue;
        }
        let Ok(home) = resolve_home(prep.servers, network, &record.server, prep.env) else {
            continue;
        };
        homes
            .entry(home.device_id.clone())
            .or_insert_with(|| (home, BTreeSet::new()));
    }
    for planned in bound.values() {
        if planned.access != tako_core::DatabaseAccess::Network {
            continue;
        }
        let remote = prep
            .targets
            .iter()
            .any(|target| *target != planned.home.key);
        if remote && let Some((_, names)) = homes.get_mut(&planned.home.device_id) {
            names.insert(planned.database_name.clone());
        }
    }
    let mut keys = HashMap::new();
    let mut warnings = Vec::new();
    for (home, names) in homes.into_values() {
        let needed = !names.is_empty();
        let result: Result<serde_json::Value, _> = crate::commands::sqlite::send(
            &home,
            Command::GrantDatabases {
                app: prep.app.to_owned(),
                databases: names.into_iter().collect(),
            },
        )
        .await;
        match result {
            Ok(response) => {
                if let Some(key) = response["peer_key"].as_str() {
                    keys.insert(home.device_id.clone(), key.to_owned());
                }
            }
            Err(error) if needed => {
                return Err(format!("Grant databases on {}: {error}", home.name).into());
            }
            // Only a stale grant is left behind; this deploy does not use it.
            Err(error) => warnings.push(format!(
                "Could not clear {}'s database grant on {}: {error}",
                prep.app, home.name
            )),
        }
    }
    Ok((keys, warnings))
}

async fn send(home: &Home, command: Command) -> Result<(), Box<dyn std::error::Error>> {
    let _: serde_json::Value = crate::commands::sqlite::send(home, command).await?;
    Ok(())
}

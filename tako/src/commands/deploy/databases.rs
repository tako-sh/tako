//! Preparing an environment's databases before its release command and
//! rollout: backup destinations and grants on the databases' servers (for
//! administrators), a snapshot before the release command, and the
//! project's migrations. KV stores take part in grants only: they have no
//! snapshots or migrations.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use tako_core::{Command, SnapshotReason};

use crate::commands::sqlite::target::{Home, PlannedBinding, resolve_home};
use crate::config::{SecretsStore, ServerInventory, TakoToml};
use crate::network::databases::DatabaseRecord;
use crate::output;

mod plan;
pub(super) use plan::{Plan, plan};

/// Verify each physical database before changing any of them. A stale
/// directory entry must not grant access to a replacement with the same name.
pub(super) async fn provision(
    plan: &Plan,
    network: &crate::config::LocalNetwork,
    app: &str,
    records: &mut Vec<DatabaseRecord>,
) -> Result<(), Box<dyn std::error::Error>> {
    if plan.sql.is_empty() && plan.kv.is_empty() && plan.replay.is_none() {
        return Ok(());
    }
    // A move can leave the same generation on the old home. Verify the live
    // directory as well as the database files before preparing a built release.
    let current = tokio::task::block_in_place(|| crate::network::databases::list(network))?;
    let mut checked = BTreeSet::new();
    let mut hosted = HashMap::new();
    for entry in plan
        .sql
        .values()
        .chain(plan.kv.values())
        .chain(plan.replay.iter())
    {
        if !checked.insert(&entry.database_name) {
            continue;
        }
        if !hosted.contains_key(&entry.home.device_id) {
            #[derive(serde::Deserialize)]
            struct Status {
                databases: Vec<tako_core::DatabaseInfo>,
            }
            let status: Status =
                crate::commands::sqlite::send(&entry.home, Command::DatabaseStatus { name: None })
                    .await?;
            hosted.insert(entry.home.device_id.clone(), status.databases);
        }
        let name = entry.database_name.as_str();
        let planned = records.iter().find(|r| r.name == name);
        if current.iter().find(|r| r.name == name) != planned {
            return Err(format!(
                "Placement of database '{name}' changed while building. Retry the deploy."
            )
            .into());
        }
        let expected = planned.map(|r| r.generation.as_str());
        let actual = hosted[&entry.home.device_id]
            .iter()
            .find(|db| db.name == name)
            .map(|db| db.generation.as_str());
        if expected != actual {
            return Err(format!("The network record for database '{name}' does not match what {} hosts. Repair its placement before deploying.", entry.home.name).into());
        }
    }
    *records = current;
    for (name, home) in &plan.create {
        // Membership may have changed while building the release.
        tokio::task::block_in_place(|| -> crate::network::Result<()> {
            let profile = crate::network::client::refresh(network)?;
            let members = crate::network::server_members(&profile)?;
            if members.len() != 1 || !members.contains_key(&home.device_id.parse()?) {
                return Err("Network placement changed while building. Retry the deploy to plan database creation.".into());
            }
            Ok(())
        })?;
        let owner = (plan.implicit_replay
            && plan
                .replay
                .as_ref()
                .is_some_and(|r| r.database_name == *name))
        .then_some(app);
        records.push(
            crate::commands::sqlite::create_database(
                network,
                home,
                name,
                owner,
                tako_core::DatabaseEngine::Sqlite,
            )
            .await?,
        );
        output::success(&format!("Created database {name} on {}", home.name));
    }
    Ok(())
}

pub(super) struct Preparation<'a> {
    pub plan: &'a Plan,
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

/// What preparing the databases leaves for the deploy.
pub(super) struct Granted {
    /// Peer keys the databases' servers issued with grants, by server
    /// device ID.
    pub peer_keys: HashMap<String, String>,
    pub replay_peer_keys: HashMap<String, String>,
    /// The exact grant each database server should end with. Grants are only
    /// widened before the rollout, since the previous release keeps serving
    /// until it succeeds; [`narrow_grants`] applies these afterwards.
    narrow: Vec<(String, Home, Vec<String>)>,
}

/// Prepares the environment's databases.
pub(super) async fn prepare(prep: Preparation<'_>) -> Result<Granted, Box<dyn std::error::Error>> {
    let _t = output::timed("Prepare databases");
    let prepared = output::with_spinner_async("Preparing databases", "Prepared databases", async {
        run(&prep).await.map_err(|error| error.to_string())
    })
    .await?;
    for warning in prepared.warnings {
        output::warning(&warning);
    }
    for line in prepared.migrated {
        output::info(&line);
    }
    Ok(Granted {
        peer_keys: prepared.keys,
        replay_peer_keys: prepared.replay_keys,
        narrow: prepared.narrow,
    })
}

/// After a successful rollout, revokes the grants the new release no longer
/// uses. A server that cannot be reached keeps a stale grant, with a warning.
pub(super) async fn narrow_grants(granted: Granted) {
    for (app, home, databases) in granted.narrow {
        let result: Result<serde_json::Value, _> = crate::commands::sqlite::send(
            &home,
            Command::GrantDatabases {
                app: app.to_owned(),
                databases,
                keep_existing: false,
            },
        )
        .await;
        if let Err(error) = result {
            output::warning(&format!(
                "Could not clear {app}'s unused database grants on {}: {error}",
                home.name
            ));
        }
    }
}

/// Sets each binding's peer key from the grant of its database's server.
pub(super) fn apply_peer_keys(
    plan: &mut HashMap<String, PlannedBinding>,
    keys: &HashMap<String, String>,
) {
    for planned in plan.values_mut() {
        planned.binding.peer_key = keys.get(&planned.home.device_id).cloned();
    }
}

#[derive(Default)]
struct Prepared {
    /// Peer keys of the grants, by server device ID.
    keys: HashMap<String, String>,
    replay_keys: HashMap<String, String>,
    warnings: Vec<String>,
    /// One line per binding that applied migrations.
    migrated: Vec<String>,
    narrow: Vec<(String, Home, Vec<String>)>,
}

async fn run(prep: &Preparation<'_>) -> Result<Prepared, Box<dyn std::error::Error>> {
    // Distinct databases and KV stores bound by this environment. KV stores
    // are only granted: they have no backups, snapshots or migrations.
    let mut bound: BTreeMap<String, &PlannedBinding> = BTreeMap::new();
    for planned in prep.plan.sql.values().chain(prep.plan.kv.values()) {
        bound
            .entry(planned.database_name.clone())
            .or_insert(planned);
    }

    let sqlite =
        |planned: &&&PlannedBinding| planned.binding.engine == tako_core::DatabaseEngine::Sqlite;
    if prep.administrator {
        let mut backed_up = bound.clone();
        if let Some(replay) = &prep.plan.replay {
            backed_up.insert(replay.database_name.clone(), replay);
        }
        for planned in backed_up.values().filter(sqlite) {
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
    let mut prepared = if prep.administrator {
        let mut prepared = Prepared::default();
        prepared.keys = grant(prep, prep.app, bound.values().copied(), &mut prepared).await?;
        prepared.replay_keys = grant(
            prep,
            &tako_core::channel_replay_grant_scope(prep.app),
            prep.plan.replay.iter(),
            &mut prepared,
        )
        .await?;
        prepared
    } else {
        Prepared::default()
    };

    if prep.release {
        for planned in bound.values().filter(sqlite) {
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

    // Migrations run before the release command and the rollout, so new
    // instances start on the new schema; old ones keep serving meanwhile.
    let owner = prep.app.split('/').next().unwrap_or(prep.app);
    let selected = crate::database_migrations::by_database(
        prep.project_dir,
        prep.config,
        prep.plan
            .sql
            .iter()
            .map(|(binding, planned)| (binding.as_str(), planned.database_name.as_str())),
    )?;
    for (database, selected) in selected {
        let (binding, migrations) = (&selected.binding, selected.migrations);
        let planned = &prep.plan.sql[binding];
        debug_assert_eq!(planned.database_name, database);
        let report = crate::commands::sqlite::apply_migrations(
            &planned.home,
            &planned.database_name,
            owner,
            migrations,
            false,
            (!prep.administrator).then(|| prep.app.to_owned()),
        )
        .await
        .map_err(|e| format!("Migrate {binding} ({}): {e}", planned.database_name))?;
        prepared.warnings.extend(
            report
                .warnings
                .iter()
                .map(|warning| format!("{binding}: {warning}")),
        );
        if !report.applied.is_empty() {
            prepared.migrated.push(format!(
                "Applied {} to {binding}",
                report.applied.join(", ")
            ));
        }
    }
    Ok(prepared)
}

/// Grants the environment its databases on their servers when it deploys
/// elsewhere, keeping the grants its running release uses, and records the
/// exact grant each server of the network's databases should end with once
/// the rollout succeeds. Returns the peer keys of the grants, and warnings
/// for servers that could not be reached for a grant this deploy does not
/// depend on.
async fn grant<'a>(
    prep: &Preparation<'_>,
    scope: &str,
    bound: impl Iterator<Item = &'a PlannedBinding>,
    prepared: &mut Prepared,
) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let network = crate::commands::sqlite::target::env_network(prep.env)?;
    let mut homes: BTreeMap<String, (Home, BTreeSet<String>)> = BTreeMap::new();
    for record in prep.records {
        let Ok(home) = resolve_home(prep.servers, &network, &record.server, prep.env) else {
            continue;
        };
        homes
            .entry(home.device_id.clone())
            .or_insert_with(|| (home, BTreeSet::new()));
    }
    for planned in bound {
        let remote = prep
            .targets
            .iter()
            .any(|target| *target != planned.home.key);
        if remote && let Some((_, names)) = homes.get_mut(&planned.home.device_id) {
            names.insert(planned.database_name.clone());
        }
    }
    let mut keys = HashMap::new();
    for (home, names) in homes.into_values() {
        let needed = !names.is_empty();
        let names: Vec<String> = names.into_iter().collect();
        // Old instances keep running until the rollout succeeds, so grants
        // they use stay until then.
        let result: Result<serde_json::Value, _> = crate::commands::sqlite::send(
            &home,
            Command::GrantDatabases {
                app: scope.to_owned(),
                databases: names.clone(),
                keep_existing: true,
            },
        )
        .await;
        prepared
            .narrow
            .push((scope.to_owned(), home.clone(), names));
        match result {
            Ok(response) => {
                if let Some(key) = response["peer_key"].as_str() {
                    keys.insert(home.device_id.clone(), key.to_owned());
                } else if needed {
                    return Err(
                        format!("Grant on {} returned no peer key for {scope}", home.name).into(),
                    );
                }
            }
            Err(error) if needed => {
                return Err(format!("Grant databases on {}: {error}", home.name).into());
            }
            // This deploy uses nothing there; clearing its grant is retried
            // after the rollout.
            Err(error) => prepared.warnings.push(format!(
                "Could not reach {} to update {}'s database grant: {error}",
                home.name, prep.app
            )),
        }
    }
    Ok(keys)
}

async fn send(home: &Home, command: Command) -> Result<(), Box<dyn std::error::Error>> {
    let _: serde_json::Value = crate::commands::sqlite::send(home, command).await?;
    Ok(())
}

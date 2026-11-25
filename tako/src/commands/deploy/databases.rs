//! Prepare Raft replicas and grants before rollout, then configure backups,
//! take pre-release snapshots, and apply SQL migrations.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use tako_core::{Command, SnapshotReason};

use crate::commands::db::target::{Home, PlannedBinding};
use crate::config::{SecretsStore, ServerInventory, TakoToml};
use crate::network::databases::DatabaseRecord;
use crate::output;

mod plan;
mod replication;
pub(super) use plan::{Plan, Service, plan};

/// Verify each physical database before changing any of them. A stale
/// directory entry must not grant access to a replacement with the same name.
pub(super) async fn provision(
    plan: &mut Plan,
    inventory: &ServerInventory,
    env: &str,
    target_names: &[String],
    tree: Option<&super::DeployTaskTreeController>,
    network: &crate::config::LocalNetwork,
    app: &str,
    records: &mut Vec<DatabaseRecord>,
) -> Result<(), Box<dyn std::error::Error>> {
    if plan.sql.is_empty() && plan.kv.is_empty() && plan.service_bindings().next().is_none() {
        return Ok(());
    }
    // Recheck the generation in the live directory after the build.
    let current = tokio::task::block_in_place(|| crate::network::databases::list(network))?;
    let mut checked = BTreeSet::new();
    for entry in plan
        .sql
        .values()
        .chain(plan.kv.values())
        .chain(plan.service_bindings().map(|(_, planned)| planned))
    {
        if !checked.insert(&entry.database_name) {
            continue;
        }
        let name = &entry.database_name;
        let planned = records.iter().find(|r| &r.name == name);
        let latest = current.iter().find(|r| &r.name == name);
        if plan.create.contains_key(name) {
            if let Some(record) = latest {
                let expected_owner = |service: &plan::ServicePlan| {
                    service.implicit
                        && service
                            .binding
                            .as_ref()
                            .is_some_and(|p| &p.database_name == name)
                };
                if record.engine != entry.engine
                    || record.channel_owner.as_deref()
                        != expected_owner(&plan.replay).then_some(app)
                    || record.workflows_owner.as_deref()
                        != expected_owner(&plan.workflows).then_some(app)
                {
                    return Err(format!("Resource '{name}' was concurrently created for a different purpose. Retry the deploy.").into());
                }
            }
            continue;
        }
        if planned != latest {
            return Err(format!(
                "Placement of database '{name}' changed while building. Retry the deploy."
            )
            .into());
        }
    }
    *records = current;
    replication::prepare_replicas(
        plan,
        network,
        inventory,
        env,
        app,
        target_names,
        records,
        tree,
    )
    .await
}

/// Resolve leadership with the caller's existing application permission.
pub(super) async fn scoped_homes(
    plan: &mut Plan,
    inventory: &ServerInventory,
    network: &str,
    env: &str,
    app: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let entries = plan
        .sql
        .values_mut()
        .chain(plan.kv.values_mut())
        .chain(plan.replay.binding.iter_mut())
        .chain(plan.workflows.binding.iter_mut());
    let mut homes = Vec::new();
    let mut leaders = HashMap::new();
    for binding in entries {
        let Some(group) = binding.generation.as_ref() else {
            continue;
        };
        if homes.is_empty() {
            homes =
                crate::commands::helpers::resolve_network_servers(inventory, network, &[], env)?
                    .iter()
                    .map(|key| crate::commands::db::target::inventory_home(inventory, key))
                    .collect::<Result<Vec<_>, _>>()?;
        }
        if !leaders.contains_key(group) {
            let elected =
                crate::commands::db::replication::discover(&homes, group, Some(app)).await?;
            leaders.insert(group.clone(), elected);
        }
        binding.home = leaders[group].clone();
    }
    Ok(())
}

pub(super) struct Preparation<'a> {
    pub plan: &'a Plan,
    pub config: &'a TakoToml,
    pub secrets: &'a SecretsStore,
    pub project_dir: &'a Path,
    pub env: &'a str,
    /// The deployment's `app/env` identity.
    pub app: &'a str,
    pub administrator: bool,
    pub release: bool,
}

/// Prepares the environment's databases.
pub(super) async fn prepare(prep: Preparation<'_>) -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
}

#[derive(Default)]
struct Prepared {
    warnings: Vec<String>,
    /// One line per binding that applied migrations.
    migrated: Vec<String>,
}

async fn run(prep: &Preparation<'_>) -> Result<Prepared, Box<dyn std::error::Error>> {
    // Distinct databases and KV stores bound by this environment. Both get
    // backups, grants and release snapshots; only databases get migrations.
    let mut bound: BTreeMap<String, &PlannedBinding> = BTreeMap::new();
    for planned in prep.plan.sql.values().chain(prep.plan.kv.values()) {
        bound
            .entry(planned.database_name.clone())
            .or_insert(planned);
    }

    if prep.administrator {
        let mut backed_up = bound.clone();
        for (_, planned) in prep.plan.service_bindings() {
            backed_up.insert(planned.database_name.clone(), planned);
        }
        for planned in backed_up.values() {
            // Without a backup storage the server stops backing up the
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
    let mut prepared = Prepared::default();

    if prep.release {
        for planned in bound.values() {
            send(
                &planned.home,
                Command::SnapshotDatabase {
                    name: planned.database_name.clone(),
                    reason: SnapshotReason::Release,
                    // Administrators may back up any database, including
                    // one this deploy binds for the first time; deployers
                    // without administration name their app.
                    app: (!prep.administrator).then(|| prep.app.to_owned()),
                },
            )
            .await
            .map_err(|e| {
                format!(
                    "Back up {} before the release command: {e}",
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
        let report = crate::commands::db::apply_migrations(
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

async fn send(home: &Home, command: Command) -> Result<(), Box<dyn std::error::Error>> {
    let _: serde_json::Value = crate::commands::db::send(home, command).await?;
    Ok(())
}

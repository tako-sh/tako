//! Creating a name is a catalog transaction. Each winning group subsequently
//! elects its own leader and commits independently of the catalog.
use super::*;

use crate::commands::db::replication::{catalog, claim, command, leader, reserve};
use futures_util::future::join_all;
use std::time::Duration;
use tako_core::StorageOperation as Op;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(super) async fn prepare_replicas(
    plan: &mut Plan,
    network: &crate::config::LocalNetwork,
    inventory: &ServerInventory,
    env: &str,
    app: &str,
    target_names: &[String],
    records: &mut Vec<DatabaseRecord>,
    tree: Option<&crate::commands::deploy::DeployTaskTreeController>,
) -> Result<()> {
    use crate::commands::db::target::inventory_home;
    let homes =
        crate::commands::helpers::resolve_network_servers(inventory, &network.id, &[], env)?
            .iter()
            .map(|key| inventory_home(inventory, key))
            .collect::<std::result::Result<Vec<_>, _>>()?;
    let targets = target_names
        .iter()
        .map(|key| inventory_home(inventory, key))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let names: BTreeSet<_> = plan
        .sql
        .values()
        .chain(plan.kv.values())
        .chain(plan.service_bindings().map(|(_, p)| p))
        .map(|p| p.database_name.clone())
        .collect();
    // Grow the name catalog as the network grows, including when every
    // requested resource already exists. Existing data groups can deploy
    // independently while the catalog is temporarily unavailable.
    let mut registry = if !names.is_empty() && homes.len() > 1 {
        match catalog(&homes).await {
            Ok(home) => Some(home),
            Err(e)
                if names
                    .iter()
                    .all(|name| records.iter().any(|r| &r.name == name)) =>
            {
                crate::output::warning(&format!(
                    "Storage catalog membership could not be updated: {e}"
                ));
                None
            }
            Err(e) => return Err(e),
        }
    } else {
        None
    };
    for name in names {
        let record = if let Some(record) = records.iter().find(|r| r.name == name) {
            if record.founders.is_empty() {
                continue;
            }
            record.clone()
        } else {
            if registry.is_none() {
                registry = Some(catalog(&homes).await?);
            }
            let is_owner = |service: &plan::ServicePlan| {
                service.implicit
                    && service
                        .binding
                        .as_ref()
                        .is_some_and(|p| p.database_name == name)
            };
            let record = reserve(
                network,
                registry.as_ref().unwrap(),
                &targets,
                &name,
                plan.create
                    .get(&name)
                    .ok_or("Missing storage creation plan")?
                    .binding
                    .engine,
                (
                    is_owner(&plan.replay).then(|| app.to_owned()),
                    is_owner(&plan.workflows).then(|| app.to_owned()),
                ),
            )
            .await?;
            records.push(record.clone());
            record
        };
        let key = tokio::task::block_in_place(|| {
            crate::network::databases::backup_key(network, &record)
        })?;
        let generation = &record.generation;
        let aliases: Vec<_> = plan
            .sql
            .iter()
            .chain(&plan.kv)
            .map(|(a, p)| (a.to_owned(), p))
            .chain(plan.service_bindings().map(|(a, p)| (a.to_owned(), p)))
            .filter(|(_, p)| p.database_name == name)
            .map(|(a, _)| a.to_owned())
            .collect();
        for target in &targets {
            if let Some(tree) = tree {
                for alias in &aliases {
                    tree.storage_step(&target.key, alias, None, 0, false);
                }
            }
        }
        // Members in a winning simultaneous first deploy can be outside our targets.
        // An already functioning group only needs the new deployment targets.
        let active = join_all(homes.iter().map(|h| {
            tokio::time::timeout(
                Duration::from_secs(2),
                command::<serde_json::Value>(
                    h,
                    Op::Progress {
                        group: generation.clone(),
                    },
                ),
            )
        }))
        .await;
        let mut participants = targets.clone();
        if !active
            .iter()
            .any(|r| matches!(r,Ok(Ok(p)) if p["leader"].is_string()))
        {
            for founder in &record.founders {
                if !participants.iter().any(|h| &h.device_id == founder) {
                    participants.push(
                        homes
                            .iter()
                            .find(|h| &h.device_id == founder)
                            .ok_or("Initial storage member is unavailable")?
                            .clone(),
                    );
                }
            }
        }
        let mut initial_bytes = HashMap::new();
        for home in &participants {
            let initial: serde_json::Value = command(
                home,
                Op::OpenReplica {
                    claim: claim(&record),
                    backup_key: key.to_string(),
                },
            )
            .await?;
            initial_bytes.insert(home.key.clone(), initial["received"].as_u64().unwrap_or(0));
        }
        let elected = leader(&homes, generation).await?;
        let _: serde_json::Value = command(
            &elected,
            Op::SeedReplica {
                name: name.clone(),
                generation: generation.clone(),
            },
        )
        .await?;
        let mut ticket = [0; 16];
        getrandom::fill(&mut ticket)?;
        let ticket = hex::encode(ticket);
        let _: tako_core::ResourcePlacement = command(
            &elected,
            Op::Placement {
                group: generation.clone(),
                change: Some(tako_core::PlacementChange::Reserve {
                    app: app.into(),
                    servers: targets.iter().map(|h| h.device_id.clone()).collect(),
                    founders: record.founders.clone(),
                    ticket: ticket.clone(),
                }),
            },
        )
        .await?;
        plan.reservations.insert(generation.clone(), ticket);
        let join = command::<serde_json::Value>(
            &elected,
            Op::AddMembers {
                group: generation.clone(),
                targets: participants.iter().map(|h| h.device_id.clone()).collect(),
            },
        );
        let ready = futures_util::future::try_join_all(targets.iter().map(|home| {
            let aliases = &aliases;
            let initial_bytes = &initial_bytes;
            let elected = &elected;
            async move {
                let mut barrier = None;
                loop {
                    let progress: serde_json::Value = command(
                        elected,
                        Op::Readiness {
                            group: generation.clone(),
                            target: home.device_id.clone(),
                            barrier,
                        },
                    )
                    .await?;
                    let index = progress["barrier"]
                        .as_u64()
                        .ok_or("missing storage readiness position")?;
                    barrier = Some(index);
                    let done = progress["applied"].as_u64().is_some_and(|a| a >= index);
                    let bytes = serde_json::from_value::<Option<(u64, u64)>>(
                        progress["snapshot_bytes"].clone(),
                    )?;
                    if let Some(tree) = tree {
                        for alias in aliases {
                            tree.storage_step(
                                &home.key,
                                alias,
                                bytes,
                                progress["received"]
                                    .as_u64()
                                    .unwrap_or(0)
                                    .saturating_sub(initial_bytes[&home.key]),
                                done,
                            );
                        }
                    }
                    if done {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Ok::<_, Box<dyn std::error::Error>>(())
            }
        }));
        tokio::try_join!(join, ready)?;
        for (scope, uses) in [
            (
                app.to_owned(),
                plan.sql
                    .values()
                    .chain(plan.kv.values())
                    .any(|p| p.database_name == name),
            ),
            (
                tako_core::channel_replay_grant_scope(app),
                plan.replay
                    .binding
                    .as_ref()
                    .is_some_and(|p| p.database_name == name),
            ),
            (
                tako_core::workflows_grant_scope(app),
                plan.workflows
                    .binding
                    .as_ref()
                    .is_some_and(|p| p.database_name == name),
            ),
        ] {
            if !uses {
                continue;
            }
            let key: serde_json::Value = command(
                &elected,
                Op::SetGrant {
                    generation: generation.clone(),
                    scope: scope.clone(),
                    enabled: true,
                },
            )
            .await?;
            let key = key["key"]
                .as_str()
                .ok_or("missing replicated resource grant")?;
            if scope == app {
                for p in plan
                    .sql
                    .values_mut()
                    .chain(plan.kv.values_mut())
                    .filter(|p| p.database_name == name)
                {
                    p.binding.peer_key = Some(key.to_owned());
                }
            } else if scope == tako_core::channel_replay_grant_scope(app) {
                plan.replay.binding.as_mut().unwrap().binding.peer_key = Some(key.to_owned());
            } else {
                plan.workflows.binding.as_mut().unwrap().binding.peer_key = Some(key.to_owned());
            }
        }
        for planned in plan
            .sql
            .values_mut()
            .chain(plan.kv.values_mut())
            .chain(plan.replay.binding.iter_mut())
            .chain(plan.workflows.binding.iter_mut())
            .filter(|p| p.database_name == name)
        {
            planned.binding.generation = Some(generation.clone());
            planned.binding.server = record.server.clone();
            planned.home = elected.clone();
        }
    }
    Ok(())
}

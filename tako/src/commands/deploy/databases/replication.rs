//! Creating a name is a catalog transaction. Each winning group subsequently
//! elects its own leader and commits independently of the catalog.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use futures_util::future::join_all;
use std::time::{Duration, Instant};
use tako_core::{StorageClaim, StorageOperation as Op};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

async fn command<T: serde::de::DeserializeOwned>(home: &Home, request: Op) -> Result<T> {
    crate::commands::db::send(home, Command::CoordinateStorage { request }).await
}

async fn leader(homes: &[Home], group: &str) -> Result<Home> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let replies = join_all(homes.iter().map(|home| {
            command::<serde_json::Value>(
                home,
                Op::Progress {
                    group: group.to_owned(),
                },
            )
        }))
        .await;
        for (home, reply) in homes.iter().zip(replies) {
            if let Ok(reply) = reply
                && reply["leader"].is_string()
                && reply["here"] == reply["leader"]
            {
                return Ok(home.clone());
            }
        }
        if Instant::now() >= deadline {
            return Err(
                format!("Could not reach an elected leader for storage group {group}.").into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn catalog(homes: &[Home]) -> Result<Home> {
    let replies = join_all(
        homes
            .iter()
            .map(|home| command::<serde_json::Value>(home, Op::OpenCatalog { key: None })),
    )
    .await;
    let key=replies.iter().filter_map(|r|r.as_ref().ok()).find_map(|r|r["key"].as_str()).ok_or("Cannot establish the storage catalog. The first enrollment or an existing catalog member must be reachable.")?;
    let mut opened = Vec::new();
    for (home, reply) in homes.iter().zip(&replies) {
        if let Ok(result) = reply
            && result["key"].as_str() != Some(key)
        {
            return Err("Servers disagree about the network storage catalog.".into());
        }
        if reply.is_ok()
            || command::<serde_json::Value>(
                home,
                Op::OpenCatalog {
                    key: Some(key.to_owned()),
                },
            )
            .await
            .is_ok()
        {
            opened.push(home.clone());
        }
    }
    let home = leader(&opened, "catalog").await?;
    command::<serde_json::Value>(
        &home,
        Op::AddMembers {
            group: "catalog".into(),
            targets: opened.iter().map(|h| h.device_id.clone()).collect(),
        },
    )
    .await?;
    Ok(home)
}

fn claim(record: &DatabaseRecord) -> StorageClaim {
    StorageClaim {
        name: record.name.clone(),
        engine: record.engine,
        generation: record.generation.clone(),
        founders: record.founders.clone(),
        created_at_unix_secs: record.created_at_unix_secs,
        channel_owner: record.channel_owner.clone(),
        workflows_owner: record.workflows_owner.clone(),
    }
}

async fn reserve(
    network: &crate::config::LocalNetwork,
    catalog: &Home,
    targets: &[Home],
    name: &str,
    engine: tako_core::DatabaseEngine,
    owner: (Option<String>, Option<String>),
) -> Result<DatabaseRecord> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes)?;
    let founders: BTreeSet<_> = targets
        .iter()
        .map(|h| h.device_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(3)
        .collect();
    let mut record = DatabaseRecord {
        name: name.to_owned(),
        engine,
        generation: hex::encode(bytes),
        created_at_unix_secs: crate::commands::db::unix_now(),
        server: founders.first().ok_or("no deployment targets")?.clone(),
        founders,
        channel_owner: owner.0,
        workflows_owner: owner.1,
    };
    let mut key = [0; 32];
    getrandom::fill(&mut key)?;
    tokio::task::block_in_place(|| {
        crate::network::databases::publish_key(network, &record, &BASE64.encode(key))
    })?;
    let winner: StorageClaim = command(
        catalog,
        Op::Claim {
            candidate: claim(&record),
        },
    )
    .await?;
    if record.generation != winner.generation {
        tokio::task::block_in_place(|| crate::network::databases::remove_key(network, &record))?;
    }
    if winner.name != name
        || winner.engine != engine
        || winner.channel_owner != record.channel_owner
        || winner.workflows_owner != record.workflows_owner
    {
        return Err(
            format!("Resource {name} was reserved for a different engine or service.").into(),
        );
    }
    record.server = winner
        .founders
        .first()
        .ok_or("catalog returned an empty initial membership")?
        .clone();
    record.generation = winner.generation;
    record.founders = winner.founders;
    record.created_at_unix_secs = winner.created_at_unix_secs;
    tokio::task::block_in_place(|| crate::network::databases::publish(network, &record, None))?;
    Ok(record)
}

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
    let mut registry = None;
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
                    tree.storage_step(&target.key, alias, None, false);
                }
            }
        }
        // Members in a winning simultaneous first deploy can be outside our targets.
        // An already functioning group only needs the new deployment targets.
        let active = join_all(homes.iter().map(|h| {
            command::<serde_json::Value>(
                h,
                Op::Progress {
                    group: generation.clone(),
                },
            )
        }))
        .await;
        let mut participants = targets.clone();
        if !active
            .iter()
            .any(|r| r.as_ref().is_ok_and(|p| p["leader"].is_string()))
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
        for home in &participants {
            let _: serde_json::Value = command(
                home,
                Op::OpenReplica {
                    claim: claim(&record),
                    backup_key: key.to_string(),
                },
            )
            .await?;
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
        let join = command::<serde_json::Value>(
            &elected,
            Op::AddMembers {
                group: generation.clone(),
                targets: participants.iter().map(|h| h.device_id.clone()).collect(),
            },
        );
        let ready = async {
            for home in &targets {
                let mut barrier = None;
                loop {
                    let progress: serde_json::Value = command(
                        &elected,
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
                        for alias in &aliases {
                            tree.storage_step(&home.key, alias, bytes, done);
                        }
                    }
                    if done {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        };
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

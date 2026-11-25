//! Retire a placement only after the selected server reports that its active
//! and archived release bindings no longer need it. A concurrent prepare
//! changes the reservation ticket before starting its rollout.
use super::replication::{catalog, command, leader};
use super::target::{Home, inventory_home};
use crate::config::{LocalNetwork, ServerInventory};
use crate::network::databases::DatabaseRecord;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use tako_core::{Command, PlacementChange as Change, ResourcePlacement, StorageOperation as Op};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(crate) async fn collect(
    network: &LocalNetwork,
    inventory: &ServerInventory,
    app: &str,
    targets: &[String],
    confirmed: &BTreeMap<String, String>,
) -> Result<()> {
    let records = tokio::task::block_in_place(|| crate::network::databases::list(network))?;
    let keys = crate::commands::helpers::resolve_network_servers(
        inventory,
        &network.id,
        &[],
        "storage cleanup",
    )?;
    let homes = keys
        .iter()
        .map(|key| inventory_home(inventory, key))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for record in records {
        if let Err(e) = one(
            &homes,
            app,
            targets,
            confirmed.get(&record.generation),
            &record,
        )
        .await
        {
            crate::output::warning(&format!("Cleanup of {} is pending: {e}", record.name));
        }
    }
    Ok(())
}
async fn change(home: &Home, group: &str, change: Option<Change>) -> Result<ResourcePlacement> {
    command(
        home,
        Op::Placement {
            group: group.into(),
            change,
        },
    )
    .await
}

async fn at_barrier(home: &Home, generation: &str, server: &str) -> Result<()> {
    let mut barrier = None;
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let progress: serde_json::Value = command(
                home,
                Op::Readiness {
                    group: generation.into(),
                    target: server.into(),
                    barrier,
                },
            )
            .await?;
            let index = progress["barrier"]
                .as_u64()
                .ok_or("missing retirement barrier")?;
            barrier = Some(index);
            if progress["applied"]
                .as_u64()
                .is_some_and(|applied| applied >= index)
            {
                return Ok::<(), Box<dyn std::error::Error>>(());
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await?
}

async fn one(
    homes: &[Home],
    app: &str,
    targets: &[String],
    ticket: Option<&String>,
    r: &DatabaseRecord,
) -> Result<()> {
    let group = &r.generation;
    let member = |id: &str| {
        homes
            .iter()
            .find(|h| h.device_id == id)
            .ok_or_else(|| format!("member {id} is unreachable"))
    };
    let home = match leader(homes, group).await {
        Ok(home) => home,
        Err(e) => {
            let registry = catalog(homes).await?;
            let retiring: Option<BTreeSet<String>> = command(
                &registry,
                Op::Retirement {
                    generation: group.clone(),
                    targets: None,
                },
            )
            .await?;
            if let Some(servers) = retiring {
                return finish(homes, r, &registry, &servers).await;
            }
            return Err(e);
        }
    };
    let mut placement = change(&home, group, None).await?;
    for key in targets {
        let Some(target) = homes.iter().find(|h| &h.key == key) else {
            continue;
        };
        let server = &target.device_id;
        let Some(usage) = placement.uses.get(server).and_then(|m| m.get(app)).cloned() else {
            continue;
        };
        let status: serde_json::Value = super::send(
            target,
            Command::DatabaseStatus {
                name: Some(r.name.clone()),
            },
        )
        .await?;
        let info = status["databases"]
            .as_array()
            .and_then(|items| items.first())
            .ok_or("resource status is missing")?;
        if info["generation"].as_str() != Some(group.as_str()) {
            return Err("resource generation changed".into());
        }
        let bindings = info["bindings"]
            .as_array()
            .ok_or("release binding history is unavailable")?;
        let referenced = bindings.iter().filter_map(|v| v.as_str()).any(|scope| {
            scope == app
                || scope
                    .strip_prefix(app)
                    .is_some_and(|suffix| suffix.starts_with('#'))
        });
        if usage.pending {
            if ticket == Some(&usage.ticket) && referenced {
                placement = change(
                    &home,
                    group,
                    Some(Change::Confirm {
                        app: app.into(),
                        server: server.clone(),
                        ticket: usage.ticket,
                    }),
                )
                .await?;
            }
        } else if !referenced {
            placement = change(
                &home,
                group,
                Some(Change::Release {
                    app: app.into(),
                    server: server.clone(),
                    ticket: usage.ticket,
                }),
            )
            .await?;
        }
    }
    if placement.retired {
        let registry = catalog(homes).await?;
        let recorded: Option<BTreeSet<String>> = command(
            &registry,
            Op::Retirement {
                generation: group.clone(),
                targets: None,
            },
        )
        .await?;
        if recorded.is_none() {
            for server in &placement.removing {
                at_barrier(&home, group, server).await?;
            }
            let _: serde_json::Value = command(
                &registry,
                Op::Retirement {
                    generation: group.clone(),
                    targets: Some(placement.removing.clone()),
                },
            )
            .await?;
        }
        return finish(homes, r, &registry, &placement.removing).await;
    }
    for server in &placement.removing {
        let target = member(server)?;
        let current = leader(homes, group).await?;
        // Once membership excludes the node it can only report its last
        // applied position. A retry may already have removed or purged it.
        let status = command::<serde_json::Value>(
            target,
            Op::Progress {
                group: group.clone(),
            },
        )
        .await;
        if status.as_ref().is_ok_and(|v| v["retiring"] != true) {
            at_barrier(&current, group, server).await?;
        }
        let _: serde_json::Value = command(
            &current,
            Op::RemoveMember {
                group: group.clone(),
                server: server.clone(),
            },
        )
        .await?;
        let _: serde_json::Value = command(
            target,
            Op::PurgeReplica {
                name: r.name.clone(),
                generation: group.clone(),
            },
        )
        .await?;
        change(
            &leader(homes, group).await?,
            group,
            Some(Change::Removed {
                server: server.clone(),
            }),
        )
        .await?;
    }
    Ok(())
}

/// Manual deletion uses the same permanent retirement fence as the last
/// release. A concurrent deploy alters the placement revision first.
pub(super) async fn delete_unused(homes: &[Home], r: &DatabaseRecord) -> Result<()> {
    let registry = catalog(homes).await?;
    let group = &r.generation;
    let recorded: Option<BTreeSet<String>> = command(
        &registry,
        Op::Retirement {
            generation: group.clone(),
            targets: None,
        },
    )
    .await?;
    if let Some(targets) = recorded {
        return finish(homes, r, &registry, &targets).await;
    }
    let home = leader(homes, group).await?;
    let mut placement = change(&home, group, None).await?;
    if !placement.retired {
        if !placement.uses.is_empty() {
            return Err(format!(
                "{} is reserved by an application or a retained release.",
                r.name
            )
            .into());
        }
        let targets = if placement.known.is_empty() {
            &r.founders
        } else {
            &placement.known
        };
        for id in targets {
            let target = homes
                .iter()
                .find(|h| &h.device_id == id)
                .ok_or("storage member is unavailable")?;
            let status: serde_json::Value = super::send(
                target,
                Command::DatabaseStatus {
                    name: Some(r.name.clone()),
                },
            )
            .await?;
            let info = status["databases"]
                .as_array()
                .and_then(|items| items.first())
                .ok_or("missing resource status")?;
            if info["generation"].as_str() != Some(group.as_str())
                || !info["bindings"].as_array().is_some_and(|b| b.is_empty())
            {
                return Err(format!(
                    "{} still has release bindings or changed generation on {}.",
                    r.name, target.name
                )
                .into());
            }
        }
        placement = change(
            &home,
            group,
            Some(Change::RetireUnused {
                revision: placement.revision,
                founders: r.founders.clone(),
            }),
        )
        .await?;
    }
    for id in &placement.removing {
        at_barrier(&home, group, id).await?;
    }
    let _: serde_json::Value = command(
        &registry,
        Op::Retirement {
            generation: group.clone(),
            targets: Some(placement.removing.clone()),
        },
    )
    .await?;
    finish(homes, r, &registry, &placement.removing).await
}

async fn finish(
    homes: &[Home],
    r: &DatabaseRecord,
    registry: &Home,
    targets: &BTreeSet<String>,
) -> Result<()> {
    for id in targets {
        let home = homes
            .iter()
            .find(|h| &h.device_id == id)
            .ok_or("retiring member is unreachable")?;
        let _: serde_json::Value = command(
            home,
            Op::PurgeReplica {
                name: r.name.clone(),
                generation: r.generation.clone(),
            },
        )
        .await?;
    }
    // The immutable generation marker survives simultaneous or late retries.
    // Its recovery key remains available for snapshots already in object storage.
    let _: serde_json::Value = command(
        registry,
        Op::RetireName {
            name: r.name.clone(),
            generation: r.generation.clone(),
        },
    )
    .await?;
    Ok(())
}

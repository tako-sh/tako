//! Locate the elected leader and reserve immutable resource generations.
use super::target::Home;
use crate::network::databases::DatabaseRecord;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use futures_util::{StreamExt, future::join_all, stream::FuturesUnordered};
use std::collections::BTreeSet;
use std::time::Duration;
use tako_core::{Command, StorageClaim, StorageOperation as Op};
use tokio::time::Instant;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(crate) async fn command<T: serde::de::DeserializeOwned>(home: &Home, request: Op) -> Result<T> {
    crate::commands::db::send(home, Command::CoordinateStorage { request }).await
}

pub(crate) async fn leader(homes: &[Home], group: &str) -> Result<Home> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let mut replies: FuturesUnordered<_> = homes
            .iter()
            .map(|home| async move {
                let reply = command::<serde_json::Value>(
                    home,
                    Op::Progress {
                        group: group.to_owned(),
                    },
                )
                .await;
                (home, reply)
            })
            .collect();
        let probe_deadline = deadline.min(Instant::now() + Duration::from_secs(2));
        while let Ok(Some((home, reply))) =
            tokio::time::timeout_at(probe_deadline, replies.next()).await
        {
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

pub(crate) async fn catalog(homes: &[Home]) -> Result<Home> {
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

pub(crate) fn claim(record: &DatabaseRecord) -> StorageClaim {
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

pub(crate) async fn reserve(
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

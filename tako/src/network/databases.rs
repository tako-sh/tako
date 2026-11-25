//! The network directory publishes catalog-reserved SQL and KV generations.
//! Deploy identities can read the directory. Only administrators can read backup keys.

use serde::{Deserialize, Serialize};
use tako_network::api::{RpcRequest, RpcResponse};
use tako_network::sealed::SealedValue;
use tako_network::shared::{
    DATABASE_SCOPE, RecordAddress, RecordContent, RecordKind, RecordMutation,
};
use tako_network::{AppEnv, LocalDeviceKeys, RequestId};
use zeroize::Zeroizing;

use super::Result;
use crate::config::LocalNetwork;

/// How long a publication may be retried before it expires.
const PUBLICATION_SECONDS: u64 = 10 * 60;

/// The server-owned service a generated database was created for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceOwner {
    Channels,
    Workflows,
}

/// Where a database lives.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DatabaseRecord {
    pub name: String,
    /// Initial coordinator. Leadership can move to another member.
    pub server: String,
    pub founders: std::collections::BTreeSet<String>,
    /// Tells this database apart from earlier ones with the same name.
    pub generation: String,
    pub created_at_unix_secs: u64,
    /// SQLite or a KV store. Both share the network's names.
    #[serde(default, skip_serializing_if = "tako_core::DatabaseEngine::is_sqlite")]
    pub engine: tako_core::DatabaseEngine,
    /// Set only when deploy creates an implicit replay database for this app/env.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_owner: Option<String>,
    /// Set only when deploy creates an implicit workflow database for this app/env.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflows_owner: Option<String>,
}

fn scope() -> Result<AppEnv> {
    Ok(DATABASE_SCOPE.parse()?)
}

/// Decrypts the readable records of `kind`, by name.
fn read(
    alias: &LocalNetwork,
    kind: RecordKind,
    cached: bool,
) -> Result<std::collections::BTreeMap<String, Zeroizing<String>>> {
    let scope = scope()?;
    let (profile, cache) = if cached {
        let Some(cache) = super::state::read_cache(alias)? else {
            return Ok(Default::default());
        };
        (super::load_profile(alias)?, cache)
    } else {
        super::state::pull(alias, Some(scope.clone()))?
    };
    let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
    let device = profile.request().device_id_for(&cache.network)?;
    let mut values = std::collections::BTreeMap::new();
    for record in cache.latest()?.values() {
        if record.address.scope != scope || record.address.kind != kind {
            continue;
        }
        // Only a tombstone is absent. A missing recipient key must never
        // turn an existing database into a candidate for automatic creation.
        let Some(bytes) = record.open(&cache, &keys, &device)? else {
            continue;
        };
        let content: RecordContent = serde_json::from_slice(&bytes)?;
        values.insert(record.address.name.clone(), content.value);
    }
    Ok(values)
}

/// The network's databases, sorted by name.
pub(crate) fn list(alias: &LocalNetwork) -> Result<Vec<DatabaseRecord>> {
    list_from(alias, false)
}

/// Dry runs consult the authenticated cache without contacting a server.
pub(crate) fn list_from(alias: &LocalNetwork, cached: bool) -> Result<Vec<DatabaseRecord>> {
    let values = read(alias, RecordKind::Database, cached)?;
    let mut records = std::collections::BTreeMap::new();
    for (address, value) in &values {
        if let Some(generation) = address.strip_prefix("retired.") {
            tako_core::validate_database_generation(generation)?;
            if value.as_str() != generation {
                return Err("Invalid resource retirement record".into());
            }
            continue;
        }
        let record: DatabaseRecord = serde_json::from_str(value)?;
        tako_core::validate_database_name(&record.name)?;
        tako_core::validate_database_generation(&record.generation)?;
        if record.founders.is_empty() || *address != format!("live.{}", record.generation) {
            return Err(format!(
                "Database directory entry for '{address}' has an invalid generation or founding membership."
            )
            .into());
        }
        if values.contains_key(&format!("retired.{}", record.generation)) {
            continue;
        }
        if records.insert(record.name.clone(), record).is_some() {
            return Err("Database directory changes are still arriving. Retry after the network synchronizes.".into());
        }
    }
    let records = records.into_values().collect();
    Ok(records)
}

/// The key record of a database is named after its name and generation.
fn key_name(record: &DatabaseRecord) -> String {
    format!("{}.{}", record.name, record.generation)
}

/// The key that encrypts a database's snapshots. Administrators only.
pub(crate) fn backup_key(
    alias: &LocalNetwork,
    record: &DatabaseRecord,
) -> Result<Zeroizing<String>> {
    generation_key(alias, &record.name, &record.generation, false)
}

/// A local restore can use a previously synchronized key even after retirement.
pub(crate) fn generation_key(
    alias: &LocalNetwork,
    name: &str,
    generation: &str,
    cached: bool,
) -> Result<Zeroizing<String>> {
    read(alias, RecordKind::DatabaseKey, cached)?
        .remove(&format!("{name}.{generation}"))
        .ok_or_else(|| {
            format!("The recovery key for '{name}' generation {generation} is unavailable. Use a full-access workstation that has synchronized this network.").into()
        })
}

pub(crate) fn retire(alias: &LocalNetwork, record: &DatabaseRecord) -> Result<()> {
    tako_core::validate_database_generation(&record.generation)?;
    mutate(
        alias,
        RecordKind::Database,
        &format!("retired.{}", record.generation),
        Some(&record.generation),
    )
}

/// Publishes a tombstone for one generation's key record.
pub(crate) fn remove_key(alias: &LocalNetwork, record: &DatabaseRecord) -> Result<()> {
    tako_core::validate_database_name(&record.name)?;
    mutate(alias, RecordKind::DatabaseKey, &key_name(record), None)
}

fn mutate(alias: &LocalNetwork, kind: RecordKind, name: &str, value: Option<&str>) -> Result<()> {
    let scope = scope()?;
    let (profile, cache) = super::state::pull(alias, Some(scope.clone()))?;
    let network = profile.snapshot().network.id.clone();
    let issuer = super::client::available_server(&profile)?;
    let members = profile.snapshot().effective_members(super::now()?)?;
    let recipient = &members
        .get(&issuer)
        .ok_or("The publishing server is no longer a network member")?
        .request
        .encryption_public_key;
    let address = RecordAddress {
        scope: scope.clone(),
        kind,
        name: name.to_owned(),
    };
    let observed_revision = cache
        .latest()?
        .get(&address)
        .map_or(0, |record| record.revision);
    let mut mutation = RecordMutation {
        id: RequestId::random()?,
        observed_revision,
        expires_at: super::now()?
            .checked_add(PUBLICATION_SECONDS)
            .ok_or("Publication deadline overflow")?,
        address,
        value: None,
    };
    if let Some(value) = value {
        let content = RecordContent {
            value: Zeroizing::new(value.to_owned()),
            expires_on: None,
        };
        let bytes = Zeroizing::new(serde_json::to_vec(&content)?);
        mutation.value = Some(SealedValue::seal(
            recipient,
            &bytes,
            &mutation.context(&network)?,
        )?);
    }
    let RpcResponse::RecordPublished(receipt) = super::client::call(
        &profile,
        &issuer,
        RpcRequest::PublishRecord(Box::new(mutation.clone())),
    )?
    else {
        return Err("Unexpected database record publication response".into());
    };
    if receipt.id != mutation.id || receipt.address != mutation.address {
        return Err("Database record publication response does not match the request".into());
    }
    // Refresh the local cache so the next read sees the new record.
    super::state::pull(alias, Some(scope))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_reject_unknown_fields() {
        let record = DatabaseRecord {
            name: "main".into(),
            founders: ["a".repeat(64)].into(),
            server: "a".repeat(64),
            generation: "0123456789abcdef0123456789abcdef".into(),
            created_at_unix_secs: 1,
            engine: tako_core::DatabaseEngine::Sqlite,
            channel_owner: None,
            workflows_owner: Some("shop/production".into()),
        };
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(
            serde_json::from_str::<DatabaseRecord>(&json).unwrap(),
            record
        );
        assert!(!json.contains("engine"), "SQLite records keep their shape");
        let kv = DatabaseRecord {
            engine: tako_core::DatabaseEngine::Kv,
            ..record.clone()
        };
        let kv_json = serde_json::to_string(&kv).unwrap();
        assert!(kv_json.contains(r#""engine":"kv""#));
        assert_eq!(
            serde_json::from_str::<DatabaseRecord>(&kv_json).unwrap(),
            kv
        );
        let mut without_founders: serde_json::Value = serde_json::from_str(&json).unwrap();
        without_founders.as_object_mut().unwrap().remove("founders");
        assert!(serde_json::from_value::<DatabaseRecord>(without_founders).is_err());
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["extra"] = true.into();
        assert!(serde_json::from_value::<DatabaseRecord>(value).is_err());
        assert!(
            scope().is_ok(),
            "the database scope is a valid address scope"
        );
        // The longest name and its generation still make a valid address.
        let longest = DatabaseRecord {
            name: "a".repeat(63),
            ..record
        };
        RecordAddress {
            scope: scope().unwrap(),
            kind: RecordKind::DatabaseKey,
            name: key_name(&longest),
        }
        .validate()
        .unwrap();
    }
}

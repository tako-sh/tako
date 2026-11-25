//! The network directory publishes catalog-reserved SQL and KV generations.
//! Deploy identities can read the directory. Only administrators can read backup keys.

use serde::{Deserialize, Serialize};
use tako_network::shared::{DATABASE_SCOPE, RecordContent, RecordKind};
use tako_network::{AppEnv, LocalDeviceKeys};
use zeroize::Zeroizing;

use super::Result;
use crate::config::LocalNetwork;

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
    /// Turso SQL and KV share the network's resource names.
    #[serde(default, skip_serializing_if = "tako_core::DatabaseEngine::is_sql")]
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
        let record: DatabaseRecord = serde_json::from_str(value)?;
        tako_core::validate_database_name(&record.name)?;
        tako_core::validate_database_generation(&record.generation)?;
        if record.founders.is_empty() || *address != format!("live.{}", record.generation) {
            return Err(format!(
                "Database directory entry for '{address}' has an invalid generation or founding membership."
            )
            .into());
        }
        if records.insert(record.name.clone(), record).is_some() {
            return Err("Database directory changes are still arriving. Retry after the network synchronizes.".into());
        }
    }
    let records = records.into_values().collect();
    Ok(records)
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
            engine: tako_core::DatabaseEngine::Turso,
            channel_owner: None,
            workflows_owner: Some("shop/production".into()),
        };
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(
            serde_json::from_str::<DatabaseRecord>(&json).unwrap(),
            record
        );
        assert!(
            !json.contains("engine"),
            "SQL is the default binding engine"
        );
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
        tako_network::shared::RecordAddress {
            scope: scope().unwrap(),
            kind: RecordKind::DatabaseKey,
            name: format!("{}.{}", longest.name, longest.generation),
        }
        .validate()
        .unwrap();
    }
}

//! Database records in the network's shared state.
//!
//! A database's name identifies it within the network. Each database has two
//! records under its name: a directory record with its home server, which
//! every member that may deploy can read, and its snapshot key,
//! which only administrators can read. Deleting a database publishes
//! tombstones for both.

use serde::{Deserialize, Serialize};
use tako_network::api::{RpcRequest, RpcResponse};
use tako_network::sealed::SealedValue;
use tako_network::shared::{
    DATABASE_SCOPE, SecretAddress, SecretContent, SecretKind, SecretMutation,
};
use tako_network::{AppEnv, LocalDeviceKeys, RequestId};
use zeroize::Zeroizing;

use super::Result;
use crate::config::LocalNetwork;

/// How long a publication may be retried before it expires.
const PUBLICATION_SECONDS: u64 = 10 * 60;

/// Where a database lives.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DatabaseRecord {
    pub name: String,
    /// Device ID of the server that hosts the database.
    pub server: String,
    /// Tells this database apart from earlier ones with the same name.
    pub generation: String,
    pub created_at_unix_secs: u64,
}

fn scope() -> Result<AppEnv> {
    Ok(DATABASE_SCOPE.parse()?)
}

/// Decrypts the readable records of `kind`, by name.
fn read(
    alias: &LocalNetwork,
    kind: SecretKind,
) -> Result<std::collections::BTreeMap<String, Zeroizing<String>>> {
    let scope = scope()?;
    let (profile, cache) = super::secrets::pull(alias, Some(scope.clone()))?;
    let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
    let device = profile.request().device_id_for(&cache.network)?;
    let mut values = std::collections::BTreeMap::new();
    for record in cache.latest()?.values() {
        if record.address.scope != scope || record.address.kind != kind {
            continue;
        }
        // Records this device may not read, and tombstones, have no value.
        let Ok(Some(bytes)) = record.decrypt(&cache, &keys, &device) else {
            continue;
        };
        let content: SecretContent = serde_json::from_slice(&bytes)?;
        values.insert(record.address.name.clone(), content.value);
    }
    Ok(values)
}

/// The network's databases, sorted by name.
pub(crate) fn list(alias: &LocalNetwork) -> Result<Vec<DatabaseRecord>> {
    let mut records = Vec::new();
    for (name, value) in read(alias, SecretKind::Database)? {
        let record: DatabaseRecord = serde_json::from_str(&value)?;
        if record.name == name {
            records.push(record);
        }
    }
    Ok(records)
}

/// The key that encrypts a database's snapshots. Administrators only.
pub(crate) fn backup_key(alias: &LocalNetwork, name: &str) -> Result<Zeroizing<String>> {
    read(alias, SecretKind::DatabaseKey)?
        .remove(name)
        .ok_or_else(|| {
            format!(
                "The snapshot key of database '{name}' is not available to this device; it needs network and server administrator permission."
            )
            .into()
        })
}

/// Publishes a database's records: the directory record and, when given,
/// its snapshot key.
pub(crate) fn publish(
    alias: &LocalNetwork,
    record: &DatabaseRecord,
    backup_key_base64: Option<&str>,
) -> Result<()> {
    tako_core::validate_database_name(&record.name)?;
    if let Some(key) = backup_key_base64 {
        mutate(alias, SecretKind::DatabaseKey, &record.name, Some(key))?;
    }
    mutate(
        alias,
        SecretKind::Database,
        &record.name,
        Some(&serde_json::to_string(record)?),
    )
}

/// Publishes tombstones for a database's records.
pub(crate) fn remove(alias: &LocalNetwork, name: &str) -> Result<()> {
    tako_core::validate_database_name(name)?;
    mutate(alias, SecretKind::Database, name, None)?;
    mutate(alias, SecretKind::DatabaseKey, name, None)
}

fn mutate(alias: &LocalNetwork, kind: SecretKind, name: &str, value: Option<&str>) -> Result<()> {
    let scope = scope()?;
    let (profile, cache) = super::secrets::pull(alias, Some(scope.clone()))?;
    let network = profile.snapshot().network.id.clone();
    let issuer = super::client::available_server(&profile)?;
    let members = profile.snapshot().effective_members(super::now()?)?;
    let recipient = &members
        .get(&issuer)
        .ok_or("The publishing server is no longer a network member")?
        .request
        .encryption_public_key;
    let address = SecretAddress {
        scope: scope.clone(),
        kind,
        name: name.to_owned(),
    };
    let observed_revision = cache
        .latest()?
        .get(&address)
        .map_or(0, |record| record.revision);
    let mut mutation = SecretMutation {
        id: RequestId::random()?,
        observed_revision,
        expires_at: super::now()?
            .checked_add(PUBLICATION_SECONDS)
            .ok_or("Publication deadline overflow")?,
        address,
        value: None,
    };
    if let Some(value) = value {
        let content = SecretContent {
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
    let RpcResponse::SecretPublished(receipt) = super::client::call(
        &profile,
        &issuer,
        RpcRequest::PublishSecret(Box::new(mutation.clone())),
    )?
    else {
        return Err("Unexpected database record publication response".into());
    };
    if receipt.id != mutation.id || receipt.address != mutation.address {
        return Err("Database record publication response does not match the request".into());
    }
    // Refresh the local cache so the next read sees the new record.
    super::secrets::pull(alias, Some(scope))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_reject_unknown_fields() {
        let record = DatabaseRecord {
            name: "main".into(),
            server: "a".repeat(64),
            generation: "0123456789abcdef0123456789abcdef".into(),
            created_at_unix_secs: 1,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(
            serde_json::from_str::<DatabaseRecord>(&json).unwrap(),
            record
        );
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["extra"] = true.into();
        assert!(serde_json::from_value::<DatabaseRecord>(value).is_err());
        assert!(
            scope().is_ok(),
            "the database scope is a valid address scope"
        );
    }
}

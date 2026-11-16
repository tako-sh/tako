//! Database records in the network's shared state.
//!
//! Each Tako-hosted database has one record, named by its ID, holding its
//! name, home server, and the key that encrypts its snapshots. Records are
//! sealed to network administrators and replicate through the network like
//! shared secrets. Deleting a database publishes a tombstone.

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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DatabaseRecord {
    pub id: String,
    pub name: String,
    /// Device ID of the server that hosts the database.
    pub server: String,
    #[serde(default)]
    pub access: tako_core::DatabaseAccess,
    /// 32-byte key, base64, that encrypts the database's snapshots.
    pub backup_key_base64: String,
    pub created_at_unix_secs: u64,
}

fn scope() -> Result<AppEnv> {
    Ok(DATABASE_SCOPE.parse()?)
}

/// The network's databases that this device may read, sorted by name.
pub(crate) fn list(alias: &LocalNetwork) -> Result<Vec<DatabaseRecord>> {
    let scope = scope()?;
    let (profile, cache) = super::secrets::pull(alias, Some(scope.clone()))?;
    let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
    let device = profile.request().device_id_for(&cache.network)?;
    let mut records = Vec::new();
    for record in cache.latest()?.values() {
        if record.address.scope != scope || record.address.kind != SecretKind::Database {
            continue;
        }
        let Some(bytes) = record.decrypt(&cache, &keys, &device)? else {
            continue;
        };
        let content: SecretContent = serde_json::from_slice(&bytes)?;
        let database: DatabaseRecord = serde_json::from_str(&content.value)?;
        if database.id == record.address.name {
            records.push(database);
        }
    }
    records.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    Ok(records)
}

/// Publishes a database record, or its tombstone when `record` is `None`.
pub(crate) fn publish(
    alias: &LocalNetwork,
    id: &str,
    record: Option<&DatabaseRecord>,
) -> Result<()> {
    tako_core::validate_database_id(id)?;
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
        kind: SecretKind::Database,
        name: id.to_owned(),
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
    if let Some(record) = record {
        let content = SecretContent {
            value: Zeroizing::new(serde_json::to_string(record)?),
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
            id: "0123456789abcdef0123456789abcdef".into(),
            name: "main".into(),
            server: "a".repeat(64),
            access: tako_core::DatabaseAccess::Network,
            backup_key_base64: "a2V5".into(),
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

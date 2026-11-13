//! Versioned encrypted values. Membership remains in its own immutable registry.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    AppEnv, DeviceId, LocalDeviceKeys, NetworkId, Permissions, RegistrySnapshot, RequestId,
    api::{RuntimeError, RuntimeResult},
    sealed::{Ciphertext, SealedValue, random_content_key},
};

mod store;
#[cfg(test)]
mod tests;

pub const MAX_SHARED_RECORDS: usize = 10_000;
pub const MAX_SHARED_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SHARED_RECORD_BYTES: usize = 128 * 1024;

pub const SHARED_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS tako_shared_records (\n\
    kind TEXT NOT NULL, record_id TEXT NOT NULL, digest TEXT NOT NULL, payload TEXT NOT NULL,\n\
    PRIMARY KEY (kind, record_id, digest)\n\
);";

/// Plaintext exists only inside authorized clients and server agents. It has
/// no Debug implementation and is never a public RPC field.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretContent {
    pub value: Zeroizing<String>,
    pub expires_on: Option<String>,
}

impl SecretContent {
    pub fn validate(&self) -> RuntimeResult<()> {
        if self.value.len() > crate::sealed::MAX_SECRET_BYTES - 256
            || self.expires_on.as_ref().is_some_and(|date| {
                date.len() != 10
                    || date.as_bytes()[4] != b'-'
                    || date.as_bytes()[7] != b'-'
                    || !date
                        .bytes()
                        .enumerate()
                        .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
            })
        {
            return Err(RuntimeError::Invalid("secret content"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    App,
    Credential,
    Storage,
    BackupKey,
    BackupActive,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretAddress {
    pub scope: AppEnv,
    pub kind: SecretKind,
    pub name: String,
}

impl SecretAddress {
    pub fn validate(&self) -> RuntimeResult<()> {
        if self.name.is_empty()
            || self.name.len() > 128
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(RuntimeError::Invalid("secret name"));
        }
        Ok(())
    }

    pub fn allowed(&self, permissions: &Permissions) -> bool {
        (permissions.network_admin && permissions.server_admin)
            || (self.kind == SecretKind::App && permissions.can_deploy(&self.scope))
    }

    pub fn context(&self, network: &NetworkId, operation: &RequestId) -> RuntimeResult<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(&(
            "tako-shared-secret-v1",
            network,
            self,
            operation,
        ))?)
    }
}

/// No plaintext is accepted by the public RPC. The ingress server decrypts the
/// proposed value and publishes one canonical ciphertext for its recipients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretMutation {
    pub id: RequestId,
    pub address: SecretAddress,
    pub value: Option<SealedValue>,
}

/// Server-asserted record. Revision order is logical, with deterministic ties.
/// A retry retains this exact record, including its ciphertext and revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRecord {
    pub id: RequestId,
    pub address: SecretAddress,
    pub revision: u64,
    pub issuer: DeviceId,
    pub author: DeviceId,
    pub value: Option<Ciphertext>,
    pub keys: BTreeMap<DeviceId, SealedValue>,
}

/// Additional recipients do not change the value or its revision. Each issuer
/// publishes at most one immutable envelope for a record/recipient pair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretGrant {
    pub record: RequestId,
    pub recipient: DeviceId,
    pub issuer: DeviceId,
    pub key: SealedValue,
}

impl SecretGrant {
    pub fn id(&self) -> String {
        format!("{}:{}:{}", self.record, self.recipient, self.issuer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedSnapshot {
    pub network: NetworkId,
    pub secrets: Vec<SecretRecord>,
    pub grants: Vec<SecretGrant>,
}

impl SharedSnapshot {
    pub fn empty(network: NetworkId) -> Self {
        Self {
            network,
            secrets: Vec::new(),
            grants: Vec::new(),
        }
    }

    pub fn validate(&self) -> RuntimeResult<()> {
        if self.secrets.len() + self.grants.len() > MAX_SHARED_RECORDS
            || serde_json::to_vec(self)?.len() > MAX_SHARED_BYTES
        {
            return Err(RuntimeError::Capacity("shared secret state"));
        }
        let mut records = BTreeMap::new();
        for record in &self.secrets {
            record.validate()?;
            if records
                .insert(&record.id, record)
                .is_some_and(|old| old != record)
            {
                return Err(RuntimeError::Conflict("secret operation"));
            }
        }
        let mut grants = BTreeMap::new();
        for grant in &self.grants {
            grant.key.ciphertext.validate()?;
            if serde_json::to_vec(grant)?.len() > MAX_SHARED_RECORD_BYTES {
                return Err(RuntimeError::Capacity("secret grant"));
            }
            if grants
                .insert(grant.id(), grant)
                .is_some_and(|old| old != grant)
            {
                return Err(RuntimeError::Conflict("secret grant"));
            }
            // Grants and records can arrive in either order during replication.
        }
        Ok(())
    }

    pub fn merge(&mut self, other: &Self) -> RuntimeResult<()> {
        if self.network != other.network {
            return Err(RuntimeError::Conflict("shared state network"));
        }
        let mut merged = self.clone();
        for record in &other.secrets {
            if !merged.secrets.contains(record) {
                merged.secrets.push(record.clone());
            }
        }
        for grant in &other.grants {
            if !merged.grants.contains(grant) {
                merged.grants.push(grant.clone());
            }
        }
        merged.validate()?;
        *self = merged;
        Ok(())
    }

    pub fn latest(&self) -> RuntimeResult<BTreeMap<SecretAddress, &SecretRecord>> {
        self.validate()?;
        let mut latest: BTreeMap<SecretAddress, &SecretRecord> = BTreeMap::new();
        for record in &self.secrets {
            let previous = latest.entry(record.address.clone()).or_insert(record);
            if record.order() > previous.order() {
                *previous = record;
            }
        }
        Ok(latest)
    }

    pub fn for_permissions(&self, permissions: &Permissions) -> RuntimeResult<Self> {
        self.validate()?;
        let secrets: Vec<_> = self
            .secrets
            .iter()
            .filter(|record| record.address.allowed(permissions))
            .cloned()
            .collect();
        let ids: BTreeSet<_> = secrets.iter().map(|record| &record.id).collect();
        let grants = self
            .grants
            .iter()
            .filter(|grant| ids.contains(&grant.record))
            .cloned()
            .collect();
        Ok(Self {
            network: self.network.clone(),
            secrets,
            grants,
        })
    }
}

impl SecretRecord {
    pub fn order(&self) -> (u64, &DeviceId, &RequestId) {
        (self.revision, &self.issuer, &self.id)
    }

    pub fn validate(&self) -> RuntimeResult<()> {
        self.address.validate()?;
        if self.revision == 0 || self.revision > i64::MAX as u64 {
            return Err(RuntimeError::Invalid("secret revision"));
        }
        if serde_json::to_vec(self)?.len() > MAX_SHARED_RECORD_BYTES {
            return Err(RuntimeError::Capacity("secret record"));
        }
        match &self.value {
            None if !self.keys.is_empty() => {
                return Err(RuntimeError::Invalid("deletion key grants"));
            }
            Some(value) => {
                value.validate()?;
                if !self.keys.contains_key(&self.issuer) {
                    return Err(RuntimeError::Invalid("missing issuer secret key"));
                }
                for key in self.keys.values() {
                    key.ciphertext.validate()?;
                }
            }
            None => (),
        }
        Ok(())
    }

    fn key_context(&self, network: &NetworkId, recipient: &DeviceId) -> RuntimeResult<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            "tako-shared-secret-key-v1",
            network,
            &self.address,
            &self.id,
            recipient,
            hex::encode(openssl::sha::sha256(&serde_json::to_vec(&self.value)?)),
        ))?)
    }

    pub fn encrypt(
        mutation: &SecretMutation,
        plaintext: Option<&[u8]>,
        revision: u64,
        issuer: DeviceId,
        author: DeviceId,
        registry: &RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<Self> {
        let key = random_content_key()?;
        let context = mutation
            .address
            .context(&registry.network.id, &mutation.id)?;
        let mut record = Self {
            id: mutation.id.clone(),
            address: mutation.address.clone(),
            revision,
            issuer,
            author,
            value: plaintext
                .map(|bytes| Ciphertext::encrypt(&key, bytes, &context))
                .transpose()?,
            keys: BTreeMap::new(),
        };
        if record.value.is_some() {
            for (id, member) in registry.effective_members(now)? {
                if record.address.allowed(&member.permissions) {
                    let sealed = SealedValue::seal(
                        &member.request.encryption_public_key,
                        key.as_slice(),
                        &record.key_context(&registry.network.id, &id)?,
                    )?;
                    record.keys.insert(id, sealed);
                }
            }
        }
        record.validate()?;
        Ok(record)
    }

    pub fn content_key(
        &self,
        state: &SharedSnapshot,
        keys: &LocalDeviceKeys,
        device: &DeviceId,
    ) -> RuntimeResult<Zeroizing<[u8; 32]>> {
        if self.value.is_none() {
            return Err(RuntimeError::Invalid("deleted secret"));
        }
        let context = self.key_context(&state.network, device)?;
        let candidates = self.keys.get(device).into_iter().chain(
            state
                .grants
                .iter()
                .filter(|grant| grant.record == self.id && &grant.recipient == device)
                .map(|grant| &grant.key),
        );
        for candidate in candidates {
            if let Ok(bytes) = candidate.open(keys, &context)
                && bytes.len() == 32
            {
                let mut key = Zeroizing::new([0; 32]);
                key.copy_from_slice(&bytes);
                return Ok(key);
            }
        }
        Err(RuntimeError::Invalid(
            "secret decryption key unavailable; synchronize through an existing authorized server",
        ))
    }

    pub fn decrypt(
        &self,
        state: &SharedSnapshot,
        keys: &LocalDeviceKeys,
        device: &DeviceId,
    ) -> RuntimeResult<Option<Zeroizing<Vec<u8>>>> {
        let Some(value) = &self.value else {
            return Ok(None);
        };
        let key = self.content_key(state, keys, device)?;
        Ok(Some(value.decrypt(
            &key,
            &self.address.context(&state.network, &self.id)?,
        )?))
    }

    pub fn grant(
        &self,
        state: &SharedSnapshot,
        keys: &LocalDeviceKeys,
        issuer: &DeviceId,
        recipient: &crate::Member,
    ) -> RuntimeResult<SecretGrant> {
        if !self.address.allowed(&recipient.permissions) {
            return Err(RuntimeError::Denied);
        }
        let key = self.content_key(state, keys, issuer)?;
        Ok(SecretGrant {
            record: self.id.clone(),
            recipient: recipient.device_id.clone(),
            issuer: issuer.clone(),
            key: SealedValue::seal(
                &recipient.request.encryption_public_key,
                key.as_slice(),
                &self.key_context(&state.network, &recipient.device_id)?,
            )?,
        })
    }
}

pub trait SharedStore {
    fn shared_snapshot(&mut self) -> RuntimeResult<SharedSnapshot>;
    fn append_secret(&mut self, record: &SecretRecord) -> RuntimeResult<()>;
    fn append_secret_grant(&mut self, grant: &SecretGrant) -> RuntimeResult<()>;
}

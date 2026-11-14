//! Versioned encrypted values. Membership remains in its own immutable registry.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    AppEnv, DeviceId, LocalDeviceKeys, NetworkId, Permissions, RegistrySnapshot, RequestId,
    api::{RuntimeError, RuntimeResult},
    sealed::{Ciphertext, SealedValue, random_content_key},
};

#[path = "shared/store.rs"]
mod store;
#[cfg(test)]
#[path = "shared/tests.rs"]
mod tests;

pub const MAX_SHARED_RECORDS: usize = 10_000;
/// Published commands can be retried for seven days, independently of value retention.
pub const SECRET_RETRY_SECONDS: u64 = 7 * 86400;
pub const MAX_SHARED_BYTES: usize = 8 * 1024 * 1024;
// A current value now includes recipient keys that used to occupy separate
// grant records. Allow those keys to use the existing shared-state budget;
// ciphertext sizes remain independently bounded by Ciphertext::validate.
pub const MAX_SHARED_RECORD_BYTES: usize = MAX_SHARED_BYTES;

pub const SHARED_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS tako_shared_records (\n\
    kind TEXT NOT NULL, record_id TEXT NOT NULL, digest TEXT NOT NULL, payload TEXT NOT NULL DEFAULT '',\n\
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
    /// Highest published revision this writer has observed for the address.
    pub observed_revision: u64,
    /// A fixed deadline; retrying must not extend the operation's lifetime.
    pub expires_at: u64,
    pub address: SecretAddress,
    pub value: Option<SealedValue>,
}

impl SecretMutation {
    pub fn context(&self, network: &NetworkId) -> RuntimeResult<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            "tako-secret-command-v2",
            network,
            &self.address,
            &self.id,
            self.observed_revision,
            self.expires_at,
        ))?)
    }
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

/// Publication acknowledgement without a historical ciphertext.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretReceipt {
    pub id: RequestId,
    pub address: SecretAddress,
    pub revision: u64,
    pub issuer: DeviceId,
    pub author: DeviceId,
}
impl SecretReceipt {
    pub fn order(&self) -> (u64, &DeviceId, &RequestId) {
        (self.revision, &self.issuer, &self.id)
    }
}
impl From<&SecretRecord> for SecretReceipt {
    fn from(record: &SecretRecord) -> Self {
        Self {
            id: record.id.clone(),
            address: record.address.clone(),
            revision: record.revision,
            issuer: record.issuer.clone(),
            author: record.author.clone(),
        }
    }
}

/// Only used to migrate persisted state written before recipient keys were
/// included in current records. It is not an RPC or new-write format.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacySecretGrant {
    pub record: RequestId,
    pub recipient: DeviceId,
    pub issuer: DeviceId,
    pub key: SealedValue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedSnapshot {
    pub network: NetworkId,
    pub secrets: Vec<SecretRecord>,
}

impl SharedSnapshot {
    pub fn empty(network: NetworkId) -> Self {
        Self {
            network,
            secrets: Vec::new(),
        }
    }

    pub fn validate(&self) -> RuntimeResult<()> {
        if self.secrets.len() > MAX_SHARED_RECORDS
            || serde_json::to_vec(self)?.len() > MAX_SHARED_BYTES
        {
            return Err(RuntimeError::Capacity("shared secret state"));
        }
        let mut ids = BTreeMap::new();
        let mut addresses = BTreeMap::new();
        for record in &self.secrets {
            record.validate()?;
            if ids.insert(&record.id, record).is_some()
                || addresses.insert(&record.address, record).is_some()
            {
                return Err(RuntimeError::Conflict("duplicate current secret"));
            }
        }
        Ok(())
    }

    pub fn merge(&mut self, other: &Self) -> RuntimeResult<()> {
        if self.network != other.network {
            return Err(RuntimeError::Conflict("shared state network"));
        }
        let mut current = BTreeMap::<SecretAddress, SecretRecord>::new();
        let mut ids = BTreeMap::new();
        for record in self.secrets.iter().chain(&other.secrets) {
            record.validate()?;
            if let Some(previous) = ids.insert(&record.id, record)
                && !record.same_value(previous)
            {
                return Err(RuntimeError::Conflict("secret operation"));
            }
            match current.get_mut(&record.address) {
                Some(previous) if previous.id == record.id => previous.merge_keys(record)?,
                Some(previous) if previous.order() >= record.order() => (),
                _ => {
                    current.insert(record.address.clone(), record.clone());
                }
            }
        }
        let merged = Self {
            network: self.network.clone(),
            secrets: current.into_values().collect(),
        };
        merged.validate()?;
        *self = merged;
        Ok(())
    }

    /// Upgrade a local cache without supporting the old wire protocol.
    pub fn from_persisted(bytes: &[u8]) -> RuntimeResult<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Persisted {
            network: NetworkId,
            secrets: Vec<SecretRecord>,
            #[serde(default)]
            grants: Vec<LegacySecretGrant>,
        }
        let old: Persisted = serde_json::from_slice(bytes)?;
        let mut state = Self::empty(old.network.clone());
        state.merge(&Self {
            network: old.network,
            secrets: old.secrets,
        })?;
        for grant in old.grants {
            state.import_grant(&grant)?;
        }
        state.validate()?;
        Ok(state)
    }

    pub(crate) fn import_grant(&mut self, grant: &LegacySecretGrant) -> RuntimeResult<bool> {
        if let Some(record) = self
            .secrets
            .iter_mut()
            .find(|record| record.id == grant.record)
        {
            record.merge_key(&grant.recipient, &grant.key)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn latest(&self) -> RuntimeResult<BTreeMap<SecretAddress, &SecretRecord>> {
        self.validate()?;
        Ok(self
            .secrets
            .iter()
            .map(|record| (record.address.clone(), record))
            .collect())
    }

    pub fn for_permissions(&self, permissions: &Permissions) -> RuntimeResult<Self> {
        self.validate()?;
        Ok(Self {
            network: self.network.clone(),
            secrets: self
                .secrets
                .iter()
                .filter(|record| record.address.allowed(permissions))
                .cloned()
                .collect(),
        })
    }
}

impl SecretRecord {
    pub fn same_value(&self, other: &Self) -> bool {
        self.id == other.id
            && self.address == other.address
            && self.revision == other.revision
            && self.issuer == other.issuer
            && self.author == other.author
            && self.value == other.value
    }

    fn merge_keys(&mut self, other: &Self) -> RuntimeResult<()> {
        if !self.same_value(other) {
            return Err(RuntimeError::Conflict("secret operation"));
        }
        for (recipient, key) in &other.keys {
            self.merge_key(recipient, key)?;
        }
        Ok(())
    }

    fn merge_key(&mut self, recipient: &DeviceId, key: &SealedValue) -> RuntimeResult<()> {
        if self.value.is_none() {
            return Err(RuntimeError::Invalid("deletion key grants"));
        }
        key.ciphertext.validate()?;
        // Different servers can wrap the same content key concurrently. Select
        // one envelope deterministically; either decrypts to that same key.
        let encoded = serde_json::to_vec(key)?;
        if self
            .keys
            .get(recipient)
            .map(serde_json::to_vec)
            .transpose()?
            .is_none_or(|old| encoded > old)
        {
            self.keys.insert(recipient.clone(), key.clone());
        }
        Ok(())
    }

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
        let candidates = self.keys.get(device).into_iter();
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
    ) -> RuntimeResult<Self> {
        if !self.address.allowed(&recipient.permissions) {
            return Err(RuntimeError::Denied);
        }
        let key = self.content_key(state, keys, issuer)?;
        let wrapped = SealedValue::seal(
            &recipient.request.encryption_public_key,
            key.as_slice(),
            &self.key_context(&state.network, &recipient.device_id)?,
        )?;
        let mut record = self.clone();
        record.merge_key(&recipient.device_id, &wrapped)?;
        record.validate()?;
        Ok(record)
    }
}

pub trait SharedStore {
    fn shared_snapshot(&mut self) -> RuntimeResult<SharedSnapshot>;
    fn append_secret(&mut self, record: &SecretRecord) -> RuntimeResult<()>;
}

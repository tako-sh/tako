//! Versioned shared records: sealed values for secrets and plaintext for
//! non-secret kinds. Membership remains in its own immutable registry.
use sha2::{Digest, Sha256};
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
pub const PUBLICATION_RETRY_SECONDS: u64 = 7 * 86400;
pub const MAX_SHARED_BYTES: usize = 8 * 1024 * 1024;
// A current value now includes recipient keys that used to occupy separate
// grant records. Allow those keys to use the existing shared-state budget;
// ciphertext sizes remain independently bounded by Ciphertext::validate.
pub const MAX_SHARED_RECORD_BYTES: usize = MAX_SHARED_BYTES;

pub const SHARED_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS tako_shared_records (\n\
    kind TEXT NOT NULL, record_id TEXT NOT NULL, digest TEXT NOT NULL, payload TEXT NOT NULL DEFAULT '',\n\
    PRIMARY KEY (kind, record_id, digest)\n\
);";

/// A record's content. The content of a secret exists in the clear only inside
/// authorized clients and server agents. It has no Debug implementation and is
/// never a public RPC field.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordContent {
    pub value: Zeroizing<String>,
    pub expires_on: Option<String>,
}

impl RecordContent {
    pub fn validate(&self) -> RuntimeResult<()> {
        validate_content(&self.value, self.expires_on.as_deref())
    }
}

/// One opened value of an app environment, as a server reads it for a deploy.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeValue {
    pub kind: RecordKind,
    pub name: String,
    pub content: RecordContent,
}

impl std::fmt::Debug for ScopeValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeValue")
            .field("kind", &self.kind)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// An app environment's current values on one server: what a deploy applies.
/// Only the local server process receives it, never a client.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeSecrets {
    /// Hash over the scope's current record versions, deletions included.
    /// Servers that agree on it hold the same values.
    pub digest: String,
    /// Whether every requested published version has arrived here.
    pub caught_up: bool,
    /// Names of values this server cannot open yet because their key grant
    /// has not arrived.
    pub missing_keys: Vec<String>,
    pub values: Vec<ScopeValue>,
}

/// A record version a client published in a scope, which a server waits for
/// before pinning the scope's snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedVersion {
    pub kind: RecordKind,
    pub name: String,
    pub revision: u64,
}

impl SharedSnapshot {
    /// The digest of `scope`'s current record versions, deletions included.
    pub fn scope_digest(&self, scope: &AppEnv) -> RuntimeResult<String> {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        for (address, record) in self.latest()? {
            if &address.scope != scope {
                continue;
            }
            hash.update(serde_json::to_vec(&(
                &address.kind,
                &address.name,
                &record.id,
            ))?);
            hash.update(b"\n");
        }
        Ok(hex::encode(hash.finalize()))
    }
}

/// The stored content of a record whose kind is not secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlainContent {
    pub value: String,
    pub expires_on: Option<String>,
}

impl PlainContent {
    pub fn validate(&self) -> RuntimeResult<()> {
        validate_content(&self.value, self.expires_on.as_deref())
    }
}

fn validate_content(value: &str, expires_on: Option<&str>) -> RuntimeResult<()> {
    if value.len() > crate::sealed::MAX_SECRET_BYTES - 256
        || expires_on.is_some_and(|date| {
            date.len() != 10
                || date.as_bytes()[4] != b'-'
                || date.as_bytes()[7] != b'-'
                || !date
                    .bytes()
                    .enumerate()
                    .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
        })
    {
        return Err(RuntimeError::Invalid("record content"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    App,
    Credential,
    Storage,
    BackupKey,
    /// The ID of the environment's active backup key. Not secret.
    BackupActive,
    /// A Tako-hosted database of the network, named by its name under
    /// [`DATABASE_SCOPE`]: its home server and generation. Not secret; every
    /// member that may deploy can read it, so deploys resolve databases by
    /// name.
    Database,
    /// The key that encrypts a database's snapshots, named
    /// `<name>.<generation>` after its [`RecordKind::Database`] record. Only
    /// administrators may read it.
    DatabaseKey,
}

impl RecordKind {
    /// Secret records are sealed to each reader. Other records are stored as
    /// plaintext, so they need no per-reader keys and every server can read them.
    pub fn is_secret(self) -> bool {
        !matches!(self, Self::BackupActive | Self::Database)
    }
}

/// The scope of database records. Not an app environment: database records
/// belong to the network, and only administrators may write them.
pub const DATABASE_SCOPE: &str = "tako/databases";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordAddress {
    pub scope: AppEnv,
    pub kind: RecordKind,
    pub name: String,
}

impl RecordAddress {
    pub fn validate(&self) -> RuntimeResult<()> {
        if self.name.is_empty()
            || self.name.len() > 128
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(RuntimeError::Invalid("record name"));
        }
        Ok(())
    }

    /// Whether members with `permissions` may publish to this address.
    pub fn writable(&self, permissions: &Permissions) -> bool {
        permissions.is_full()
            || (self.kind == RecordKind::App && permissions.can_deploy(&self.scope))
    }

    /// Whether members with `permissions` may read this address. Database
    /// records hold no secrets, only where a database lives, and deploys of
    /// any app need them.
    pub fn readable(&self, permissions: &Permissions) -> bool {
        self.writable(permissions)
            || (self.kind == RecordKind::Database
                && self.scope.as_str() == DATABASE_SCOPE
                && permissions.deploys())
    }

    // Context labels are bound into stored ciphertexts, so they keep the names
    // they were created with.
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

/// No plaintext is accepted by the public RPC. The ingress server opens the
/// proposed value and publishes one canonical record: a ciphertext for its
/// recipients, or plaintext for a kind that is not secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordMutation {
    pub id: RequestId,
    /// Highest published revision this writer has observed for the address.
    pub observed_revision: u64,
    /// A fixed deadline; retrying must not extend the operation's lifetime.
    pub expires_at: u64,
    pub address: RecordAddress,
    pub value: Option<SealedValue>,
}

impl RecordMutation {
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
pub struct SharedRecord {
    pub id: RequestId,
    pub address: RecordAddress,
    pub revision: u64,
    pub issuer: DeviceId,
    pub author: DeviceId,
    /// The sealed value of a secret record.
    pub value: Option<Ciphertext>,
    pub keys: BTreeMap<DeviceId, SealedValue>,
    /// The value of a record whose kind is not secret. A record with neither
    /// value is a deletion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plaintext: Option<PlainContent>,
}

/// Publication acknowledgement without a historical ciphertext.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordReceipt {
    pub id: RequestId,
    pub address: RecordAddress,
    pub revision: u64,
    pub issuer: DeviceId,
    pub author: DeviceId,
}
impl RecordReceipt {
    pub fn order(&self) -> (u64, &DeviceId, &RequestId) {
        (self.revision, &self.issuer, &self.id)
    }
}
impl From<&SharedRecord> for RecordReceipt {
    fn from(record: &SharedRecord) -> Self {
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
pub(crate) struct LegacyKeyGrant {
    pub record: RequestId,
    pub recipient: DeviceId,
    pub issuer: DeviceId,
    pub key: SealedValue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedSnapshot {
    pub network: NetworkId,
    pub records: Vec<SharedRecord>,
}

impl SharedSnapshot {
    pub fn empty(network: NetworkId) -> Self {
        Self {
            network,
            records: Vec::new(),
        }
    }

    pub fn validate(&self) -> RuntimeResult<()> {
        if self.records.len() > MAX_SHARED_RECORDS
            || serde_json::to_vec(self)?.len() > MAX_SHARED_BYTES
        {
            return Err(RuntimeError::Capacity("shared state"));
        }
        let mut ids = BTreeMap::new();
        let mut addresses = BTreeMap::new();
        for record in &self.records {
            record.validate()?;
            if ids.insert(&record.id, record).is_some()
                || addresses.insert(&record.address, record).is_some()
            {
                return Err(RuntimeError::Conflict("duplicate current record"));
            }
        }
        Ok(())
    }

    pub fn merge(&mut self, other: &Self) -> RuntimeResult<()> {
        if self.network != other.network {
            return Err(RuntimeError::Conflict("shared state network"));
        }
        let mut current = BTreeMap::<RecordAddress, SharedRecord>::new();
        let mut ids = BTreeMap::new();
        for record in self.records.iter().chain(&other.records) {
            record.validate()?;
            if let Some(previous) = ids.insert(&record.id, record)
                && !record.same_value(previous)
            {
                return Err(RuntimeError::Conflict("record operation"));
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
            records: current.into_values().collect(),
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
            #[serde(alias = "secrets")]
            records: Vec<SharedRecord>,
            #[serde(default)]
            grants: Vec<LegacyKeyGrant>,
        }
        let old: Persisted = serde_json::from_slice(bytes)?;
        let mut state = Self::empty(old.network.clone());
        state.merge(&Self {
            network: old.network,
            records: old.records,
        })?;
        for grant in old.grants {
            state.import_grant(&grant)?;
        }
        state.validate()?;
        Ok(state)
    }

    pub(crate) fn import_grant(&mut self, grant: &LegacyKeyGrant) -> RuntimeResult<bool> {
        if let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.id == grant.record)
        {
            record.merge_key(&grant.recipient, &grant.key)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn latest(&self) -> RuntimeResult<BTreeMap<RecordAddress, &SharedRecord>> {
        self.validate()?;
        Ok(self
            .records
            .iter()
            .map(|record| (record.address.clone(), record))
            .collect())
    }

    pub fn for_permissions(&self, permissions: &Permissions) -> RuntimeResult<Self> {
        self.validate()?;
        Ok(Self {
            network: self.network.clone(),
            records: self
                .records
                .iter()
                .filter(|record| record.address.readable(permissions))
                .cloned()
                .collect(),
        })
    }
}

impl SharedRecord {
    pub fn same_value(&self, other: &Self) -> bool {
        self.id == other.id
            && self.address == other.address
            && self.revision == other.revision
            && self.issuer == other.issuer
            && self.author == other.author
            && self.value == other.value
            && self.plaintext == other.plaintext
    }

    /// Whether this record marks its address as deleted.
    pub fn is_deletion(&self) -> bool {
        self.value.is_none() && self.plaintext.is_none()
    }

    fn merge_keys(&mut self, other: &Self) -> RuntimeResult<()> {
        if !self.same_value(other) {
            return Err(RuntimeError::Conflict("record operation"));
        }
        for (recipient, key) in &other.keys {
            self.merge_key(recipient, key)?;
        }
        Ok(())
    }

    fn merge_key(&mut self, recipient: &DeviceId, key: &SealedValue) -> RuntimeResult<()> {
        if self.value.is_none() {
            return Err(RuntimeError::Invalid("keys for an unsealed record"));
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
            return Err(RuntimeError::Invalid("record revision"));
        }
        if serde_json::to_vec(self)?.len() > MAX_SHARED_RECORD_BYTES {
            return Err(RuntimeError::Capacity("shared record"));
        }
        match (&self.value, &self.plaintext) {
            (Some(_), Some(_)) => {
                return Err(RuntimeError::Invalid("sealed and plaintext record value"));
            }
            // Records of a kind that is not secret were sealed before plaintext
            // records existed, so a sealed value stays valid for every kind.
            (Some(value), None) => {
                value.validate()?;
                if !self.keys.contains_key(&self.issuer) {
                    return Err(RuntimeError::Invalid("missing issuer record key"));
                }
                for key in self.keys.values() {
                    key.ciphertext.validate()?;
                }
            }
            (None, Some(plaintext)) => {
                if self.address.kind.is_secret() {
                    return Err(RuntimeError::Invalid("plaintext secret record"));
                }
                if !self.keys.is_empty() {
                    return Err(RuntimeError::Invalid("keys for an unsealed record"));
                }
                plaintext.validate()?;
            }
            (None, None) if !self.keys.is_empty() => {
                return Err(RuntimeError::Invalid("keys for an unsealed record"));
            }
            (None, None) => (),
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
            hex::encode(Sha256::digest(&serde_json::to_vec(&self.value)?)),
        ))?)
    }

    /// Builds the record a mutation publishes. `content` is a serialized
    /// [`RecordContent`], or `None` for a deletion. Secret content is sealed to
    /// every member allowed to read it; other content is stored as plaintext.
    pub fn create(
        mutation: &RecordMutation,
        content: Option<&[u8]>,
        revision: u64,
        issuer: DeviceId,
        author: DeviceId,
        registry: &RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<Self> {
        let mut record = Self {
            id: mutation.id.clone(),
            address: mutation.address.clone(),
            revision,
            issuer,
            author,
            value: None,
            keys: BTreeMap::new(),
            plaintext: None,
        };
        match content {
            None => (),
            Some(bytes) if !record.address.kind.is_secret() => {
                let content: RecordContent = serde_json::from_slice(bytes)?;
                record.plaintext = Some(PlainContent {
                    value: content.value.to_string(),
                    expires_on: content.expires_on,
                });
            }
            Some(bytes) => {
                let key = random_content_key()?;
                let context = record.address.context(&registry.network.id, &record.id)?;
                record.value = Some(Ciphertext::encrypt(&key, bytes, &context)?);
                for (id, member) in registry.effective_members(now)? {
                    if record.address.readable(&member.permissions) {
                        let sealed = SealedValue::seal(
                            &member.request.encryption_public_key,
                            key.as_slice(),
                            &record.key_context(&registry.network.id, &id)?,
                        )?;
                        record.keys.insert(id, sealed);
                    }
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
            return Err(RuntimeError::Invalid("record has no sealed value"));
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
            "record key unavailable; synchronize through an existing authorized server",
        ))
    }

    /// The record's serialized [`RecordContent`], or `None` for a deletion.
    pub fn open(
        &self,
        state: &SharedSnapshot,
        keys: &LocalDeviceKeys,
        device: &DeviceId,
    ) -> RuntimeResult<Option<Zeroizing<Vec<u8>>>> {
        if let Some(plaintext) = &self.plaintext {
            let content = RecordContent {
                value: Zeroizing::new(plaintext.value.clone()),
                expires_on: plaintext.expires_on.clone(),
            };
            return Ok(Some(Zeroizing::new(serde_json::to_vec(&content)?)));
        }
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
        if !self.address.readable(&recipient.permissions) {
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
    fn append_record(&mut self, record: &SharedRecord) -> RuntimeResult<()>;
}

use super::*;
use crate::AppEnv;
use crate::shared::{
    PUBLICATION_RETRY_SECONDS, RecordAddress, RecordMutation, RecordReceipt, SharedRecord,
    SharedSnapshot,
};
use sha2::{Digest, Sha256};

const MAX_PUBLICATION_RECEIPTS: u64 = 100_000;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Publication {
    mutation_hash: String,
    receipt: RecordReceipt,
    // Only an unfinished write retains ciphertext. Once acknowledged, the
    // receipt alone answers retries without reviving an overwritten value.
    pending: Option<SharedRecord>,
}

impl<S: RegistryStore> ServerRuntime<S> {
    /// Open `scope`'s current values with this server's key grants, for the
    /// local server process applying a deploy. Never reachable over RPC.
    pub fn scope_secrets(
        &mut self,
        scope: &AppEnv,
        published: &[crate::shared::PublishedVersion],
    ) -> RuntimeResult<crate::shared::ScopeSecrets> {
        let state = self.shared_snapshot()?;
        let latest = state.latest()?;
        let caught_up = published.iter().all(|version| {
            let address = RecordAddress {
                scope: scope.clone(),
                kind: version.kind,
                name: version.name.clone(),
            };
            latest
                .get(&address)
                .is_some_and(|record| record.revision >= version.revision)
        });
        let mut values = Vec::new();
        let mut missing_keys = Vec::new();
        for (address, record) in &latest {
            if &address.scope != scope {
                continue;
            }
            match record.open(&state, &self.keys, &self.config.server_id) {
                Ok(Some(bytes)) => values.push(crate::shared::ScopeValue {
                    kind: address.kind,
                    name: address.name.clone(),
                    content: serde_json::from_slice(&bytes)?,
                }),
                Ok(None) => {}
                Err(_) => missing_keys.push(address.name.clone()),
            }
        }
        Ok(crate::shared::ScopeSecrets {
            digest: state.scope_digest(scope)?,
            caught_up,
            missing_keys,
            values,
        })
    }

    fn shared_snapshot(&mut self) -> RuntimeResult<SharedSnapshot> {
        let snapshot = self.store.shared_snapshot()?;
        snapshot.validate()?;
        if snapshot.network != self.config.network.id {
            return Err(RuntimeError::Conflict("shared state network"));
        }
        Ok(snapshot)
    }

    pub(super) fn publish_record(
        &mut self,
        mutation: RecordMutation,
        author: &DeviceId,
        registry: &RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        mutation.address.validate()?;
        if !mutation
            .address
            .writable(registry.permissions(author, now)?)
        {
            return Err(RuntimeError::Denied);
        }
        if mutation.expires_at <= now
            || mutation.expires_at
                > now
                    .saturating_add(PUBLICATION_RETRY_SECONDS)
                    .saturating_add(self.config.limits.auth_window_seconds)
        {
            return Err(RuntimeError::Invalid("publication deadline"));
        }
        self.purge_publications(now)?;
        let id = mutation.id.to_string();
        let hash = hex::encode(Sha256::digest(&serde_json::to_vec(&mutation)?));
        let previous = SHARED_PUBLICATIONS.get(&self.db, &id)?;
        let mut publication = if let Some((_, previous)) = previous {
            let saved: Publication = serde_json::from_str(&previous)?;
            if &saved.receipt.author != author || saved.mutation_hash != hash {
                return Err(RuntimeError::Conflict("record operation"));
            }
            saved
        } else {
            let state = self.shared_snapshot()?;
            if state.records.iter().any(|record| record.id == mutation.id) {
                return Err(RuntimeError::Conflict("record operation issuer"));
            }
            let revision = state
                .latest()?
                .get(&mutation.address)
                .map_or(0, |previous| previous.revision)
                .max(mutation.observed_revision)
                .checked_add(1)
                .ok_or(RuntimeError::Capacity("record revision"))?;
            let plaintext = mutation
                .value
                .as_ref()
                .map(|value| {
                    value.open(
                        &self.keys,
                        &mutation.context(&self.config.network.id).map_err(|_| {
                            crate::error::invalid("record command", "invalid context")
                        })?,
                    )
                })
                .transpose()?;
            if let Some(plaintext) = &plaintext {
                let content: crate::shared::RecordContent = serde_json::from_slice(plaintext)?;
                content.validate()?;
            }
            let record = SharedRecord::create(
                &mutation,
                plaintext.as_ref().map(|value| value.as_slice()),
                revision,
                self.config.server_id.clone(),
                author.clone(),
                registry,
                now,
            )?;
            let publication = Publication {
                mutation_hash: hash,
                receipt: (&record).into(),
                pending: Some(record),
            };
            let txn = write(&self.db)?;
            if SHARED_PUBLICATIONS.count(&txn)? >= MAX_PUBLICATION_RECEIPTS {
                return Err(RuntimeError::Capacity("publication receipts"));
            }
            SHARED_PUBLICATIONS.put(
                &txn,
                &id,
                journal_time(mutation.expires_at)?,
                &serde_json::to_string(&publication)?,
            )?;
            txn.commit()?;
            publication
        };
        if let Some(record) = &publication.pending {
            // The durable outbox commit precedes publication. After a lost
            // response, retry uses the original revision and ciphertext.
            self.store.append_record(record)?;
            let state = self.shared_snapshot()?;
            if state
                .latest()?
                .get(&record.address)
                .is_none_or(|current| current.order() < record.order())
            {
                return Err(RuntimeError::Catalog("published record absent"));
            }
            publication.pending = None;
            let txn = write(&self.db)?;
            SHARED_PUBLICATIONS.update(&txn, &id, &serde_json::to_string(&publication)?)?;
            txn.commit()?;
        }
        Ok(RpcResponse::RecordPublished(publication.receipt))
    }

    fn purge_publications(&self, now: u64) -> RuntimeResult<()> {
        let txn = write(&self.db)?;
        SHARED_PUBLICATIONS.purge_through(&txn, journal_time(now)?)?;
        txn.commit()?;
        Ok(())
    }

    pub(super) fn pull_records(
        &mut self,
        signer: &DeviceId,
        scope: Option<&crate::AppEnv>,
        registry: &RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        let effective = registry.effective_members(now)?;
        let member = effective.get(signer).ok_or(RuntimeError::Denied)?;
        // Deployers read the database directory; the filter below leaves out
        // what they may not read, such as snapshot keys.
        let database_directory = |scope: &crate::AppEnv| {
            scope.as_str() == crate::shared::DATABASE_SCOPE && member.permissions.deploys()
        };
        if let Some(scope) = scope
            && !member.permissions.can_deploy(scope)
            && !database_directory(scope)
        {
            return Err(RuntimeError::Denied);
        }
        let state = self
            .shared_snapshot()?
            .for_permissions(&member.permissions)?;
        let mut filtered = state.clone();
        filtered
            .records
            .retain(|record| scope.is_none_or(|scope| &record.address.scope == scope));
        for record in &mut filtered.records {
            // A server that has not received a record's key yet returns the
            // record without one, so one record cannot fail the whole pull.
            // The reader asks another server for the key.
            if record.value.is_some()
                && !record.keys.contains_key(signer)
                && record
                    .content_key(&state, &self.keys, &self.config.server_id)
                    .is_ok()
            {
                let granted = record.grant(&state, &self.keys, &self.config.server_id, member)?;
                self.store.append_record(&granted)?;
                *record = granted;
            }
        }
        filtered.validate()?;
        // Persist newly wrapped keys before acknowledging them.
        self.shared_snapshot()?;
        Ok(RpcResponse::Records(filtered))
    }

    /// Supply current value keys to newly admitted servers. Superseded versions
    /// are never granted, and each successful reconciliation also retires them.
    pub fn reconcile_record_keys(&mut self, now: u64) -> RuntimeResult<()> {
        self.purge_publications(now)?;
        let registry = self.snapshot()?;
        self.require_server(&registry, now)?;
        let state = self.shared_snapshot()?;
        let members = registry.effective_members(now)?;
        let mut published = 0;
        for current in &state.records {
            if current.value.is_none()
                || current
                    .content_key(&state, &self.keys, &self.config.server_id)
                    .is_err()
            {
                continue;
            }
            let mut record = current.clone();
            for member in members.values().filter(|member| {
                matches!(member.kind, MemberKind::Server(_))
                    && current.address.readable(&member.permissions)
                    && !current.keys.contains_key(&member.device_id)
            }) {
                record = record.grant(&state, &self.keys, &self.config.server_id, member)?;
                published += 1;
                if published >= 32 {
                    break;
                }
            }
            if record != *current {
                self.store.append_record(&record)?;
            }
            if published >= 32 {
                break;
            }
        }
        self.shared_snapshot()?;
        Ok(())
    }
}

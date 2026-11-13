use super::*;
use crate::shared::{
    MAX_SHARED_RECORDS, SecretGrant, SecretMutation, SecretRecord, SharedSnapshot,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Publication {
    mutation_hash: String,
    record: SecretRecord,
}

impl<S: RegistryStore> ServerRuntime<S> {
    fn shared_snapshot(&mut self) -> RuntimeResult<SharedSnapshot> {
        let snapshot = self.store.shared_snapshot()?;
        snapshot.validate()?;
        if snapshot.network != self.config.network.id {
            return Err(RuntimeError::Conflict("shared state network"));
        }
        Ok(snapshot)
    }

    pub(super) fn publish_secret(
        &mut self,
        mutation: SecretMutation,
        author: &DeviceId,
        registry: &RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        mutation.address.validate()?;
        if !mutation.address.allowed(registry.permissions(author, now)?) {
            return Err(RuntimeError::Denied);
        }
        let hash = hex::encode(openssl::sha::sha256(&serde_json::to_vec(&mutation)?));
        let previous: Option<String> = self
            .db
            .query_row(
                "SELECT payload FROM shared_commands WHERE id=?1",
                [mutation.id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let publication = if let Some(previous) = previous {
            let saved: Publication = serde_json::from_str(&previous)?;
            if &saved.record.author != author || saved.mutation_hash != hash {
                return Err(RuntimeError::Conflict("secret operation"));
            }
            saved
        } else {
            let state = self.shared_snapshot()?;
            if state.secrets.iter().any(|record| record.id == mutation.id) {
                return Err(RuntimeError::Conflict("secret operation issuer"));
            }
            let revision = state
                .latest()?
                .get(&mutation.address)
                .map_or(Ok(1), |previous| {
                    previous
                        .revision
                        .checked_add(1)
                        .ok_or(RuntimeError::Capacity("secret revision"))
                })?;
            let plaintext = mutation
                .value
                .as_ref()
                .map(|value| {
                    value.open(
                        &self.keys,
                        &mutation
                            .address
                            .context(&self.config.network.id, &mutation.id)
                            .map_err(|_| {
                                crate::error::invalid("secret address", "invalid context")
                            })?,
                    )
                })
                .transpose()?;
            if let Some(plaintext) = &plaintext {
                // Content metadata travels with its encrypted value, never as a
                // separately merged database column.
                let content: crate::shared::SecretContent = serde_json::from_slice(plaintext)?;
                content.validate()?;
            }
            let record = SecretRecord::encrypt(
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
                record,
            };
            let count: i64 =
                self.db
                    .query_row("SELECT count(*) FROM shared_commands", [], |row| row.get(0))?;
            if count >= MAX_SHARED_RECORDS as i64 {
                return Err(RuntimeError::Capacity("secret publication journal"));
            }
            self.db.execute(
                "INSERT INTO shared_commands VALUES (?1, ?2)",
                params![
                    mutation.id.to_string(),
                    serde_json::to_string(&publication)?
                ],
            )?;
            publication
        };
        // FULL synchronous journal commit precedes publication. A response loss
        // retries these exact bytes and never creates a later revision.
        self.store.append_secret(&publication.record)?;
        let state = self.shared_snapshot()?;
        if !state.secrets.contains(&publication.record) {
            return Err(RuntimeError::Corrosion("published secret absent"));
        }
        Ok(RpcResponse::SecretPublished(publication.record))
    }

    fn publish_grant(
        &mut self,
        record: &SecretRecord,
        state: &SharedSnapshot,
        recipient: &Member,
    ) -> RuntimeResult<SecretGrant> {
        let id = format!(
            "{}:{}:{}",
            record.id, recipient.device_id, self.config.server_id
        );
        let previous: Option<String> = self
            .db
            .query_row(
                "SELECT payload FROM shared_grants WHERE id=?1",
                [&id],
                |row| row.get(0),
            )
            .optional()?;
        let grant = if let Some(previous) = previous {
            serde_json::from_str(&previous)?
        } else {
            let grant = record.grant(state, &self.keys, &self.config.server_id, recipient)?;
            let count: i64 =
                self.db
                    .query_row("SELECT count(*) FROM shared_grants", [], |row| row.get(0))?;
            if count >= MAX_SHARED_RECORDS as i64 {
                return Err(RuntimeError::Capacity("secret grant journal"));
            }
            self.db.execute(
                "INSERT INTO shared_grants VALUES (?1, ?2)",
                params![id, serde_json::to_string(&grant)?],
            )?;
            grant
        };
        self.store.append_secret_grant(&grant)?;
        Ok(grant)
    }

    pub(super) fn pull_secrets(
        &mut self,
        signer: &DeviceId,
        scope: Option<&crate::AppEnv>,
        registry: &RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        let effective = registry.effective_members(now)?;
        let member = effective.get(signer).ok_or(RuntimeError::Denied)?;
        if let Some(scope) = scope
            && !(member.permissions.network_admin && member.permissions.server_admin)
            && !member.permissions.can_deploy(scope)
        {
            return Err(RuntimeError::Denied);
        }
        let state = self
            .shared_snapshot()?
            .for_permissions(&member.permissions)?;
        let mut filtered = state.clone();
        filtered
            .secrets
            .retain(|record| scope.is_none_or(|scope| &record.address.scope == scope));
        let ids: std::collections::BTreeSet<_> = filtered
            .secrets
            .iter()
            .map(|record| record.id.clone())
            .collect();
        filtered.grants.retain(|grant| ids.contains(&grant.record));
        for record in &filtered.secrets.clone() {
            if record.value.is_some()
                && !record.keys.contains_key(signer)
                && !filtered
                    .grants
                    .iter()
                    .any(|grant| grant.record == record.id && &grant.recipient == signer)
            {
                let grant = self.publish_grant(record, &state, member)?;
                filtered.grants.push(grant);
            }
        }
        filtered.validate()?;
        Ok(RpcResponse::Secrets(filtered))
    }

    /// A ready server supplies old value keys to newly admitted servers. This
    /// cannot restore revoked access; all recipients come from effective membership.
    /// Missing keys on this server are left for an existing holder to grant.
    pub fn reconcile_secret_grants(&mut self, now: u64) -> RuntimeResult<()> {
        let registry = self.snapshot()?;
        self.require_server(&registry, now)?;
        let state = self.shared_snapshot()?;
        let members = registry.effective_members(now)?;
        let mut published = 0;
        for record in &state.secrets {
            if record.value.is_none() {
                continue;
            }
            let recipients: Vec<_> = members
                .values()
                .filter(|member| {
                    matches!(member.kind, MemberKind::Server(_))
                        && record.address.allowed(&member.permissions)
                        && !record.keys.contains_key(&member.device_id)
                        && !state.grants.iter().any(|grant| {
                            grant.record == record.id && grant.recipient == member.device_id
                        })
                })
                .collect();
            if recipients.is_empty()
                || record
                    .content_key(&state, &self.keys, &self.config.server_id)
                    .is_err()
            {
                continue;
            }
            for recipient in recipients {
                self.publish_grant(record, &state, recipient)?;
                published += 1;
                if published == 16 {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

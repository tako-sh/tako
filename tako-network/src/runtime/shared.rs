use super::*;
use crate::shared::{
    LegacySecretGrant, SECRET_RETRY_SECONDS, SecretMutation, SecretReceipt, SecretRecord,
    SharedSnapshot,
};

const MAX_PUBLICATION_RECEIPTS: i64 = 100_000;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Publication {
    mutation_hash: String,
    receipt: SecretReceipt,
    // Only an unfinished write retains ciphertext. Once acknowledged, the
    // receipt alone answers retries without reviving an overwritten value.
    pending: Option<SecretRecord>,
}

impl<S: RegistryStore> ServerRuntime<S> {
    fn shared_snapshot(&mut self) -> RuntimeResult<SharedSnapshot> {
        let snapshot = self.store.shared_snapshot()?;
        snapshot.validate()?;
        if snapshot.network != self.config.network.id {
            return Err(RuntimeError::Conflict("shared state network"));
        }
        let mut observed = SharedSnapshot::empty(self.config.network.id.clone());
        let mut legacy_grants = Vec::new();
        let mut seen_ids = std::collections::BTreeSet::new();
        {
            let mut statement = self
                .db
                .prepare("SELECT kind, id, payload FROM shared_observed ORDER BY kind, id")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let kind: String = row.get(0)?;
                let payload: String = row.get(2)?;
                if payload.len() > crate::shared::MAX_SHARED_RECORD_BYTES {
                    return Err(RuntimeError::Capacity("observed shared record"));
                }
                match kind.as_str() {
                    "secret" => {
                        let record: SecretRecord = serde_json::from_str(&payload)?;
                        seen_ids.insert(record.id.clone());
                        let mut addition = SharedSnapshot::empty(observed.network.clone());
                        addition.secrets.push(record);
                        observed.merge(&addition)?;
                    }
                    "grant" => legacy_grants.push((
                        row.get::<_, String>(1)?,
                        serde_json::from_str::<LegacySecretGrant>(&payload)?,
                    )),
                    _ => return Err(RuntimeError::Invalid("observed shared record kind")),
                }
            }
        }
        seen_ids.extend(snapshot.secrets.iter().map(|record| record.id.clone()));
        observed.merge(&snapshot)?;
        for (_, grant) in &legacy_grants {
            observed.import_grant(grant)?;
        }
        observed.validate()?;
        // Repair with the durable current value if replication is temporarily
        // missing it. This cache is one record per address, not a value history.
        for record in &observed.secrets {
            if !snapshot.secrets.contains(record) {
                self.store.append_secret(record)?;
            }
        }
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM shared_observed WHERE kind='secret'", [])?;
        for record in &observed.secrets {
            tx.execute(
                "INSERT INTO shared_observed VALUES ('secret',?1,?2)",
                params![
                    serde_json::to_string(&record.address)?,
                    serde_json::to_string(record)?
                ],
            )?;
        }
        for (id, grant) in &legacy_grants {
            if seen_ids.contains(&grant.record) {
                tx.execute(
                    "DELETE FROM shared_observed WHERE kind='grant' AND id=?1",
                    [id],
                )?;
            }
        }
        // The old protocol's commands have no retry deadline. New clients must
        // explicitly replace those pending commands; published values have just
        // been preserved above. Do not retain their historical ciphertexts.
        tx.execute_batch(
            "DROP TABLE IF EXISTS shared_commands; DROP TABLE IF EXISTS shared_grants;",
        )?;
        tx.commit()?;
        Ok(observed)
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
        if mutation.expires_at <= now
            || mutation.expires_at
                > now
                    .saturating_add(SECRET_RETRY_SECONDS)
                    .saturating_add(self.config.limits.auth_window_seconds)
        {
            return Err(RuntimeError::Invalid("secret publication deadline"));
        }
        self.db.execute(
            "DELETE FROM shared_publications WHERE expires<=?1",
            [sql_time(now)?],
        )?;
        let hash = hex::encode(openssl::sha::sha256(&serde_json::to_vec(&mutation)?));
        let previous: Option<String> = self
            .db
            .query_row(
                "SELECT payload FROM shared_publications WHERE id=?1",
                [mutation.id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let mut publication = if let Some(previous) = previous {
            let saved: Publication = serde_json::from_str(&previous)?;
            if &saved.receipt.author != author || saved.mutation_hash != hash {
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
                .map_or(0, |previous| previous.revision)
                .max(mutation.observed_revision)
                .checked_add(1)
                .ok_or(RuntimeError::Capacity("secret revision"))?;
            let plaintext = mutation
                .value
                .as_ref()
                .map(|value| {
                    value.open(
                        &self.keys,
                        &mutation.context(&self.config.network.id).map_err(|_| {
                            crate::error::invalid("secret command", "invalid context")
                        })?,
                    )
                })
                .transpose()?;
            if let Some(plaintext) = &plaintext {
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
                receipt: (&record).into(),
                pending: Some(record),
            };
            let count: i64 =
                self.db
                    .query_row("SELECT count(*) FROM shared_publications", [], |row| {
                        row.get(0)
                    })?;
            if count >= MAX_PUBLICATION_RECEIPTS {
                return Err(RuntimeError::Capacity("secret publication receipts"));
            }
            self.db.execute(
                "INSERT INTO shared_publications VALUES (?1,?2,?3)",
                params![
                    mutation.id.to_string(),
                    sql_time(mutation.expires_at)?,
                    serde_json::to_string(&publication)?
                ],
            )?;
            publication
        };
        if let Some(record) = &publication.pending {
            // The FULL synchronous outbox commit precedes publication. After a
            // lost response, retry uses the original revision and ciphertext.
            self.store.append_secret(record)?;
            let state = self.shared_snapshot()?;
            if state
                .latest()?
                .get(&record.address)
                .is_none_or(|current| current.order() < record.order())
            {
                return Err(RuntimeError::Corrosion("published secret absent"));
            }
            publication.pending = None;
            self.db.execute(
                "UPDATE shared_publications SET payload=?1 WHERE id=?2",
                params![
                    serde_json::to_string(&publication)?,
                    mutation.id.to_string()
                ],
            )?;
        }
        Ok(RpcResponse::SecretPublished(publication.receipt))
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
        for record in &mut filtered.secrets {
            if record.value.is_some() && !record.keys.contains_key(signer) {
                let granted = record.grant(&state, &self.keys, &self.config.server_id, member)?;
                self.store.append_secret(&granted)?;
                *record = granted;
            }
        }
        filtered.validate()?;
        // Persist newly wrapped keys before acknowledging them.
        self.shared_snapshot()?;
        Ok(RpcResponse::Secrets(filtered))
    }

    /// Supply current value keys to newly admitted servers. Superseded versions
    /// are never granted, and each successful reconciliation also retires them.
    pub fn reconcile_secret_grants(&mut self, now: u64) -> RuntimeResult<()> {
        self.db.execute(
            "DELETE FROM shared_publications WHERE expires<=?1",
            [sql_time(now)?],
        )?;
        let registry = self.snapshot()?;
        self.require_server(&registry, now)?;
        let state = self.shared_snapshot()?;
        let members = registry.effective_members(now)?;
        let mut published = 0;
        for current in &state.secrets {
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
                    && current.address.allowed(&member.permissions)
                    && !current.keys.contains_key(&member.device_id)
            }) {
                record = record.grant(&state, &self.keys, &self.config.server_id, member)?;
                published += 1;
                if published >= 32 {
                    break;
                }
            }
            if record != *current {
                self.store.append_secret(&record)?;
            }
            if published >= 32 {
                break;
            }
        }
        self.shared_snapshot()?;
        Ok(())
    }
}

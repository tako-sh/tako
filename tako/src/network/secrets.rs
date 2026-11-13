//! Shared ciphertext cache and an independent draft journal in each worktree.
use super::{Profile, Result};
use crate::config::{LocalNetwork, LocalNetworks, SecretsStore, TakoToml};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tako_network::api::{RpcRequest, RpcResponse};
use tako_network::sealed::SealedValue;
use tako_network::shared::{
    SecretAddress, SecretContent, SecretKind, SecretMutation, SharedSnapshot,
};
use tako_network::{AppEnv, DeviceId, LocalDeviceKeys, NetworkId, RequestId};
use zeroize::Zeroizing;
mod local;
use local::{Row, RowKey, Rows};

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Workspace {
    #[serde(default)]
    values: SecretsStore,
    #[serde(default)]
    environments: BTreeMap<String, EnvironmentState>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentState {
    network: NetworkId,
    scope: AppEnv,
    base: Vec<Row>,
    pending: Vec<Pending>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    issuer: DeviceId,
    mutation: SecretMutation,
    local: Option<Row>,
}

fn lock(project: &Path) -> Result<std::fs::File> {
    let directory = project.join(".tako");
    super::private_directory(&directory)?;
    let ignore = directory.join(".gitignore");
    if !ignore.try_exists()? {
        super::write_bytes(&ignore, b"*\n", false)?;
    }
    let lock = super::private_file(&directory.join("secrets.lock"), true)?;
    lock.lock()?;
    Ok(lock)
}
fn read_workspace(project: &Path) -> Result<Workspace> {
    let path = SecretsStore::default_path(project);
    if path.try_exists()? {
        return super::read_json(&path);
    }
    let legacy = project.join(".tako/secrets.json");
    let values = if legacy.try_exists()? {
        SecretsStore::load_from_file(legacy)?
    } else {
        SecretsStore::default()
    };
    Ok(Workspace {
        values,
        ..Default::default()
    })
}
fn write_workspace(project: &Path, state: &Workspace) -> Result<()> {
    state.values.validate()?;
    super::write_json(&SecretsStore::default_path(project), state, true)
}

pub(crate) fn load_local(project: &Path) -> Result<SecretsStore> {
    let _lock = if crate::output::is_dry_run() {
        None
    } else {
        Some(lock(project)?)
    };
    let mut state = read_workspace(project)?;
    let mut materialized = false;
    // A new worktree starts from the machine's authenticated shared cache. It
    // never reads another checkout's unpublished values.
    if let Ok(config) = TakoToml::load_from_dir(project)
        && let Some(app) = config.name.as_deref()
    {
        let networks = LocalNetworks::load()?;
        for (env, spec) in &config.envs {
            if state.environments.contains_key(env) {
                continue;
            }
            let Some(id) = spec.network.as_deref() else {
                continue;
            };
            let Ok(alias) = networks.select(Some(id)) else {
                continue;
            };
            let Some(cache) = read_cache(alias)? else {
                continue;
            };
            let profile = super::load_profile(alias)?;
            let scope: AppEnv = format!("{app}/{env}").parse()?;
            merge_remote(&mut state, env, &scope, &profile, &cache)?;
            materialized = true;
        }
    }
    // Persist the baseline together with any cache materialization; otherwise
    // a later edit could mistake cached values for unpublished local changes.
    // A read must not rewrite existing drafts: dev watches this file and reloads
    // secrets when it changes, so rewriting here would trigger a restart loop.
    if !crate::output::is_dry_run() && materialized {
        write_workspace(project, &state)?;
    }
    let mut values = state.values;
    values.loaded = Some(values.environments.clone());
    Ok(values)
}

pub(crate) fn save_local(project: &Path, values: &SecretsStore) -> Result<()> {
    let _lock = lock(project)?;
    let mut state = read_workspace(project)?;
    if values
        .loaded
        .as_ref()
        .is_some_and(|loaded| loaded != &state.values.environments)
    {
        return Err(
            "Secrets changed in this checkout while the command was running; retry the edit".into(),
        );
    }
    state.values = values.clone();
    write_workspace(project, &state)
}

fn cache_path(alias: &LocalNetwork) -> Result<std::path::PathBuf> {
    Ok(alias.profile_path()?.with_file_name("secrets.json"))
}
fn read_cache(alias: &LocalNetwork) -> Result<Option<SharedSnapshot>> {
    let path = cache_path(alias)?;
    if !path.try_exists()? {
        return Ok(None);
    }
    let cache: SharedSnapshot = super::read_json(&path)?;
    if cache.network.to_string() != alias.id {
        return Err("Secret cache belongs to another network".into());
    }
    cache.validate()?;
    Ok(Some(cache))
}
fn cache_merge(
    alias: &LocalNetwork,
    incoming: &SharedSnapshot,
    profile: &Profile,
) -> Result<SharedSnapshot> {
    let path = cache_path(alias)?;
    let lock = super::private_file(&path.with_extension("lock"), true)?;
    lock.lock()?;
    let mut cache =
        read_cache(alias)?.unwrap_or_else(|| SharedSnapshot::empty(incoming.network.clone()));
    cache.merge(incoming)?;
    let own = profile.request().device_id_for(&cache.network)?;
    cache = cache.for_permissions(profile.snapshot().permissions(&own, super::now()?)?)?;
    super::write_json(&path, &cache, true)?;
    Ok(cache)
}

/// Pulling membership or shared values never publishes any worktree's drafts.
pub(crate) fn pull(
    alias: &LocalNetwork,
    scope: Option<AppEnv>,
) -> Result<(Profile, SharedSnapshot)> {
    let profile = super::client::refresh(alias)?;
    let server = super::client::available_server(&profile)?;
    let RpcResponse::Secrets(incoming) =
        super::client::call(&profile, &server, RpcRequest::PullSecrets { scope })?
    else {
        return Err("Unexpected shared secret response".into());
    };
    let cache = cache_merge(alias, &incoming, &profile)?;
    Ok((profile, cache))
}

fn bind<'a>(
    state: &'a mut Workspace,
    env: &str,
    scope: &AppEnv,
    network: &NetworkId,
) -> Result<&'a mut EnvironmentState> {
    let entry = state
        .environments
        .entry(env.into())
        .or_insert_with(|| EnvironmentState {
            network: network.clone(),
            scope: scope.clone(),
            base: vec![],
            pending: vec![],
        });
    if &entry.network != network || &entry.scope != scope {
        return Err("This checkout's secret drafts are bound to another app or network. Use a fresh worktree for a different deployment target".into());
    }
    Ok(entry)
}
fn merge_remote(
    state: &mut Workspace,
    env: &str,
    scope: &AppEnv,
    profile: &Profile,
    cache: &SharedSnapshot,
) -> Result<()> {
    let previous = bind(state, env, scope, &cache.network)?
        .base
        .iter()
        .map(|row| (row.key.clone(), row.clone()))
        .collect::<Rows>();
    let current = local::rows(state.values.environments.get(env))?;
    let mut dirty: BTreeSet<_> = current
        .keys()
        .chain(previous.keys())
        .filter(|key| current.get(*key) != previous.get(*key))
        .cloned()
        .collect();
    dirty.extend(
        bind(state, env, scope, &cache.network)?
            .pending
            .iter()
            .map(|pending| RowKey {
                kind: pending.mutation.address.kind,
                name: pending.mutation.address.name.clone(),
            }),
    );
    let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
    let device = profile.request().device_id_for(&cache.network)?;
    let permissions = profile.snapshot().permissions(&device, super::now()?)?;
    let latest = cache.latest()?;
    let records: Vec<_> = latest
        .values()
        .filter(|record| &record.address.scope == scope && record.address.allowed(permissions))
        .copied()
        .collect();
    if records.is_empty() && current.is_empty() {
        return Ok(());
    }
    let had_values = state.values.env_has_encrypted_values(env);
    let key_id = state.values.ensure_env_key_id(env)?;
    let store = crate::crypto::KeyStore::for_key_id(&key_id)?;
    let encryption = match store.load_key_optional()? {
        Some(key) => key,
        None if !had_values => { let key = crate::crypto::EncryptionKey::generate()?; store.save_key(&key)?; key }
        None => return Err("Existing encrypted secrets need their original local key; import it before synchronizing".into()),
    };
    let mut remote = Rows::new();
    for record in records {
        let Some(bytes) = record.decrypt(cache, &keys, &device)? else {
            continue;
        };
        let content: SecretContent = serde_json::from_slice(&bytes)?;
        let key = RowKey {
            kind: record.address.kind,
            name: record.address.name.clone(),
        };
        remote.insert(key.clone(), local::encrypted(key, content, &encryption)?);
    }
    let mut merged = remote.clone();
    for key in dirty {
        match current.get(&key) {
            Some(value) => {
                merged.insert(key, value.clone());
            }
            None => {
                merged.remove(&key);
            }
        }
    }
    state
        .values
        .environments
        .insert(env.into(), local::environment(key_id, &merged)?);
    bind(state, env, scope, &cache.network)?.base = remote.into_values().collect();
    Ok(())
}

pub(crate) fn sync_environment(
    project: &Path,
    config: &TakoToml,
    app: &str,
    env: &str,
    publish: bool,
) -> Result<SecretsStore> {
    let id = config
        .envs
        .get(env)
        .and_then(|spec| spec.network.as_deref())
        .ok_or("Set this environment's network ID in tako.toml before synchronizing secrets")?;
    let alias = LocalNetworks::load()?.select(Some(id))?.clone();
    let scope: AppEnv = format!("{app}/{env}").parse()?;
    let _lock = lock(project)?;
    let mut state = read_workspace(project)?;
    let (profile, cache) = pull(&alias, Some(scope.clone()))?;
    merge_remote(&mut state, env, &scope, &profile, &cache)?;
    write_workspace(project, &state)?;
    if publish {
        publish_changes(project, &mut state, env, &scope, &alias, &profile)?;
        let (profile, cache) = pull(&alias, Some(scope.clone()))?;
        merge_remote(&mut state, env, &scope, &profile, &cache)?;
        write_workspace(project, &state)?;
    }
    let mut values = state.values;
    values.loaded = Some(values.environments.clone());
    Ok(values)
}

fn publish_changes(
    project: &Path,
    state: &mut Workspace,
    env: &str,
    scope: &AppEnv,
    alias: &LocalNetwork,
    profile: &Profile,
) -> Result<()> {
    let network = &profile.snapshot().network.id;
    // Finish durable commands first, even when a subsequent edit superseded
    // their local values. They retain their original server-assigned revision.
    loop {
        let pending = bind(state, env, scope, network)?.pending.first().cloned();
        let Some(pending) = pending else {
            break;
        };
        let observed = read_cache(alias)?.and_then(|cache| {
            cache
                .secrets
                .into_iter()
                .find(|record| record.id == pending.mutation.id)
        });
        let record = if let Some(record) = observed {
            record
        } else {
            let RpcResponse::SecretPublished(record) = super::client::call(
                profile,
                &pending.issuer,
                RpcRequest::PublishSecret(Box::new(pending.mutation.clone())),
            )?
            else {
                return Err("Unexpected secret publication response".into());
            };
            record
        };
        if record.id != pending.mutation.id
            || record.address != pending.mutation.address
            || record.issuer != pending.issuer
            || record.author != profile.request().device_id_for(network)?
            || record.revision <= pending.mutation.observed_revision
        {
            return Err("Secret publication response does not match the pending command".into());
        }
        let mut addition = SharedSnapshot::empty(network.clone());
        addition.secrets.push(record);
        cache_merge(alias, &addition, profile)?;
        let entry = bind(state, env, scope, network)?;
        let key = RowKey {
            kind: pending.mutation.address.kind,
            name: pending.mutation.address.name,
        };
        entry.base.retain(|row| row.key != key);
        if let Some(row) = pending.local {
            entry.base.push(row);
        }
        entry.pending.remove(0);
        write_workspace(project, state)?;
    }
    let previous: Rows = bind(state, env, scope, network)?
        .base
        .iter()
        .map(|row| (row.key.clone(), row.clone()))
        .collect();
    let current = local::rows(state.values.environments.get(env))?;
    let dirty: BTreeSet<_> = current
        .keys()
        .chain(previous.keys())
        .filter(|key| current.get(*key) != previous.get(*key))
        .cloned()
        .collect();
    if dirty.is_empty() {
        return Ok(());
    }
    let issuer = super::client::available_server(profile)?;
    let members = profile.snapshot().effective_members(super::now()?)?;
    let recipient = &members
        .get(&issuer)
        .ok_or("Publication server is no longer a member")?
        .request
        .encryption_public_key;
    let key = state
        .values
        .get_key_id(env)
        .map(crate::crypto::KeyStore::for_key_id)
        .transpose()?
        .map(|store| store.load_key())
        .transpose()?;
    let observed_cache =
        read_cache(alias)?.unwrap_or_else(|| SharedSnapshot::empty(network.clone()));
    let observed = observed_cache.latest()?;
    for row_key in dirty {
        let id = RequestId::random()?;
        let address = SecretAddress {
            scope: scope.clone(),
            kind: row_key.kind,
            name: row_key.name.clone(),
        };
        let observed_revision = observed.get(&address).map_or(0, |record| record.revision);
        let value = current
            .get(&row_key)
            .map(|row| -> Result<_> {
                let content =
                    local::plaintext(row, key.as_ref().ok_or("Local secret key is missing")?)?;
                let bytes = Zeroizing::new(serde_json::to_vec(&content)?);
                Ok(SealedValue::seal(
                    recipient,
                    &bytes,
                    &address.context(network, &id)?,
                )?)
            })
            .transpose()?;
        bind(state, env, scope, network)?.pending.push(Pending {
            issuer: issuer.clone(),
            mutation: SecretMutation {
                id,
                address,
                value,
                observed_revision,
            },
            local: current.get(&row_key).cloned(),
        });
    }
    write_workspace(project, state)?;
    // The second pass only drains the just-persisted operations. No new dirty
    // records remain once their exact local values become the baseline.
    publish_changes(project, state, env, scope, alias, profile)
}

#[cfg(test)]
mod tests;

/// Preserve the original ciphertext in an ignored recovery copy before removing
/// the tracked file. This changes the working tree, never Git history.
pub(crate) fn migrate(project: &Path) -> Result<()> {
    let _lock = lock(project)?;
    let state = read_workspace(project)?;
    let legacy = project.join(".tako/secrets.json");
    let legacy_values = if legacy.try_exists()? {
        Some(SecretsStore::load_from_file(&legacy)?)
    } else {
        None
    };
    // An existing draft may use a different key. Validate the original file
    // independently before replacing it with an ignored recovery copy.
    for values in std::iter::once(&state.values).chain(legacy_values.as_ref()) {
        for (env, environment) in &values.environments {
            let rows = local::rows(Some(environment))?;
            if rows.is_empty() {
                continue;
            }
            let key = crate::commands::secret::load_secret_key(env, values, Some(project))?;
            for row in rows.values() {
                local::plaintext(row, &key)?;
            }
        }
    }
    write_workspace(project, &state)?;
    if legacy.try_exists()? {
        let bytes = std::fs::read(&legacy)?;
        let recovery = project.join(".tako/secrets.legacy.json");
        if recovery.try_exists()? {
            if std::fs::read(&recovery)? != bytes {
                return Err("A different secret recovery copy already exists; preserve it before migrating again".into());
            }
        } else {
            super::write_bytes(&recovery, &bytes, false)?;
        }
        std::fs::remove_file(legacy)?;
    }
    Ok(())
}

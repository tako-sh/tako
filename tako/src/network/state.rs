//! This device's authenticated cache of the network state: the shared records
//! it may read, pulled from servers over signed, pinned HTTPS.
use super::{Profile, Result};
use crate::config::LocalNetwork;
use tako_network::api::{RpcRequest, RpcResponse};
use tako_network::shared::SharedSnapshot;
use tako_network::{AppEnv, DeviceId};

fn cache_path(alias: &LocalNetwork) -> Result<std::path::PathBuf> {
    // The cache keeps the file name it had when it held only secrets.
    Ok(alias.profile_path()?.with_file_name("secrets.json"))
}

pub(crate) fn read_cache(alias: &LocalNetwork) -> Result<Option<SharedSnapshot>> {
    let path = cache_path(alias)?;
    if !path.try_exists()? {
        return Ok(None);
    }
    let stored: serde_json::Value = super::read_json(&path)?;
    let cache = SharedSnapshot::from_persisted(&serde_json::to_vec(&stored)?)?;
    if cache.network.to_string() != alias.id {
        return Err("Network state cache belongs to another network".into());
    }
    cache.validate()?;
    Ok(Some(cache))
}

pub(crate) fn cache_merge(
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

/// Pulls the records in `scope`, or every record this device may read.
/// Pulling never publishes any worktree's drafts.
pub(crate) fn pull(
    alias: &LocalNetwork,
    scope: Option<AppEnv>,
) -> Result<(Profile, SharedSnapshot)> {
    let profile = super::client::refresh(alias)?;
    let first = super::client::available_server(&profile)?;
    let mut cache = pull_from(alias, &profile, &first, &scope)?;
    // A server that has not received a record's key yet returns the record
    // without a key for this device. Another server may be able to add it.
    let device = profile.request().device_id_for(&cache.network)?;
    if missing_keys(&cache, &device, scope.as_ref()) {
        for server in super::client::trusted_servers(&profile)?.into_keys() {
            if server == first {
                continue;
            }
            let Ok(merged) = pull_from(alias, &profile, &server, &scope) else {
                continue;
            };
            cache = merged;
            if !missing_keys(&cache, &device, scope.as_ref()) {
                break;
            }
        }
    }
    Ok((profile, cache))
}

fn pull_from(
    alias: &LocalNetwork,
    profile: &Profile,
    server: &DeviceId,
    scope: &Option<AppEnv>,
) -> Result<SharedSnapshot> {
    let RpcResponse::Records(incoming) = super::client::call(
        profile,
        server,
        RpcRequest::PullRecords {
            scope: scope.clone(),
        },
    )?
    else {
        return Err("Unexpected network state response".into());
    };
    cache_merge(alias, &incoming, profile)
}

/// Whether a sealed record in `scope` lacks this device's key.
fn missing_keys(cache: &SharedSnapshot, device: &DeviceId, scope: Option<&AppEnv>) -> bool {
    cache.records.iter().any(|record| {
        scope.is_none_or(|scope| &record.address.scope == scope)
            && record.value.is_some()
            && !record.keys.contains_key(device)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::tests::fixture;
    use tako_network::RequestId;
    use tako_network::shared::{
        RecordAddress, RecordContent, RecordKind, RecordMutation, SharedRecord,
    };

    #[test]
    fn only_sealed_records_in_scope_without_this_devices_key_are_missing() {
        let fixture = fixture();
        let network = fixture.profile.snapshot().network.id.clone();
        let device = fixture.profile.request().device_id_for(&network).unwrap();
        let record = |kind, content: Option<&[u8]>| {
            SharedRecord::create(
                &RecordMutation {
                    id: RequestId::random().unwrap(),
                    observed_revision: 0,
                    expires_at: 86400,
                    address: RecordAddress {
                        scope: "demo/production".parse().unwrap(),
                        kind,
                        name: "NAME".into(),
                    },
                    value: None,
                },
                content,
                1,
                device.clone(),
                device.clone(),
                fixture.profile.snapshot(),
                super::super::now().unwrap(),
            )
            .unwrap()
        };
        let selection = serde_json::to_vec(&RecordContent {
            value: zeroize::Zeroizing::new("backup-key-0123456789abcdef".into()),
            expires_on: None,
        })
        .unwrap();
        let sealed = record(RecordKind::App, Some(b"value"));
        let mut cache = SharedSnapshot::empty(network);
        cache.records = vec![
            sealed.clone(),
            record(RecordKind::Credential, None),
            record(RecordKind::BackupActive, Some(&selection)),
        ];
        assert!(!missing_keys(&cache, &device, None));
        let mut unkeyed = sealed;
        unkeyed.keys.clear();
        cache.records.push(unkeyed);
        assert!(missing_keys(&cache, &device, None));
        assert!(missing_keys(
            &cache,
            &device,
            Some(&"demo/production".parse().unwrap())
        ));
        assert!(!missing_keys(
            &cache,
            &device,
            Some(&"demo/staging".parse().unwrap())
        ));
    }
}

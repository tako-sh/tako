//! Remote replica identity. One data key is shared by all consumers, while each
//! consumer keeps its own recovery key. Conditional writes prevent lost updates.
use crate::encryption::{EncryptedStore, Key, open, seal};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, UpdateVersion, path::Path};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u8,
    key_id: String,
    envelopes: std::collections::BTreeMap<String, String>,
}

fn data_key(bytes: [u8; 32]) -> Result<Key> {
    Key::new(hex::encode(Sha256::digest(bytes)), bytes)
}

/// Opens or creates a replica. `saved` is kept encrypted in the server state;
/// without it, any registered consumer's recovery key can recover the data key.
pub async fn connect(
    remote: Arc<dyn ObjectStore>,
    prefix: &Path,
    recovery: &[Key],
    saved: Option<[u8; 32]>,
) -> Result<(Arc<dyn ObjectStore>, Key)> {
    ensure!(!recovery.is_empty(), "replica requires a recovery key");
    let path = prefix.clone().join("keys.json");
    let context = format!("tako replica key v1\0{prefix}");
    for _ in 0..8 {
        let (mut manifest, version, key) = match remote.get(&path).await {
            Ok(object) => {
                let version = UpdateVersion {
                    e_tag: object.meta.e_tag.clone(),
                    version: object.meta.version.clone(),
                };
                ensure!(
                    object.meta.size <= 1024 * 1024,
                    "replica key manifest is too large"
                );
                let manifest: Manifest = serde_json::from_slice(&object.bytes().await?)?;
                ensure!(
                    manifest.version == 1,
                    "unsupported replica manifest version"
                );
                let key = if let Some(bytes) = saved {
                    data_key(bytes)?
                } else {
                    let mut recovered = None;
                    for key in recovery {
                        if let Some(envelope) = manifest.envelopes.get(&key.id) {
                            let bytes = open(
                                std::slice::from_ref(key),
                                context.as_bytes(),
                                &STANDARD.decode(envelope)?,
                            )?;
                            recovered = Some(data_key(
                                bytes
                                    .try_into()
                                    .map_err(|_| anyhow::anyhow!("invalid replica data key"))?,
                            )?);
                            break;
                        }
                    }
                    recovered.context(
                        "none of this app's backup keys can recover the existing database replica",
                    )?
                };
                ensure!(key.id == manifest.key_id, "replica data key changed");
                (manifest, Some(version), key)
            }
            Err(object_store::Error::NotFound { .. }) => {
                let key = match saved {
                    Some(bytes) => data_key(bytes)?,
                    None => {
                        let mut bytes = [0; 32];
                        openssl::rand::rand_bytes(&mut bytes)?;
                        data_key(bytes)?
                    }
                };
                // Never start a new encryption identity over an existing history.
                use futures::TryStreamExt;
                ensure!(
                    remote
                        .list(Some(&prefix.clone().join("ltx")))
                        .try_next()
                        .await?
                        .is_none(),
                    "replica history exists but its key manifest is missing"
                );
                (
                    Manifest {
                        version: 1,
                        key_id: key.id.clone(),
                        envelopes: Default::default(),
                    },
                    None,
                    key,
                )
            }
            Err(error) => return Err(error.into()),
        };
        let mut changed = version.is_none();
        for recovery_key in recovery {
            if let Some(envelope) = manifest.envelopes.get(&recovery_key.id) {
                ensure!(
                    open(
                        std::slice::from_ref(recovery_key),
                        context.as_bytes(),
                        &STANDARD.decode(envelope)?
                    )? == key.bytes(),
                    "recovery key id conflicts with the registered key"
                );
            } else {
                manifest.envelopes.insert(
                    recovery_key.id.clone(),
                    STANDARD.encode(seal(recovery_key, context.as_bytes(), key.bytes())?),
                );
                changed = true;
            }
        }
        if changed {
            let mode = version.map_or(PutMode::Create, PutMode::Update);
            match remote
                .put_opts(
                    &path,
                    serde_json::to_vec(&manifest)?.into(),
                    PutOptions {
                        mode,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(_) => {}
                Err(
                    object_store::Error::AlreadyExists { .. }
                    | object_store::Error::Precondition { .. },
                ) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        return Ok((
            Arc::new(EncryptedStore::new(
                remote,
                vec![key.clone()],
                prefix.to_string(),
            )?),
            key,
        ));
    }
    anyhow::bail!("replica key manifest changed concurrently; retry registration")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn two_apps_share_one_key_and_either_can_recover_it() {
        let remote = Arc::new(object_store::memory::InMemory::new());
        let a = Key::new("app-a".into(), [1; 32]).unwrap();
        let b = Key::new("app-b".into(), [2; 32]).unwrap();
        let prefix = Path::from("shared/database");
        let (_, key) = connect(remote.clone(), &prefix, &[a], None).await.unwrap();
        let (_, joined) = connect(
            remote.clone(),
            &prefix,
            std::slice::from_ref(&b),
            Some(*key.bytes()),
        )
        .await
        .unwrap();
        assert_eq!(key.bytes(), joined.bytes());
        let (_, recovered) = connect(remote.clone(), &prefix, &[b], None).await.unwrap();
        assert_eq!(key.bytes(), recovered.bytes());
        let unknown = Key::new("unknown".into(), [3; 32]).unwrap();
        assert!(connect(remote, &prefix, &[unknown], None).await.is_err());
    }
}

//! The layout and format of hosted databases' snapshots in backup storage.
//!
//! A snapshot is a compact copy of the database, compressed with zstd and
//! encrypted with the database's backup key, stored next to a JSON manifest
//! that is written last. Shared by tako-server, which takes and restores
//! snapshots, and the CLI, which restores one onto a developer's machine.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt, path::Path as ObjectPath};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tako_core::{BackupEncryptionInfo, BackupKeyBinding, DatabaseSnapshot};
use tokio::io::AsyncWriteExt;

/// The manifest format this code reads and writes.
pub const MANIFEST_FORMAT: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub database: String,
    pub generation: String,
    pub snapshot: DatabaseSnapshot,
    pub encryption: BackupEncryptionInfo,
}

/// Snapshots live under the database's name and generation, so a database
/// created again under an earlier one's name never sees its snapshots.
pub fn prefix(name: &str, generation: &str) -> String {
    format!("_tako/databases/{name}/{generation}/snapshots")
}

pub fn manifest_key(name: &str, generation: &str, snapshot: &str) -> String {
    format!("{}/{snapshot}.json", prefix(name, generation))
}

pub fn data_key(name: &str, generation: &str, snapshot: &str) -> String {
    format!("{}/{snapshot}.sqlite.zst.enc", prefix(name, generation))
}

/// Names the server that holds the database's snapshots.
pub fn holder_key(name: &str, generation: &str) -> String {
    format!("{}/holder", prefix(name, generation))
}

/// The key a database's snapshots are encrypted with.
pub fn key_binding(generation: &str, key_base64: &str) -> BackupKeyBinding {
    BackupKeyBinding {
        id: format!("database-{generation}"),
        key_base64: key_base64.to_owned(),
    }
}

async fn with_deadline<T>(
    timeout: Duration,
    operation: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| "backup storage request timed out".to_string())?
}

/// Every object under a database's snapshot prefix, with the time it was
/// last written (Unix seconds).
pub async fn list_objects(
    store: &dyn ObjectStore,
    name: &str,
    generation: &str,
    timeout: Duration,
) -> Result<Vec<(ObjectPath, i64)>, String> {
    let prefix = ObjectPath::parse(prefix(name, generation)).map_err(|e| e.to_string())?;
    with_deadline(timeout, async {
        store
            .list(Some(&prefix))
            .map_ok(|meta| (meta.location, meta.last_modified.timestamp()))
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| format!("list database snapshots: {e}"))
    })
    .await
}

/// The manifests among `objects` of this database, newest first. Unreadable
/// ones, and ones of another database, are skipped.
pub async fn read_manifests(
    store: &dyn ObjectStore,
    name: &str,
    generation: &str,
    objects: &[(ObjectPath, i64)],
    timeout: Duration,
) -> Result<Vec<Manifest>, String> {
    let mut manifests = Vec::new();
    for (key, _) in objects
        .iter()
        .filter(|(key, _)| key.as_ref().ends_with(".json"))
    {
        let bytes = with_deadline(timeout, async {
            store
                .get(key)
                .await
                .map_err(|e| format!("read snapshot manifest: {e}"))?
                .bytes()
                .await
                .map_err(|e| format!("read snapshot manifest: {e}"))
        })
        .await?;
        match serde_json::from_slice::<Manifest>(&bytes) {
            Ok(manifest) if manifest.database == name && manifest.generation == generation => {
                manifests.push(manifest)
            }
            Ok(_) => tracing::warn!(%key, "Ignoring a snapshot manifest of another database"),
            Err(error) => tracing::warn!(%key, %error, "Ignoring an unreadable snapshot manifest"),
        }
    }
    // Newest first; within a second, by the millisecond-ordered ID.
    manifests.sort_by(|a, b| {
        (b.snapshot.created_at_unix_secs, &b.snapshot.id)
            .cmp(&(a.snapshot.created_at_unix_secs, &a.snapshot.id))
    });
    Ok(manifests)
}

/// A database's snapshots, newest first.
pub async fn list_manifests(
    store: &dyn ObjectStore,
    name: &str,
    generation: &str,
    timeout: Duration,
) -> Result<Vec<Manifest>, String> {
    let objects = list_objects(store, name, generation, timeout).await?;
    read_manifests(store, name, generation, &objects, timeout).await
}

/// The snapshot a restore picks: `wanted` by ID, or the newest one of the
/// database's own history (copies taken before restores and imports are
/// left out, so repeating a restore does not undo it).
pub fn choose(
    manifests: Vec<Manifest>,
    name: &str,
    wanted: Option<&str>,
) -> Result<Manifest, String> {
    if let Some(wanted) = wanted {
        return manifests
            .into_iter()
            .find(|manifest| manifest.snapshot.id == wanted)
            .ok_or_else(|| format!("Snapshot {wanted} of database {name} was not found."));
    }
    let fallback = manifests.first().map(|m| m.snapshot.id.clone());
    manifests
        .into_iter()
        .find(|m| m.snapshot.reason != tako_core::SnapshotReason::Restore)
        .ok_or_else(|| match fallback {
            None => format!("Database {name} has no snapshots yet."),
            Some(id) => format!(
                "Database {name} has only snapshots taken before restores or imports; pass --snapshot {id} to restore one."
            ),
        })
}

/// Downloads an object to `path`.
pub async fn download(
    store: &dyn ObjectStore,
    key: &str,
    path: &Path,
    timeout: Duration,
) -> Result<(), String> {
    with_deadline(timeout, async {
        let key = ObjectPath::parse(key).map_err(|e| e.to_string())?;
        let mut stream = store
            .get(&key)
            .await
            .map_err(|e| format!("download backup object: {e}"))?
            .into_stream();
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|e| format!("create backup download: {e}"))?;
        while let Some(chunk) = stream
            .try_next()
            .await
            .map_err(|e| format!("read backup object: {e}"))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("write backup download: {e}"))?;
        }
        file.shutdown()
            .await
            .map_err(|e| format!("flush backup download: {e}"))
    })
    .await
}

pub fn decompress(input: &Path, output: &Path) -> Result<(), String> {
    let reader = std::fs::File::open(input).map_err(|e| e.to_string())?;
    let mut decoder = zstd::stream::Decoder::new(reader).map_err(|e| e.to_string())?;
    let mut writer = std::fs::File::create(output).map_err(|e| e.to_string())?;
    std::io::copy(&mut decoder, &mut writer).map_err(|e| format!("decompress snapshot: {e}"))?;
    writer.sync_all().map_err(|e| e.to_string())
}

/// SHA-256 of a file, as hex, and its size.
pub fn sha256_file(path: &Path) -> Result<(String, u64), String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((hex::encode(hasher.finalize()), total))
}

/// Turns a downloaded snapshot (`encrypted`) into the database file it
/// holds, in `dir`, and checks it against the manifest's checksum. The
/// caller verifies the database itself. Returns the restored file's path.
pub fn open(
    encrypted: &Path,
    dir: &Path,
    manifest: &Manifest,
    key: &BackupKeyBinding,
) -> Result<PathBuf, String> {
    if manifest.format != MANIFEST_FORMAT {
        return Err(format!(
            "Snapshot {} is in format {}, which this version cannot read (it reads format {MANIFEST_FORMAT}); upgrade it.",
            manifest.snapshot.id, manifest.format
        ));
    }
    let compressed = dir.join("snapshot.sqlite.zst");
    let restored = dir.join("restored.sqlite");
    crate::encryption::decrypt_file(encrypted, &compressed, key, &manifest.encryption)?;
    decompress(&compressed, &restored)?;
    let _ = std::fs::remove_file(&compressed);
    let (sha256, _) = sha256_file(&restored)?;
    if sha256 != manifest.snapshot.sha256 {
        return Err("the downloaded snapshot does not match its checksum".into());
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use tako_core::SnapshotReason;

    fn snapshot(id: &str, reason: SnapshotReason) -> DatabaseSnapshot {
        DatabaseSnapshot {
            id: id.into(),
            created_at_unix_secs: 0,
            reason,
            database_bytes: 0,
            stored_bytes: 0,
            sha256: String::new(),
        }
    }

    fn manifest(snapshot: DatabaseSnapshot, encryption: BackupEncryptionInfo) -> Manifest {
        Manifest {
            format: MANIFEST_FORMAT,
            database: "shop".into(),
            generation: "g1".into(),
            snapshot,
            encryption,
        }
    }

    #[test]
    fn a_snapshot_opens_into_the_file_it_was_taken_from() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.sqlite");
        std::fs::write(&original, b"SQLite format 3\0 and pages".repeat(100)).unwrap();
        let compressed = dir.path().join("snapshot.zst");
        {
            let reader = std::fs::File::open(&original).unwrap();
            let writer = std::fs::File::create(&compressed).unwrap();
            let mut encoder = zstd::stream::Encoder::new(writer, 3).unwrap();
            std::io::copy(&mut &reader, &mut encoder).unwrap();
            encoder.finish().unwrap();
        }
        let key = key_binding(
            "g1",
            &base64::engine::general_purpose::STANDARD.encode([3u8; 32]),
        );
        let encrypted = dir.path().join("snapshot.zst.enc");
        let encryption = crate::encryption::encrypt_file(&compressed, &encrypted, &key).unwrap();
        let mut taken = snapshot("1-a", SnapshotReason::Manual);
        taken.sha256 = sha256_file(&original).unwrap().0;
        let work = tempfile::tempdir().unwrap();
        let restored = open(
            &encrypted,
            work.path(),
            &manifest(taken.clone(), encryption.clone()),
            &key,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(restored).unwrap(),
            std::fs::read(&original).unwrap()
        );
        // A checksum that does not match is refused.
        taken.sha256 = "0".repeat(64);
        let error = open(&encrypted, work.path(), &manifest(taken, encryption), &key).unwrap_err();
        assert!(error.contains("checksum"), "{error}");
    }

    #[test]
    fn a_restore_picks_the_newest_snapshot_of_the_databases_own_history() {
        let encryption = BackupEncryptionInfo {
            algorithm: crate::encryption::ALGORITHM.into(),
            key_id: "database-g1".into(),
            nonce_base64: String::new(),
            tag_base64: String::new(),
        };
        let manifests = || {
            vec![
                manifest(snapshot("3-c", SnapshotReason::Restore), encryption.clone()),
                manifest(
                    snapshot("2-b", SnapshotReason::Scheduled),
                    encryption.clone(),
                ),
            ]
        };
        assert_eq!(
            choose(manifests(), "shop", None).unwrap().snapshot.id,
            "2-b"
        );
        assert_eq!(
            choose(manifests(), "shop", Some("3-c"))
                .unwrap()
                .snapshot
                .id,
            "3-c"
        );
        assert!(choose(manifests(), "shop", Some("9-z")).is_err());
        assert!(
            choose(Vec::new(), "shop", None)
                .unwrap_err()
                .contains("no snapshots")
        );
    }
}

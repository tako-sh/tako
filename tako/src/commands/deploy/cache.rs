use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::build::compute_file_hash;

pub(super) const ARTIFACT_CACHE_SCHEMA_VERSION: u32 = 0;

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(super) struct ArtifactCacheMetadata {
    pub(super) schema_version: u32,
    pub(super) artifact_sha256: String,
    pub(super) artifact_size: u64,
}

#[derive(Debug, Clone)]
pub(super) struct ArtifactCachePaths {
    pub(super) artifact_path: PathBuf,
    pub(super) metadata_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(super) struct CachedArtifact {
    pub(super) path: PathBuf,
    pub(super) size_bytes: u64,
}

pub(super) fn sanitize_cache_label(label: &str) -> String {
    label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn artifact_cache_paths(
    cache_dir: &Path,
    version: &str,
    target_label: Option<&str>,
) -> ArtifactCachePaths {
    let base = match target_label {
        Some(label) => cache_dir.join(sanitize_cache_label(label)),
        None => cache_dir.to_path_buf(),
    };
    ArtifactCachePaths {
        artifact_path: base.join(format!("{}.tar.zst", version)),
        metadata_path: base.join(format!("{}.json", version)),
    }
}

pub(super) fn artifact_cache_paths_for_manifest(
    cache_dir: &Path,
    version: &str,
    target_label: Option<&str>,
    manifest: &[u8],
) -> ArtifactCachePaths {
    use sha2::{Digest, Sha256};

    let cache_key = format!("{version}-{}", hex::encode(Sha256::digest(manifest)));
    artifact_cache_paths(cache_dir, &cache_key, target_label)
}

pub(super) fn artifact_cache_temp_path(final_path: &Path) -> Result<PathBuf, String> {
    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("Invalid cache artifact filename '{}'", final_path.display()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_name = format!("{}.tmp-{}-{}", file_name, std::process::id(), nanos);
    Ok(final_path.with_file_name(tmp_name))
}

pub(super) fn cleanup_local_artifact_cache(
    cache_dir: &Path,
    keep_target_artifacts: usize,
) -> Result<super::task_tree::LocalArtifactCacheCleanupSummary, String> {
    use super::task_tree::LocalArtifactCacheCleanupSummary;

    if !cache_dir.exists() {
        return Ok(LocalArtifactCacheCleanupSummary::default());
    }

    let mut summary = LocalArtifactCacheCleanupSummary::default();

    // Collect artifacts from cache_dir itself and from target subdirectories.
    let mut all_artifacts: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    let mut all_metadata: Vec<PathBuf> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();

    // Scan a single directory, collecting artifacts/metadata and subdirs.
    fn scan_artifact_dir(
        dir: &Path,
        artifacts: &mut Vec<(PathBuf, std::time::SystemTime)>,
        metadata: &mut Vec<PathBuf>,
        mut subdirs: Option<&mut Vec<PathBuf>>,
    ) -> Result<(), String> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(()),
        };
        for entry in entries {
            let entry =
                entry.map_err(|e| format!("Failed to read dir entry in {}: {e}", dir.display()))?;
            let path = entry.path();
            let file_name = match path.file_name().and_then(|name| name.to_str()) {
                Some(name) => name,
                None => continue,
            };
            let meta = entry
                .metadata()
                .map_err(|e| format!("Failed to read metadata for {}: {e}", path.display()))?;
            if meta.is_dir() {
                if let Some(ref mut subs) = subdirs {
                    subs.push(path);
                }
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            if file_name.ends_with(".tar.zst") {
                artifacts.push((path, meta.modified().unwrap_or(UNIX_EPOCH)));
            } else if file_name.ends_with(".json") {
                metadata.push(path);
            }
        }
        Ok(())
    }

    scan_artifact_dir(
        cache_dir,
        &mut all_artifacts,
        &mut all_metadata,
        Some(&mut subdirs),
    )?;
    for subdir in subdirs {
        scan_artifact_dir(&subdir, &mut all_artifacts, &mut all_metadata, None)?;
    }

    all_artifacts.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.0.cmp(&left.0)));

    for (path, _) in all_artifacts.into_iter().skip(keep_target_artifacts) {
        std::fs::remove_file(&path)
            .map_err(|e| format!("Failed to remove artifact cache {}: {e}", path.display()))?;
        summary.removed_target_artifacts += 1;

        if let Some(metadata_path) = artifact_cache_metadata_path_for_archive(&path)
            && metadata_path.exists()
        {
            std::fs::remove_file(&metadata_path).map_err(|e| {
                format!(
                    "Failed to remove artifact metadata {}: {e}",
                    metadata_path.display()
                )
            })?;
            summary.removed_target_metadata += 1;
        }
    }

    for metadata_path in all_metadata {
        let Some(archive_path) = artifact_cache_archive_path_for_metadata(&metadata_path) else {
            continue;
        };
        if archive_path.exists() || !metadata_path.exists() {
            continue;
        }
        std::fs::remove_file(&metadata_path).map_err(|e| {
            format!(
                "Failed to remove orphan artifact metadata {}: {e}",
                metadata_path.display()
            )
        })?;
        summary.removed_target_metadata += 1;
    }

    Ok(summary)
}

fn artifact_cache_metadata_path_for_archive(archive_path: &Path) -> Option<PathBuf> {
    let file_name = archive_path.file_name()?.to_str()?;
    let stem = file_name.strip_suffix(".tar.zst")?;
    Some(archive_path.with_file_name(format!("{stem}.json")))
}

fn artifact_cache_archive_path_for_metadata(metadata_path: &Path) -> Option<PathBuf> {
    let file_name = metadata_path.file_name()?.to_str()?;
    let stem = file_name.strip_suffix(".json")?;
    Some(metadata_path.with_file_name(format!("{stem}.tar.zst")))
}

pub(super) fn remove_cached_artifact_files(paths: &ArtifactCachePaths) {
    let _ = std::fs::remove_file(&paths.artifact_path);
    let _ = std::fs::remove_file(&paths.metadata_path);
}

pub(super) fn load_valid_cached_artifact(
    paths: &ArtifactCachePaths,
) -> Result<Option<CachedArtifact>, String> {
    if !paths.artifact_path.exists() || !paths.metadata_path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&paths.metadata_path).map_err(|e| {
        format!(
            "Failed to read cache metadata {}: {e}",
            paths.metadata_path.display()
        )
    })?;
    let metadata: ArtifactCacheMetadata = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Failed to parse cache metadata {}: {e}",
            paths.metadata_path.display()
        )
    })?;

    if metadata.schema_version != ARTIFACT_CACHE_SCHEMA_VERSION {
        return Err(format!(
            "cache schema mismatch (found {}, expected {})",
            metadata.schema_version, ARTIFACT_CACHE_SCHEMA_VERSION
        ));
    }

    let artifact_size = std::fs::metadata(&paths.artifact_path)
        .map_err(|e| {
            format!(
                "Failed to stat cached artifact {}: {e}",
                paths.artifact_path.display()
            )
        })?
        .len();
    if artifact_size != metadata.artifact_size {
        return Err(format!(
            "cached artifact size mismatch (metadata {}, file {})",
            metadata.artifact_size, artifact_size
        ));
    }

    let actual_sha = compute_file_hash(&paths.artifact_path).map_err(|e| {
        format!(
            "Failed to hash cached artifact {}: {e}",
            paths.artifact_path.display()
        )
    })?;
    if actual_sha != metadata.artifact_sha256 {
        return Err("cached artifact checksum mismatch".to_string());
    }

    Ok(Some(CachedArtifact {
        path: paths.artifact_path.clone(),
        size_bytes: artifact_size,
    }))
}

pub(super) fn persist_cached_artifact(
    artifact_temp_path: &Path,
    paths: &ArtifactCachePaths,
    artifact_size: u64,
) -> Result<(), String> {
    if let Some(parent) = paths.artifact_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }

    let artifact_sha = compute_file_hash(artifact_temp_path).map_err(|e| {
        format!(
            "Failed to hash built artifact {}: {e}",
            artifact_temp_path.display()
        )
    })?;
    let metadata = ArtifactCacheMetadata {
        schema_version: ARTIFACT_CACHE_SCHEMA_VERSION,
        artifact_sha256: artifact_sha,
        artifact_size,
    };
    let metadata_bytes = serde_json::to_vec_pretty(&metadata)
        .map_err(|e| format!("Failed to serialize artifact cache metadata: {e}"))?;
    let metadata_temp_path = artifact_cache_temp_path(&paths.metadata_path)?;
    std::fs::write(&metadata_temp_path, metadata_bytes).map_err(|e| {
        format!(
            "Failed to write artifact cache metadata {}: {e}",
            metadata_temp_path.display()
        )
    })?;

    std::fs::rename(artifact_temp_path, &paths.artifact_path).map_err(|e| {
        format!(
            "Failed to move artifact {} to {}: {e}",
            artifact_temp_path.display(),
            paths.artifact_path.display()
        )
    })?;
    if let Err(e) = std::fs::rename(&metadata_temp_path, &paths.metadata_path) {
        let _ = std::fs::remove_file(&paths.artifact_path);
        return Err(format!(
            "Failed to move cache metadata {} to {}: {e}",
            metadata_temp_path.display(),
            paths.metadata_path.display()
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn artifact_cache_separates_descriptions_for_the_same_release_tag() {
        let cache_dir = Path::new("artifacts");
        let original = artifact_cache_paths_for_manifest(
            cache_dir,
            "abc123",
            None,
            br#"{"description":"First"}"#,
        );
        let changed = artifact_cache_paths_for_manifest(
            cache_dir,
            "abc123",
            None,
            br#"{"description":"Second"}"#,
        );
        assert_ne!(original.artifact_path, changed.artifact_path);
        let repeated = artifact_cache_paths_for_manifest(
            cache_dir,
            "abc123",
            None,
            br#"{"description":"First"}"#,
        );
        assert_eq!(original.artifact_path, repeated.artifact_path);
    }

    #[test]
    fn cached_artifact_round_trip_verifies_checksum_and_size() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join("artifacts");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let paths = artifact_cache_paths(&cache_dir, "abc123", None);

        let artifact_tmp = cache_dir.join("artifact.tmp");
        std::fs::write(&artifact_tmp, b"hello artifact").unwrap();
        let size = std::fs::metadata(&artifact_tmp).unwrap().len();
        persist_cached_artifact(&artifact_tmp, &paths, size).unwrap();

        let verified = load_valid_cached_artifact(&paths).unwrap().unwrap();
        assert_eq!(verified.path, paths.artifact_path);
        assert_eq!(verified.size_bytes, size);
    }

    #[test]
    fn cached_artifact_verification_fails_on_checksum_mismatch() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join("artifacts");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let paths = artifact_cache_paths(&cache_dir, "abc123", None);

        std::fs::write(&paths.artifact_path, b"hello artifact").unwrap();
        let bad_metadata = ArtifactCacheMetadata {
            schema_version: ARTIFACT_CACHE_SCHEMA_VERSION,
            artifact_sha256: "deadbeef".to_string(),
            artifact_size: 14,
        };
        std::fs::write(
            &paths.metadata_path,
            serde_json::to_vec_pretty(&bad_metadata).unwrap(),
        )
        .unwrap();

        let err = load_valid_cached_artifact(&paths).unwrap_err();
        assert!(err.contains("checksum mismatch"));
    }

    #[test]
    fn cleanup_local_artifact_cache_prunes_old_artifacts() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join("artifacts");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let old_artifact = cache_dir.join("old-version.tar.zst");
        let old_metadata = cache_dir.join("old-version.json");
        let new_artifact = cache_dir.join("new-version.tar.zst");
        let new_metadata = cache_dir.join("new-version.json");
        std::fs::write(&old_artifact, b"old artifact").unwrap();
        std::fs::write(&old_metadata, b"{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&new_artifact, b"new artifact").unwrap();
        std::fs::write(&new_metadata, b"{}").unwrap();

        let summary = cleanup_local_artifact_cache(&cache_dir, 1).unwrap();
        assert_eq!(
            summary,
            super::super::task_tree::LocalArtifactCacheCleanupSummary {
                removed_target_artifacts: 1,
                removed_target_metadata: 1,
            }
        );

        assert!(!old_artifact.exists());
        assert!(!old_metadata.exists());
        assert!(new_artifact.exists());
        assert!(new_metadata.exists());
    }

    #[test]
    fn cleanup_local_artifact_cache_prunes_target_subdirectories() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join("artifacts");
        let target_dir = cache_dir.join("linux-x86_64-glibc");
        std::fs::create_dir_all(&target_dir).unwrap();

        let old_artifact = target_dir.join("old-version.tar.zst");
        let old_metadata = target_dir.join("old-version.json");
        let new_artifact = target_dir.join("new-version.tar.zst");
        std::fs::write(&old_artifact, b"old").unwrap();
        std::fs::write(&old_metadata, b"{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&new_artifact, b"new").unwrap();

        let summary = cleanup_local_artifact_cache(&cache_dir, 1).unwrap();
        assert_eq!(summary.removed_target_artifacts, 1);
        assert!(!old_artifact.exists());
        assert!(new_artifact.exists());
    }

    #[test]
    fn cleanup_local_artifact_cache_removes_orphan_target_metadata() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join("artifacts");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let artifact = cache_dir.join("live-version.tar.zst");
        let live_metadata = cache_dir.join("live-version.json");
        let orphan_metadata = cache_dir.join("orphan-version.json");
        std::fs::write(&artifact, b"live artifact").unwrap();
        std::fs::write(&live_metadata, b"{}").unwrap();
        std::fs::write(&orphan_metadata, b"{}").unwrap();

        let summary = cleanup_local_artifact_cache(&cache_dir, 10).unwrap();
        assert_eq!(
            summary,
            super::super::task_tree::LocalArtifactCacheCleanupSummary {
                removed_target_artifacts: 0,
                removed_target_metadata: 1,
            }
        );

        assert!(artifact.exists());
        assert!(live_metadata.exists());
        assert!(!orphan_metadata.exists());
    }
}

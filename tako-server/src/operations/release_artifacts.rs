use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::release::{app_release_root, app_root, validate_app_name, validate_release_version};
use crate::socket::Response;

const OLD_RELEASE_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAX_RELEASE_RETENTION_COUNT: usize = 50;

struct ReleasePaths {
    app_root: PathBuf,
    release_path: PathBuf,
    releases_root: PathBuf,
    archive_root: PathBuf,
    shared_logs: PathBuf,
}

impl crate::ServerState {
    pub(crate) async fn prepare_release_upload(&self, app: &str, version: &str) -> Response {
        let paths = match self.release_paths(app, version) {
            Ok(paths) => paths,
            Err(error) => return Response::error(error),
        };
        let active = self
            .app_manager
            .get_app(app)
            .is_some_and(|app| app.version() == version);
        let result =
            tokio::task::spawn_blocking(move || prepare_release_upload_inner(paths, active))
                .await
                .unwrap_or_else(|error| Err(format!("release restoration worker failed: {error}")));
        match result {
            Ok(plan) => Response::ok(plan),
            Err(error) => Response::error(error),
        }
    }

    pub(crate) async fn cleanup_release(&self, app: &str, version: &str) -> Response {
        let lock = self.get_deploy_lock(app).await;
        let _guard = lock.lock().await;
        if self
            .app_manager
            .get_app(app)
            .is_some_and(|current| current.version() == version)
        {
            return Response::error("Cannot remove the active release");
        }
        let protected = match self.release_references(app).await {
            Ok(paths) => paths,
            Err(error) => return Response::error(error),
        };
        match self.release_paths(app, version) {
            Ok(paths) => {
                if protected
                    .iter()
                    .any(|used| used.starts_with(&paths.release_path))
                {
                    return Response::error(
                        "Cannot remove release files referenced by running instances, workers, or Compose containers",
                    );
                }
                let release_path = paths.release_path.clone();
                if let Err(error) =
                    tokio::task::spawn_blocking(move || remove_release(&release_path))
                        .await
                        .unwrap_or_else(|error| Err(error.to_string()))
                {
                    return Response::error(format!(
                        "Failed to remove release {}: {error}",
                        paths.release_path.display()
                    ));
                }
                // A failed redeploy must not destroy an earlier successful release.
                let historical = match self.state_store.release_routing(app, version) {
                    Ok(value) => value.is_some(),
                    Err(error) => {
                        return Response::error(format!("Failed to read release history: {error}"));
                    }
                };
                if !historical && paths.archive_root.exists() {
                    if let Err(error) =
                        tokio::task::spawn_blocking(move || remove_directory(&paths.archive_root))
                            .await
                            .unwrap_or_else(|error| Err(error.to_string()))
                    {
                        return Response::error(format!(
                            "Failed to remove release archive: {error}"
                        ));
                    }
                } else {
                    let _ = std::fs::remove_file(paths.archive_root.join("pending"));
                }
                Response::ok(serde_json::json!({ "status": "removed" }))
            }
            Err(error) => Response::error(error),
        }
    }

    pub(crate) async fn finalize_release(&self, app: &str, version: &str) -> Response {
        match self.finalize_release_inner(app, version).await {
            Ok(()) => Response::ok(serde_json::json!({ "status": "finalized" })),
            Err(error) => Response::error(error),
        }
    }

    pub(crate) async fn check_deploy_space(&self, min_free_bytes: u64) -> Response {
        match available_bytes(&self.runtime_config().data_dir) {
            Ok(available) if available >= min_free_bytes => Response::ok(serde_json::json!({
                "available_bytes": available
            })),
            Ok(available) => Response::error(format!(
                "Insufficient disk space under {}. Required: at least {} bytes. Available: {} bytes.",
                self.runtime_config().data_dir.display(),
                min_free_bytes,
                available
            )),
            Err(error) => Response::error(format!("Failed to check free disk space: {error}")),
        }
    }

    pub(crate) fn store_uploaded_release_artifact(
        &self,
        app: &str,
        version: &str,
        archive_path: &Path,
    ) -> Result<tako_core::ReleaseUploadPlan, String> {
        let paths = self.release_paths(app, version)?;
        std::fs::create_dir_all(&paths.releases_root)
            .map_err(|e| format!("create releases dir {}: {e}", paths.releases_root.display()))?;
        std::fs::create_dir_all(&paths.shared_logs).map_err(|e| {
            format!(
                "create shared logs dir {}: {e}",
                paths.shared_logs.display()
            )
        })?;

        if paths.archive_root.is_dir() {
            let active = self
                .app_manager
                .get_app(app)
                .is_some_and(|app| app.version() == version);
            let (_, preparation_required) = restore_release_artifact(&paths, active)?;
            return Ok(release_upload_plan(
                &paths.release_path,
                false,
                preparation_required,
            ));
        }
        if paths.release_path.exists() {
            return Err(format!(
                "Release {} exists without a retained archive",
                paths.release_path.display()
            ));
        }

        let extracted = tempfile::Builder::new()
            .prefix(".extract-")
            .tempdir_in(&paths.releases_root)
            .map_err(|e| e.to_string())?;
        let temp_release_path = extracted.path();

        let extract_result = (|| {
            crate::extract_zstd_archive(archive_path, temp_release_path)?;
            // Publish the original bytes and listing metadata together. Installed
            // dependencies and runtime-generated files never enter this archive.
            let archives_root = paths.archive_root.parent().ok_or("Invalid archive path")?;
            std::fs::create_dir_all(archives_root).map_err(|e| e.to_string())?;
            let staged = tempfile::Builder::new()
                .prefix(".upload-")
                .tempdir_in(archives_root)
                .map_err(|e| e.to_string())?;
            std::fs::copy(archive_path, staged.path().join("release.tar.zst"))
                .map_err(|e| format!("retain uploaded archive: {e}"))?;
            std::fs::copy(
                temp_release_path.join("app.json"),
                staged.path().join("app.json"),
            )
            .map_err(|e| format!("retain release metadata: {e}"))?;
            std::fs::write(staged.path().join("pending"), []).map_err(|e| e.to_string())?;
            replace_logs_with_shared_link(temp_release_path, &paths.shared_logs)?;
            std::fs::rename(staged.path(), &paths.archive_root)
                .map_err(|e| format!("publish release archive: {e}"))?;
            std::fs::rename(temp_release_path, &paths.release_path).map_err(|e| {
                format!("activate release dir {}: {e}", paths.release_path.display())
            })?;
            Ok::<(), String>(())
        })();

        extract_result?;

        Ok(release_upload_plan(&paths.release_path, true, true))
    }

    pub(crate) async fn restore_release_artifact(
        &self,
        app: &str,
        version: &str,
    ) -> Result<(PathBuf, bool), String> {
        let paths = self.release_paths(app, version)?;
        let active = self
            .app_manager
            .get_app(app)
            .is_some_and(|app| app.version() == version);
        tokio::task::spawn_blocking(move || restore_release_artifact(&paths, active))
            .await
            .map_err(|error| format!("release restoration worker failed: {error}"))?
    }

    pub(super) async fn discard_restored_release(
        &self,
        app: &str,
        version: &str,
        release_preexisted: bool,
        pending_preexisted: bool,
    ) -> Result<(), String> {
        let paths = self.release_paths(app, version)?;
        if !paths.archive_root.join("release.tar.zst").is_file() {
            return Ok(());
        }
        if !release_preexisted {
            let protected = self.release_references(app).await?;
            if !protected
                .iter()
                .any(|used| used.starts_with(&paths.release_path))
            {
                let release_path = paths.release_path.clone();
                tokio::task::spawn_blocking(move || remove_release(&release_path))
                    .await
                    .map_err(|error| error.to_string())??;
            }
        }
        if !pending_preexisted && paths.archive_root.join("pending").exists() {
            std::fs::remove_file(paths.archive_root.join("pending")).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub(crate) async fn finalize_release_inner(
        &self,
        app: &str,
        version: &str,
    ) -> Result<(), String> {
        let lock = self.get_deploy_lock(app).await;
        let _guard = lock.lock().await;
        if let Some(current) = self.app_manager.get_app(app) {
            if current.version() != version {
                return Err("Cannot finalize a release that is not active".into());
            }
        }
        self.finalize_release_locked(app, version).await
    }

    pub(super) async fn finalize_release_locked(
        &self,
        app: &str,
        version: &str,
    ) -> Result<(), String> {
        // A standby's local view cannot finalize or collect the primary's releases.
        if self.runtime.standby {
            return Ok(());
        }
        let paths = self.release_paths(app, version)?;
        if !paths.release_path.is_dir() {
            return Err(format!(
                "Release {} does not exist",
                paths.release_path.display()
            ));
        }
        if !paths.archive_root.join("release.tar.zst").is_file() {
            return Err("Cannot finalize a release without a retained archive".into());
        }

        let current_link = paths.app_root.join("current");
        let temp_link = paths
            .app_root
            .join(format!(".current-{}", nanoid::nanoid!(8)));
        #[cfg(unix)]
        std::os::unix::fs::symlink(&paths.release_path, &temp_link)
            .map_err(|e| format!("create current symlink {}: {e}", temp_link.display()))?;
        #[cfg(not(unix))]
        return Err("release finalization requires Unix symlinks".to_string());

        std::fs::rename(&temp_link, &current_link)
            .map_err(|e| format!("update current symlink {}: {e}", current_link.display()))?;
        if let Err(error) = clear_pending_marker(&paths.archive_root) {
            tracing::warn!(app, %error, "Could not clear release preparation marker; periodic cleanup will retry");
        }
        let cleanup = async {
            let Some(lease) = self.release_cleanup_lease().await? else {
                return Ok(());
            };
            let protected = self.release_references(app).await?;
            tokio::task::spawn_blocking(move || {
                let _lease = lease;
                prune_old_releases(&paths.releases_root, &paths.release_path, &protected);
                compact_releases(&paths.releases_root, &paths.release_path, &protected)
            })
            .await
            .map_err(|error| error.to_string())?
        }
        .await;
        if let Err(error) = cleanup {
            tracing::warn!(app, %error, "Could not compact inactive releases; periodic cleanup will retry");
        }
        Ok(())
    }

    pub(crate) async fn compact_app_releases(&self, app_name: &str) -> Result<(), String> {
        let Some(lease) = self.release_cleanup_lease().await? else {
            return Ok(());
        };
        // Housekeeping yields to operators and bounds the entire Docker inspection,
        // rather than allowing a separate timeout for every container.
        let Ok(management) = self.management_lock.clone().try_lock_owned() else {
            return Ok(());
        };
        let lock = self.get_deploy_lock(app_name).await;
        let Ok(guard) = lock.try_lock_owned() else {
            return Ok(());
        };
        let Some(app) = self.app_manager.get_app(app_name) else {
            return Ok(());
        };
        let paths = self.release_paths(app_name, &app.version())?;
        if std::fs::read_link(paths.app_root.join("current"))
            .is_ok_and(|current| current == paths.release_path)
        {
            clear_pending_marker(&paths.archive_root)?;
        }
        let protected =
            tokio::time::timeout(Duration::from_secs(1), self.release_references(app_name))
                .await
                .map_err(|_| {
                    "Release reference inspection timed out; deferring compaction".to_string()
                })??;
        tokio::task::spawn_blocking(move || {
            let _guards = (lease, management, guard);
            compact_releases(&paths.releases_root, &paths.release_path, &protected)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    async fn release_cleanup_lease(
        &self,
    ) -> Result<Option<crate::state_store::DrainLease>, String> {
        if self.runtime.standby
            || self.is_draining()
            || *self.server_mode.read().await != tako_core::UpgradeMode::Normal
        {
            return Ok(None);
        }
        // A companion may still serve an older release unknown to this process.
        crate::state_store::DrainLease::try_standby_in(&self.runtime.data_dir, true)
            .map_err(|error| error.to_string())
    }

    async fn release_references(
        &self,
        app: &str,
    ) -> Result<std::collections::BTreeSet<PathBuf>, String> {
        let Some(app) = self.app_manager.get_app(app) else {
            return Ok(Default::default());
        };
        let config = app.config.read().clone();
        let mut paths = self.workflows.release_paths(&app.name());
        for instance in app.get_instances() {
            paths.insert(app_release_root(
                &self.runtime.data_dir,
                &app.name(),
                instance.build_version(),
            ));
        }
        if config.compose.is_some() {
            let runtime = crate::compose_runtime::for_config(&config)?;
            paths.extend(
                runtime
                    .containers()
                    .await?
                    .into_iter()
                    .filter_map(|container| container.release),
            );
        }
        Ok(paths)
    }

    fn release_paths(&self, app: &str, version: &str) -> Result<ReleasePaths, String> {
        validate_app_name(app)?;
        validate_release_version(version)?;
        let app_root = app_root(&self.runtime_config().data_dir, app);
        let releases_root = app_root.join("releases");
        let release_path = app_release_root(&self.runtime_config().data_dir, app, version);
        let archive_root = app_root.join("archives").join(version);
        let shared_logs = app_root.join("shared").join("logs");
        Ok(ReleasePaths {
            app_root,
            release_path,
            releases_root,
            archive_root,
            shared_logs,
        })
    }
}

fn clear_pending_marker(archive_root: &Path) -> Result<(), String> {
    std::fs::remove_file(archive_root.join("pending"))
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        })
        .map_err(|error| format!("finalize release archive: {error}"))
}

pub(crate) fn sweep_release_staging_dirs(data_dir: &Path, mode: super::RecoveryMode) {
    // Only cold-start recovery holds an exclusive data-dir lease. A successor
    // or standby may share this directory with an upload that is still running.
    if mode != super::RecoveryMode::Primary {
        return;
    }
    let directories = |root: &Path| {
        let entries = match std::fs::symlink_metadata(root) {
            Ok(metadata) if metadata.is_dir() => std::fs::read_dir(root),
            Ok(_) => return None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => Err(error),
        };
        match entries {
            Ok(entries) => Some(entries.filter_map(|entry| match entry {
                Ok(entry) if entry.file_type().is_ok_and(|kind| kind.is_dir()) => {
                    Some(entry.path())
                }
                _ => None,
            })),
            Err(error) => {
                tracing::warn!(path = %root.display(), %error, "Could not inspect release staging directories");
                None
            }
        }
    };
    // Interrupted first deployments may have no durable app record yet.
    for app in directories(&data_dir.join("apps")).into_iter().flatten() {
        for environment in directories(&app).into_iter().flatten() {
            for (subdir, prefixes) in [
                ("releases", [".extract-", ".remove-"]),
                ("archives", [".upload-", ".remove-"]),
            ] {
                for path in directories(&environment.join(subdir)).into_iter().flatten() {
                    if path.file_name().is_some_and(|name| {
                        prefixes
                            .iter()
                            .any(|prefix| name.as_encoded_bytes().starts_with(prefix.as_bytes()))
                    }) && let Err(error) = std::fs::remove_dir_all(&path)
                    {
                        tracing::warn!(path = %path.display(), %error, "Could not remove abandoned release staging directory");
                    }
                }
            }
        }
    }
}

fn prepare_release_upload_inner(
    paths: ReleasePaths,
    active: bool,
) -> Result<tako_core::ReleaseUploadPlan, String> {
    std::fs::create_dir_all(&paths.releases_root)
        .map_err(|e| format!("create releases dir {}: {e}", paths.releases_root.display()))?;
    std::fs::create_dir_all(&paths.shared_logs).map_err(|e| {
        format!(
            "create shared logs dir {}: {e}",
            paths.shared_logs.display()
        )
    })?;
    if paths.archive_root.is_dir() {
        let (_, preparation_required) = restore_release_artifact(&paths, active)?;
        return Ok(release_upload_plan(
            &paths.release_path,
            false,
            preparation_required,
        ));
    }
    Ok(release_upload_plan(&paths.release_path, true, true))
}

fn restore_release_artifact(paths: &ReleasePaths, active: bool) -> Result<(PathBuf, bool), String> {
    let archive = paths.archive_root.join("release.tar.zst");
    if !archive.is_file() {
        return Err(format!("Release archive {} not found", archive.display()));
    }
    let pending = paths.archive_root.join("pending").exists();
    std::fs::write(paths.archive_root.join("pending"), [])
        .map_err(|e| format!("mark release preparation: {e}"))?;
    if paths.release_path.is_dir() {
        return Ok((paths.release_path.clone(), pending && !active));
    }
    // Interrupted compaction may have left private Compose inputs behind.
    tako_compose::remove_release_preparations(
        &paths.release_path,
        &paths.app_root.join("data/app"),
    )?;
    std::fs::create_dir_all(&paths.releases_root).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&paths.shared_logs).map_err(|e| e.to_string())?;
    let staged = tempfile::Builder::new()
        .prefix(".extract-")
        .tempdir_in(&paths.releases_root)
        .map_err(|e| e.to_string())?;
    crate::extract_zstd_archive(&archive, staged.path())?;
    replace_logs_with_shared_link(staged.path(), &paths.shared_logs)?;
    std::fs::rename(staged.path(), &paths.release_path)
        .map_err(|e| format!("restore release directory: {e}"))?;
    Ok((paths.release_path.clone(), true))
}

fn release_upload_plan(
    release_path: &Path,
    upload_required: bool,
    preparation_required: bool,
) -> tako_core::ReleaseUploadPlan {
    tako_core::ReleaseUploadPlan {
        path: release_path.to_string_lossy().to_string(),
        upload_required,
        preparation_required,
    }
}

fn replace_logs_with_shared_link(release_path: &Path, shared_logs: &Path) -> Result<(), String> {
    let logs_path = release_path.join("logs");
    if let Ok(metadata) = std::fs::symlink_metadata(&logs_path) {
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            std::fs::remove_dir_all(&logs_path)
                .map_err(|e| format!("remove release logs dir {}: {e}", logs_path.display()))?;
        } else {
            std::fs::remove_file(&logs_path)
                .map_err(|e| format!("remove release logs path {}: {e}", logs_path.display()))?;
        }
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(shared_logs, &logs_path)
        .map_err(|e| format!("link release logs {}: {e}", logs_path.display()))?;
    #[cfg(not(unix))]
    return Err("release log linking requires Unix symlinks".to_string());
    Ok(())
}

pub(super) fn release_entries(
    releases_root: &Path,
) -> Result<std::collections::BTreeMap<String, PathBuf>, std::io::Error> {
    let mut releases = std::collections::BTreeMap::new();
    let entries = match std::fs::read_dir(releases_root.with_file_name("archives")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(releases),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let Some(version) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !entry.file_type()?.is_dir()
            || version.starts_with(".upload-")
            || validate_release_version(&version).is_err()
            || !entry.path().join("release.tar.zst").is_file()
        {
            continue;
        }
        releases.insert(version, entry.path());
    }
    Ok(releases)
}

fn compact_releases(
    releases_root: &Path,
    active_release: &Path,
    protected: &std::collections::BTreeSet<PathBuf>,
) -> Result<(), String> {
    let entries = match std::fs::read_dir(releases_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let version = entry.file_name();
        let Some(version) = version.to_str() else {
            continue;
        };
        if version.starts_with(".remove-") && entry.file_type().map_err(|e| e.to_string())?.is_dir()
        {
            std::fs::remove_dir_all(entry.path()).map_err(|e| e.to_string())?;
            continue;
        }
        if version.starts_with(".extract-")
            || version.starts_with(".upload-")
            || validate_release_version(version).is_err()
            || !entry.file_type().map_err(|e| e.to_string())?.is_dir()
        {
            continue;
        }
        let path = entry.path();
        let metadata_root = releases_root.with_file_name("archives").join(version);
        if path == active_release
            || protected.iter().any(|used| used.starts_with(&path))
            || metadata_root.join("pending").exists()
        {
            continue;
        }
        remove_release(&path)?;
    }
    Ok(())
}

fn prune_old_releases(
    releases_root: &Path,
    active_release: &Path,
    protected: &std::collections::BTreeSet<PathBuf>,
) {
    let Ok(entries) = release_entries(releases_root) else {
        return;
    };
    let now = SystemTime::now();
    let mut retained = Vec::new();
    for (version, metadata_root) in entries {
        let path = releases_root.join(version);
        if path == active_release
            || protected.iter().any(|used| used.starts_with(&path))
            || std::fs::metadata(metadata_root.join("pending"))
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| {
                    now.duration_since(modified)
                        .map_or(true, |age| age <= OLD_RELEASE_RETENTION)
                })
        {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(metadata_root.join("release.tar.zst")) else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now
            .duration_since(modified)
            .is_ok_and(|age| age > OLD_RELEASE_RETENTION)
        {
            let _ = remove_retained_release(&path);
        } else {
            retained.push((path, modified));
        }
    }

    retained.sort_by(|(a_path, a_modified), (b_path, b_modified)| {
        b_modified.cmp(a_modified).then_with(|| b_path.cmp(a_path))
    });

    let non_active_limit = if active_release.is_dir() {
        MAX_RELEASE_RETENTION_COUNT.saturating_sub(1)
    } else {
        MAX_RELEASE_RETENTION_COUNT
    };
    for (path, _) in retained.into_iter().skip(non_active_limit) {
        let _ = remove_retained_release(&path);
    }
}

fn remove_retained_release(path: &Path) -> Result<(), String> {
    remove_release(path)?;
    let archive = path
        .parent()
        .and_then(Path::parent)
        .ok_or("Invalid release path")?
        .join("archives")
        .join(path.file_name().ok_or("Invalid release version")?);
    if archive.exists() {
        remove_directory(&archive)?;
    }
    Ok(())
}

fn remove_release(path: &Path) -> Result<(), String> {
    let app_root = path
        .parent()
        .and_then(Path::parent)
        .ok_or("Invalid release path")?;
    let removed = path
        .exists()
        .then(|| stage_directory_removal(path))
        .transpose()?
        .map(tempfile::TempDir::keep);
    tako_compose::remove_release_preparations(path, &app_root.join("data/app"))?;
    if let Some(removed) = removed {
        std::fs::remove_dir_all(removed).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn remove_directory(path: &Path) -> Result<(), String> {
    stage_directory_removal(path)?
        .close()
        .map_err(|error| error.to_string())
}

fn stage_directory_removal(path: &Path) -> Result<tempfile::TempDir, String> {
    let parent = path.parent().ok_or("Invalid release directory")?;
    let removed = tempfile::Builder::new()
        .prefix(".remove-")
        .tempdir_in(parent)
        .map_err(|error| error.to_string())?;
    // Never expose a partially deleted tree at a reusable release/archive path.
    std::fs::rename(path, removed.path().join("tree")).map_err(|error| error.to_string())?;
    Ok(removed)
}

#[cfg(unix)]
pub(crate) fn available_bytes(path: &Path) -> Result<u64, String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("path contains interior nul: {}", path.display()))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize))
}

#[cfg(not(unix))]
pub(crate) fn available_bytes(_path: &Path) -> Result<u64, String> {
    Err("disk space checks require Unix".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::FileTime;

    fn create_release(root: &Path, name: &str, modified: SystemTime) -> PathBuf {
        let path = root.join(name);
        std::fs::create_dir_all(&path).expect("create release dir");
        filetime::set_file_mtime(&path, FileTime::from_system_time(modified))
            .expect("set release mtime");
        path
    }

    fn release_dirs(root: &Path) -> usize {
        std::fs::read_dir(root)
            .expect("read releases root")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .count()
    }

    fn create_retained_release(root: &Path, name: &str, modified: SystemTime) -> PathBuf {
        let release = create_release(root, name, modified);
        let archive_root = create_release(&root.with_file_name("archives"), name, modified);
        let archive = archive_root.join("release.tar.zst");
        std::fs::write(&archive, b"original").unwrap();
        filetime::set_file_mtime(archive, FileTime::from_system_time(modified)).unwrap();
        release
    }

    #[test]
    fn archive_retention_counts_each_release_once_and_removes_expired_archives() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("releases");
        let archives = temp.path().join("archives");
        let now = SystemTime::now();
        let active = create_retained_release(&root, "active", now);
        for index in 0..55 {
            let version = format!("r{index:03}");
            let archive_root = create_release(&archives, &version, now);
            let archive = archive_root.join("release.tar.zst");
            std::fs::write(&archive, b"original").unwrap();
            filetime::set_file_mtime(
                &archive,
                FileTime::from_system_time(now - Duration::from_secs(index)),
            )
            .unwrap();
            if index % 2 == 0 {
                create_release(&root, &version, now);
            }
        }
        let expired = create_release(&archives, "expired", now);
        std::fs::write(expired.join("release.tar.zst"), b"expired").unwrap();
        filetime::set_file_mtime(
            expired.join("release.tar.zst"),
            FileTime::from_system_time(now - OLD_RELEASE_RETENTION * 2),
        )
        .unwrap();
        prune_old_releases(&root, &active, &Default::default());
        assert_eq!(
            release_entries(&root).unwrap().len(),
            MAX_RELEASE_RETENTION_COUNT
        );
        assert!(!expired.exists());
        assert!(archives.join("r048/release.tar.zst").is_file());
        assert!(!archives.join("r049").exists());
        assert!(!root.join("r050").exists());
    }

    #[test]
    fn compaction_waits_for_release_references_and_keeps_original_archives() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("releases");
        let active = create_release(&root, "active", SystemTime::now());
        let old = create_release(&root, "old", SystemTime::now());
        let unarchived = create_release(&root, "unarchived", SystemTime::now());
        std::fs::write(
            unarchived.join("release.tar.zst"),
            b"app-owned file, not a retained upload",
        )
        .unwrap();
        let archive_root = create_release(&temp.path().join("archives"), "old", SystemTime::now());
        std::fs::write(archive_root.join("release.tar.zst"), b"original").unwrap();
        let protected = std::collections::BTreeSet::from([old.join("app")]);
        compact_releases(&root, &active, &protected).unwrap();
        assert!(old.is_dir());
        compact_releases(&root, &active, &Default::default()).unwrap();
        assert!(!old.exists());
        assert!(active.is_dir());
        assert!(!unarchived.exists());
        assert_eq!(
            std::fs::read(archive_root.join("release.tar.zst")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn startup_sweeps_abandoned_release_staging_without_touching_retained_releases() {
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("apps/unregistered/production");
        let staged = [
            app.join("releases/.extract-interrupted"),
            app.join("archives/.upload-interrupted"),
            app.join("releases/.remove-interrupted"),
            app.join("archives/.remove-interrupted"),
        ];
        let retained = [
            app.join("releases/v1"),
            app.join("archives/v1"),
            app.join("archives/pending"),
        ];
        for path in staged.iter().chain(&retained) {
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(path.join("payload"), b"unchanged").unwrap();
        }
        std::fs::write(app.join("archives/pending/pending"), []).unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(outside.join("production/releases/.extract-keep")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, temp.path().join("apps/redirected")).unwrap();
        for mode in [
            super::super::RecoveryMode::Standby,
            super::super::RecoveryMode::Successor,
        ] {
            sweep_release_staging_dirs(temp.path(), mode);
            assert!(staged.iter().all(|path| path.is_dir()));
        }
        sweep_release_staging_dirs(temp.path(), super::super::RecoveryMode::Primary);
        assert!(staged.iter().all(|path| !path.exists()));
        for path in retained {
            assert_eq!(std::fs::read(path.join("payload")).unwrap(), b"unchanged");
        }
        assert!(app.join("archives/pending/pending").is_file());
        assert!(outside.join("production/releases/.extract-keep").is_dir());
    }

    #[test]
    fn prune_old_releases_caps_total_release_count() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = &temp.path().join("releases");
        let now = SystemTime::now();
        let active = create_retained_release(root, "r054", now - Duration::from_secs(54));

        for index in 0..54 {
            create_retained_release(
                root,
                &format!("r{index:03}"),
                now - Duration::from_secs(index),
            );
        }

        prune_old_releases(root, &active, &Default::default());

        assert_eq!(release_dirs(root), MAX_RELEASE_RETENTION_COUNT);
        assert!(active.is_dir());
        assert!(root.join("r048").is_dir());
        assert!(!root.join("r049").exists());
        assert!(!root.join("r053").exists());
    }

    #[test]
    fn prune_keeps_release_files_still_mounted_by_unchanged_compose_services() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("releases");
        let now = SystemTime::now();
        let active = create_retained_release(&root, "current", now);
        let referenced = create_retained_release(
            &root,
            "referenced",
            now - OLD_RELEASE_RETENTION - Duration::from_secs(1),
        );
        let obsolete = create_retained_release(
            &root,
            "obsolete",
            now - OLD_RELEASE_RETENTION - Duration::from_secs(1),
        );
        let private = temp.path().join("compose");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        for release in [&referenced, &obsolete] {
            let preparation = private.join(release.file_name().unwrap());
            std::fs::create_dir(&preparation).unwrap();
            std::fs::set_permissions(&preparation, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(
                preparation.join("release.json"),
                serde_json::to_vec(&release.join("app")).unwrap(),
            )
            .unwrap();
            std::fs::write(preparation.join("source.txt"), "mounted input").unwrap();
        }
        let protected = std::collections::BTreeSet::from([referenced.join("app")]);
        prune_old_releases(&root, &active, &protected);
        assert!(referenced.is_dir());
        assert!(private.join("referenced/source.txt").is_file());
        assert!(!obsolete.exists());
        assert!(!private.join("obsolete").exists());
        prune_old_releases(&root, &active, &Default::default());
        assert!(!referenced.exists());
        assert!(!private.join("referenced").exists());
    }

    #[test]
    fn prune_old_releases_removes_non_active_releases_by_age() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = &temp.path().join("releases");
        let now = SystemTime::now();
        let active = create_retained_release(root, "active", now - OLD_RELEASE_RETENTION * 2);
        let expired = create_retained_release(root, "expired", now - OLD_RELEASE_RETENTION * 2);
        let fresh = create_retained_release(root, "fresh", now - Duration::from_secs(60));

        prune_old_releases(root, &active, &Default::default());

        assert!(active.is_dir());
        assert!(fresh.is_dir());
        assert!(!expired.exists());
    }
}

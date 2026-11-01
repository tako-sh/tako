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
    shared_logs: PathBuf,
}

impl crate::ServerState {
    pub(crate) async fn prepare_release_upload(&self, app: &str, version: &str) -> Response {
        match self.prepare_release_upload_inner(app, version) {
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
        let protected = match self.compose_release_references(app).await {
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
                        "Cannot remove release files referenced by Compose containers",
                    );
                }
                if let Err(error) = remove_release(&paths.release_path) {
                    return Response::error(format!(
                        "Failed to remove release {}: {error}",
                        paths.release_path.display()
                    ));
                }
                Response::ok(serde_json::json!({ "status": "removed" }))
            }
            Err(error) => Response::error(error),
        }
    }

    pub(crate) async fn finalize_release(&self, app: &str, version: &str) -> Response {
        match self.finalize_release_inner(app, version).await {
            Ok(()) => {
                let backup = match self.backup_after_deploy(app).await {
                    Some(Ok(info)) => serde_json::json!({
                        "status": "created",
                        "id": info.id,
                        "size_bytes": info.size_bytes
                    }),
                    Some(Err(error)) => {
                        tracing::warn!(app, "Post-deploy backup failed: {error}");
                        serde_json::json!({
                            "status": "failed",
                            "message": error
                        })
                    }
                    None => serde_json::json!({
                        "status": "disabled"
                    }),
                };
                Response::ok(serde_json::json!({ "status": "finalized", "backup": backup }))
            }
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

        if paths.release_path.is_dir() {
            return Ok(release_upload_plan(&paths.release_path, false));
        }

        let temp_release_path = paths.release_path.with_file_name(format!(
            "{}.uploading-{}",
            version,
            nanoid::nanoid!(8)
        ));
        if temp_release_path.exists() {
            std::fs::remove_dir_all(&temp_release_path).map_err(|e| {
                format!(
                    "remove stale upload dir {}: {e}",
                    temp_release_path.display()
                )
            })?;
        }

        let extract_result = (|| {
            crate::extract_zstd_archive(archive_path, &temp_release_path)?;
            replace_logs_with_shared_link(&temp_release_path, &paths.shared_logs)?;
            std::fs::rename(&temp_release_path, &paths.release_path).map_err(|e| {
                format!("activate release dir {}: {e}", paths.release_path.display())
            })?;
            Ok::<(), String>(())
        })();

        if extract_result.is_err() {
            let _ = std::fs::remove_dir_all(&temp_release_path);
        }
        extract_result?;

        Ok(release_upload_plan(&paths.release_path, true))
    }

    fn prepare_release_upload_inner(
        &self,
        app: &str,
        version: &str,
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

        Ok(release_upload_plan(
            &paths.release_path,
            !paths.release_path.is_dir(),
        ))
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
        let protected = self.compose_release_references(app).await?;
        let paths = self.release_paths(app, version)?;
        if !paths.release_path.is_dir() {
            return Err(format!(
                "Release {} does not exist",
                paths.release_path.display()
            ));
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
        prune_old_releases(&paths.releases_root, &paths.release_path, &protected);
        Ok(())
    }

    async fn compose_release_references(
        &self,
        app: &str,
    ) -> Result<std::collections::BTreeSet<PathBuf>, String> {
        let Some(app) = self.app_manager.get_app(app) else {
            return Ok(Default::default());
        };
        let config = app.config.read().clone();
        if config.compose.is_none() {
            return Ok(Default::default());
        }
        let runtime = crate::compose_runtime::for_config(&config)?;
        Ok(runtime
            .containers()
            .await?
            .into_iter()
            .filter_map(|container| container.release)
            .collect())
    }

    fn release_paths(&self, app: &str, version: &str) -> Result<ReleasePaths, String> {
        validate_app_name(app)?;
        validate_release_version(version)?;
        let app_root = app_root(&self.runtime_config().data_dir, app);
        let releases_root = app_root.join("releases");
        let release_path = app_release_root(&self.runtime_config().data_dir, app, version);
        let shared_logs = app_root.join("shared").join("logs");
        Ok(ReleasePaths {
            app_root,
            release_path,
            releases_root,
            shared_logs,
        })
    }
}

fn release_upload_plan(release_path: &Path, upload_required: bool) -> tako_core::ReleaseUploadPlan {
    tako_core::ReleaseUploadPlan {
        path: release_path.to_string_lossy().to_string(),
        upload_required,
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

fn prune_old_releases(
    releases_root: &Path,
    active_release: &Path,
    protected: &std::collections::BTreeSet<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(releases_root) else {
        return;
    };
    let now = SystemTime::now();
    let mut retained = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path == active_release
            || !path.is_dir()
            || protected.iter().any(|used| used.starts_with(&path))
        {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now
            .duration_since(modified)
            .is_ok_and(|age| age > OLD_RELEASE_RETENTION)
        {
            let _ = remove_release(&path);
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
        let _ = remove_release(&path);
    }
}

fn remove_release(path: &Path) -> Result<(), String> {
    let app_root = path
        .parent()
        .and_then(Path::parent)
        .ok_or("Invalid release path")?;
    tako_compose::remove_release_preparations(path, &app_root.join("data/app"))?;
    if path.exists() {
        std::fs::remove_dir_all(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(unix)]
fn available_bytes(path: &Path) -> Result<u64, String> {
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
fn available_bytes(_path: &Path) -> Result<u64, String> {
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

    #[test]
    fn prune_old_releases_caps_total_release_count() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let now = SystemTime::now();
        let active = create_release(root, "r054", now - Duration::from_secs(54));

        for index in 0..54 {
            create_release(
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
        let active = create_release(&root, "current", now);
        let referenced = create_release(
            &root,
            "referenced",
            now - OLD_RELEASE_RETENTION - Duration::from_secs(1),
        );
        let obsolete = create_release(
            &root,
            "obsolete",
            now - OLD_RELEASE_RETENTION - Duration::from_secs(1),
        );
        let private = temp.path().join("data/compose");
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
        let root = temp.path();
        let now = SystemTime::now();
        let active = create_release(root, "active", now - OLD_RELEASE_RETENTION * 2);
        let expired = create_release(root, "expired", now - OLD_RELEASE_RETENTION * 2);
        let fresh = create_release(root, "fresh", now - Duration::from_secs(60));

        prune_old_releases(root, &active, &Default::default());

        assert!(active.is_dir());
        assert!(fresh.is_dir());
        assert!(!expired.exists());
    }
}

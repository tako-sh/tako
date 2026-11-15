use std::path::Path;

use sha2::{Digest, Sha256};
use tako_spawn::{CgroupAssignment, ProcessIsolation, UserIds};

#[cfg(test)]
pub(crate) mod fixture;
#[cfg(any(target_os = "linux", test))]
mod provision;
#[cfg(target_os = "linux")]
pub(crate) use provision::run_helper;

#[cfg(target_os = "linux")]
pub(crate) fn provision_app_data(data_dir: &Path, app_id: &str) -> Result<(), String> {
    provision::request(data_dir, app_id, None)
}

const APP_USER_PREFIX: &str = "tako-";
const SHARED_APP_GROUP: &str = "tako-app";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppUnixIdentity {
    pub(crate) user_name: String,
    pub(crate) ids: UserIds,
}

pub(crate) fn authorize_internal_socket_app(app: &str, peer_uid: u32) -> Result<(), String> {
    if cfg!(not(target_os = "linux")) && !crate::unix::is_root() {
        return if peer_uid == unsafe { libc::geteuid() } {
            Ok(())
        } else {
            Err(format!("internal socket app mismatch for {app}"))
        };
    }
    let user_name = app_unix_user_name(app);
    let Some((uid, _)) = crate::unix::lookup_user_ids(&user_name)
        .map_err(|error| format!("Failed to resolve {user_name}: {error}"))?
    else {
        return Err(format!("unknown app identity for {app}"));
    };
    if uid == peer_uid {
        Ok(())
    } else {
        Err(format!("internal socket app mismatch for {app}"))
    }
}

pub(crate) fn app_unix_user_name(app_id: &str) -> String {
    let digest = Sha256::digest(app_id.as_bytes());
    let hex = hex::encode(digest);
    format!("{APP_USER_PREFIX}{}", &hex[..27])
}

pub(crate) fn app_process_isolation(
    data_dir: &Path,
    app_id: &str,
) -> Result<ProcessIsolation, String> {
    #[cfg(all(test, target_os = "linux"))]
    if fixture::contains(data_dir) {
        return Ok(ProcessIsolation {
            parent_death_signal: app_child_parent_death_signal(),
            umask: Some(tako_spawn::APP_DATA_UMASK),
            resource_limits: tako_spawn::ResourceLimits {
                address_space_bytes: None,
                ..Default::default()
            },
            ..Default::default()
        });
    }
    let mut isolation = ProcessIsolation {
        parent_death_signal: app_child_parent_death_signal(),
        umask: Some(tako_spawn::APP_DATA_UMASK),
        ..ProcessIsolation::default()
    };

    if cfg!(not(target_os = "linux")) && !crate::unix::is_root() {
        isolation.resource_limits = tako_spawn::ResourceLimits {
            open_files: None,
            processes: None,
            address_space_bytes: None,
        };
        return Ok(isolation);
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        provision::validate_service_context(data_dir)?;
        let membership = Path::new("/sys/fs/cgroup/tako-apps")
            .join(app_unix_user_name(app_id))
            .join("cgroup.procs");
        if std::fs::metadata(membership).map_or(true, |metadata| metadata.uid() != 0) {
            provision::request(data_dir, app_id, None)?;
        }
    }
    let identity = ensure_app_unix_identity(app_id)?;
    isolation.user = Some(identity.ids);
    #[cfg(target_os = "linux")]
    {
        isolation.cgroup = Some(CgroupAssignment {
            path: Path::new("/sys/fs/cgroup/tako-apps").join(app_unix_user_name(app_id)),
        });
        isolation.resource_limits.address_space_bytes = None;
    }
    #[cfg(not(target_os = "linux"))]
    {
        isolation.cgroup = prepare_app_cgroup(data_dir, app_id).ok();
    }
    Ok(isolation)
}

pub(crate) fn prepare_app_filesystem_isolation(
    data_dir: &Path,
    app_id: &str,
    release_path: Option<&Path>,
    data_paths: &crate::release::AppRuntimeDataPaths,
) -> Result<Option<AppUnixIdentity>, String> {
    prepare_app_directory_modes(data_dir, app_id, data_paths, release_path)?;

    #[cfg(all(test, target_os = "linux"))]
    if fixture::contains(data_dir) {
        provision::request(data_dir, app_id, release_path)?;
        return Ok(None);
    }

    if cfg!(not(target_os = "linux")) && !crate::unix::is_root() {
        return Ok(None);
    }

    #[cfg(target_os = "linux")]
    {
        provision::request(data_dir, app_id, release_path)?;
        return ensure_app_unix_identity(app_id).map(Some);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let identity = ensure_app_unix_identity(app_id)?;
        apply_app_directory_ownership(data_dir, app_id, &identity, data_paths, release_path)?;
        Ok(Some(identity))
    }
}

fn prepare_app_directory_modes(
    data_dir: &Path,
    app_id: &str,
    data_paths: &crate::release::AppRuntimeDataPaths,
    release_path: Option<&Path>,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let app_root = crate::release::app_root(data_dir, app_id);
    std::fs::create_dir_all(&app_root)
        .map_err(|e| format!("create app root {}: {e}", app_root.display()))?;
    set_mode(&app_root, 0o750)?;

    let releases_root = app_root.join("releases");
    std::fs::create_dir_all(&releases_root).map_err(|e| e.to_string())?;
    set_mode(&releases_root, 0o750)?;
    let shared_root = app_root.join("shared");
    std::fs::create_dir_all(&shared_root).map_err(|e| e.to_string())?;
    set_mode(&shared_root, 0o750)?;
    let shared_logs = shared_root.join("logs");
    if shared_logs.exists() {
        set_mode(&shared_logs, 0o2770)?;
    }
    if let Some(release_path) = release_path {
        set_mode(release_path, 0o750)?;
    }
    set_mode(&data_paths.root, 0o710)?;
    set_mode(&data_paths.app, 0o2770)?;
    set_mode(&data_paths.tako, 0o700)?;

    fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("set permissions {}: {e}", path.display()))
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn apply_app_directory_ownership(
    data_dir: &Path,
    app_id: &str,
    identity: &AppUnixIdentity,
    data_paths: &crate::release::AppRuntimeDataPaths,
    release_path: Option<&Path>,
) -> Result<(), String> {
    let app_root = crate::release::app_root(data_dir, app_id);
    crate::unix::chown_path(&app_root, 0, identity.ids.gid)
        .map_err(|e| format!("set app root owner {}: {e}", app_root.display()))?;
    let releases_root = app_root.join("releases");
    if releases_root.exists() {
        crate::unix::chown_path(&releases_root, 0, identity.ids.gid)
            .map_err(|e| format!("set releases root owner {}: {e}", releases_root.display()))?;
    }
    let shared_root = app_root.join("shared");
    if shared_root.exists() {
        crate::unix::chown_path(&shared_root, 0, identity.ids.gid)
            .map_err(|e| format!("set shared root owner {}: {e}", shared_root.display()))?;
    }
    let shared_logs = shared_root.join("logs");
    if shared_logs.exists() {
        chown_path_tree(&shared_logs, identity.ids.uid, identity.ids.gid)
            .map_err(|e| format!("set shared logs owner {}: {e}", shared_logs.display()))?;
    }
    if let Some(release_path) = release_path {
        chown_path_tree(release_path, identity.ids.uid, identity.ids.gid)
            .map_err(|e| format!("set release owner {}: {e}", release_path.display()))?;
    }
    chown_path_tree(&data_paths.app, identity.ids.uid, identity.ids.gid)
        .map_err(|e| format!("set app data owner {}: {e}", data_paths.app.display()))?;
    crate::unix::chown_path(&data_paths.root, 0, identity.ids.gid)
        .map_err(|e| format!("set app data root owner {}: {e}", data_paths.root.display()))?;
    crate::unix::chown_path(&data_paths.tako, 0, 0)
        .map_err(|e| format!("set internal data owner {}: {e}", data_paths.tako.display()))?;
    Ok(())
}

fn ensure_app_unix_identity(app_id: &str) -> Result<AppUnixIdentity, String> {
    let user_name = app_unix_user_name(app_id);
    let shared_gid = ensure_shared_app_group()?;
    if let Some((uid, gid)) = crate::unix::lookup_user_ids(&user_name)
        .map_err(|e| format!("Failed to resolve {user_name}: {e}"))?
    {
        return Ok(AppUnixIdentity {
            user_name,
            ids: UserIds {
                uid,
                gid,
                supplementary_gids: vec![shared_gid],
            },
        });
    }

    if !crate::unix::is_root() {
        return Err(format!("app identity {user_name} has not been provisioned"));
    }
    create_app_user(&user_name, shared_gid)?;
    let (uid, gid) = crate::unix::lookup_user_ids(&user_name)
        .map_err(|e| format!("Failed to resolve created user {user_name}: {e}"))?
        .ok_or_else(|| format!("Created user {user_name} was not found"))?;
    Ok(AppUnixIdentity {
        user_name,
        ids: UserIds {
            uid,
            gid,
            supplementary_gids: vec![shared_gid],
        },
    })
}

fn ensure_shared_app_group() -> Result<u32, String> {
    if let Some(gid) = crate::unix::lookup_group_id(SHARED_APP_GROUP)
        .map_err(|e| format!("Failed to resolve {SHARED_APP_GROUP} group: {e}"))?
    {
        return Ok(gid);
    }

    #[cfg(target_os = "linux")]
    {
        let status = std::process::Command::new("groupadd")
            .args(["--system", SHARED_APP_GROUP])
            .status()
            .map_err(|e| format!("create shared app group {SHARED_APP_GROUP}: {e}"))?;
        if !status.success() {
            return Err(format!(
                "create shared app group {SHARED_APP_GROUP}: {status}"
            ));
        }
        return crate::unix::lookup_group_id(SHARED_APP_GROUP)
            .map_err(|e| format!("Failed to resolve created group {SHARED_APP_GROUP}: {e}"))?
            .ok_or_else(|| format!("Created group {SHARED_APP_GROUP} was not found"));
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(format!(
            "shared app group {SHARED_APP_GROUP} must exist before root can spawn app processes"
        ))
    }
}

fn create_app_user(user_name: &str, _shared_gid: u32) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let status = std::process::Command::new("useradd")
            .args([
                "--system",
                "--user-group",
                "--no-create-home",
                "--home-dir",
                "/nonexistent",
                "--shell",
                "/usr/sbin/nologin",
                "--groups",
                SHARED_APP_GROUP,
                user_name,
            ])
            .status()
            .map_err(|e| format!("create app user {user_name}: {e}"))?;
        if !status.success() {
            return Err(format!("create app user {user_name}: {status}"));
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(format!(
            "per-app Unix users require Linux root; user {user_name} does not exist"
        ))
    }
}

#[cfg(not(target_os = "linux"))]
fn prepare_app_cgroup(_data_dir: &Path, app_id: &str) -> Result<CgroupAssignment, String> {
    let _ = app_id;
    Err("cgroups require Linux".to_string())
}

#[cfg(not(target_os = "linux"))]
fn chown_path_tree(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    crate::unix::lchown_path(path, uid, gid)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        for entry in std::fs::read_dir(path)? {
            chown_path_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

fn app_child_parent_death_signal() -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        Some(libc::SIGTERM)
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(target_os = "linux"))]
    use crate::release::{app_runtime_data_paths, ensure_app_runtime_data_dirs};
    #[cfg(not(target_os = "linux"))]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(not(target_os = "linux"))]
    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn authorize_internal_socket_rejects_other_user() {
        assert!(authorize_internal_socket_app("notes/production", u32::MAX).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_app_start_rejects_an_unconfigured_data_directory() {
        let temp = tempfile::tempdir().unwrap();
        assert!(app_process_isolation(temp.path(), "unconfigured-test/production").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn app_process_allows_group_writable_shared_sidecars() {
        let temp = fixture::TestDataDir::new().unwrap();
        let isolation = app_process_isolation(temp.path(), "shared/production").unwrap();
        assert_eq!(isolation.umask, Some(tako_spawn::APP_DATA_UMASK));
    }

    #[test]
    fn app_unix_user_name_is_stable_and_posix_friendly() {
        let first = app_unix_user_name("notes/production");
        let second = app_unix_user_name("notes/production");
        assert_eq!(first, second);
        assert!(first.starts_with(APP_USER_PREFIX));
        assert!(first.len() <= 32);
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
    }

    #[test]
    fn app_unix_user_name_separates_environments() {
        assert_ne!(
            app_unix_user_name("notes/production"),
            app_unix_user_name("notes/staging")
        );
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn prepare_app_filesystem_isolation_sets_private_modes_without_root() {
        let temp = tempfile::tempdir().unwrap();
        let app_id = "notes/production";
        let data_paths = ensure_app_runtime_data_dirs(temp.path(), app_id).unwrap();
        let release_path = temp
            .path()
            .join("apps")
            .join("notes")
            .join("production")
            .join("releases")
            .join("v1");
        std::fs::create_dir_all(&release_path).unwrap();

        prepare_app_filesystem_isolation(temp.path(), app_id, Some(&release_path), &data_paths)
            .unwrap();

        let paths = app_runtime_data_paths(temp.path(), app_id);
        assert_eq!(mode(&release_path), 0o750);
        assert_eq!(mode(&paths.root), 0o710);
        assert_eq!(mode(&paths.app), 0o2770);
        assert_eq!(mode(&paths.tako), 0o700);
    }
}

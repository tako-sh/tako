use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use tako_spawn::{CgroupAssignment, ProcessIsolation, UserIds};

const APP_USER_PREFIX: &str = "tako-";
const SHARED_APP_GROUP: &str = "tako-app";
#[cfg(target_os = "linux")]
const CGROUP_MEMORY_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
#[cfg(target_os = "linux")]
const CGROUP_CPU_MAX: &str = "200000 100000";
#[cfg(target_os = "linux")]
const CGROUP_PIDS_MAX: u64 = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppUnixIdentity {
    pub(crate) user_name: String,
    pub(crate) ids: UserIds,
}

pub(crate) fn app_unix_user_name(app_id: &str) -> String {
    let digest = Sha256::digest(app_id.as_bytes());
    let hex = hex::encode(digest);
    format!("{APP_USER_PREFIX}{}", &hex[..16])
}

pub(crate) fn app_process_isolation(
    data_dir: &Path,
    app_id: &str,
) -> Result<ProcessIsolation, String> {
    let mut isolation = ProcessIsolation {
        parent_death_signal: app_child_parent_death_signal(),
        ..ProcessIsolation::default()
    };

    if !crate::unix::is_root() {
        isolation.resource_limits = tako_spawn::ResourceLimits {
            open_files: None,
            processes: None,
            address_space_bytes: None,
        };
        isolation.no_new_privs = false;
        isolation.clear_ambient_capabilities = false;
        isolation.umask = None;
        return Ok(isolation);
    }

    let identity = ensure_app_unix_identity(app_id)?;
    isolation.user = Some(identity.ids);
    isolation.cgroup = prepare_app_cgroup(data_dir, app_id).ok();
    Ok(isolation)
}

pub(crate) fn prepare_app_filesystem_isolation(
    data_dir: &Path,
    app_id: &str,
    release_path: Option<&Path>,
    data_paths: &crate::release::AppRuntimeDataPaths,
) -> Result<Option<AppUnixIdentity>, String> {
    prepare_app_directory_modes(data_dir, app_id, data_paths, release_path)?;

    if !crate::unix::is_root() {
        return Ok(None);
    }

    let identity = ensure_app_unix_identity(app_id)?;
    apply_app_directory_ownership(data_dir, app_id, &identity, data_paths, release_path)?;
    Ok(Some(identity))
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
    if releases_root.exists() {
        set_mode(&releases_root, 0o750)?;
    }
    let shared_root = app_root.join("shared");
    if shared_root.exists() {
        set_mode(&shared_root, 0o750)?;
    }
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

fn prepare_app_cgroup(_data_dir: &Path, app_id: &str) -> Result<CgroupAssignment, String> {
    #[cfg(target_os = "linux")]
    {
        let cgroup_root = current_cgroup_root()?.join("tako");
        let app_cgroup = cgroup_root.join(app_unix_user_name(app_id));
        std::fs::create_dir_all(&app_cgroup)
            .map_err(|e| format!("create app cgroup {}: {e}", app_cgroup.display()))?;
        write_if_exists(
            app_cgroup.join("memory.max"),
            CGROUP_MEMORY_MAX_BYTES.to_string(),
        )?;
        write_if_exists(app_cgroup.join("cpu.max"), CGROUP_CPU_MAX.to_string())?;
        write_if_exists(app_cgroup.join("pids.max"), CGROUP_PIDS_MAX.to_string())?;
        Ok(CgroupAssignment { path: app_cgroup })
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = app_id;
        Err("cgroups require Linux".to_string())
    }
}

#[cfg(target_os = "linux")]
fn current_cgroup_root() -> Result<PathBuf, String> {
    let raw = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| format!("read /proc/self/cgroup: {e}"))?;
    let rel = raw
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| "cgroup v2 is not available".to_string())?
        .trim_start_matches('/');
    Ok(Path::new("/sys/fs/cgroup").join(rel))
}

#[cfg(target_os = "linux")]
fn write_if_exists(path: PathBuf, value: String) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    std::fs::write(&path, value).map_err(|e| format!("write cgroup file {}: {e}", path.display()))
}

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
    use crate::release::{app_runtime_data_paths, ensure_app_runtime_data_dirs};
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
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

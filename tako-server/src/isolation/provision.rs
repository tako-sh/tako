//! Root-only provisioning. The caller selects an app and optional release, never a path or uid.
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

const CONFIG: &str = "/etc/tako/isolation.conf";
#[cfg(target_os = "linux")]
const HELPER: &str = "/usr/local/bin/tako-provision-app";

fn component(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
    {
        return Err("invalid provisioning name".into());
    }
    Ok(())
}

fn app_components(app: &str) -> Result<(&str, &str), String> {
    let (name, environment) = app.split_once('/').ok_or("expected app/environment")?;
    component(name)?;
    component(environment)?;
    Ok((name, environment))
}

fn config() -> Result<(String, String), String> {
    let file =
        open_absolute(Path::new(CONFIG)).map_err(|e| format!("open isolation config: {e}"))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(
            "isolation config must be root-owned and not writable by group or others".into(),
        );
    }
    let text = std::io::read_to_string(file).map_err(|e| e.to_string())?;
    let mut lines = text.lines();
    let root = lines.next().ok_or("missing isolation data directory")?;
    let user = lines.next().ok_or("missing isolation service user")?;
    if lines.next().is_some() || !Path::new(root).is_absolute() || root == "/" {
        return Err("invalid isolation configuration".into());
    }
    component(user)?;
    Ok((root.into(), user.into()))
}

#[cfg(target_os = "linux")]
pub(super) fn validate_service_context(data_dir: &Path) -> Result<(), String> {
    let (root, user) = config()?;
    let (service_uid, _) = crate::unix::lookup_user_ids(&user)
        .map_err(|e| e.to_string())?
        .ok_or("unknown isolation service user")?;
    check_service_context(data_dir, Path::new(&root), service_uid, unsafe {
        libc::geteuid()
    })
}

#[cfg(any(target_os = "linux", test))]
fn check_service_context(
    data_dir: &Path,
    configured_dir: &Path,
    service_uid: u32,
    caller_uid: u32,
) -> Result<(), String> {
    if data_dir != configured_dir {
        return Err("data directory differs from the installed isolation configuration".into());
    }
    if service_uid == 0 || (caller_uid != service_uid && caller_uid != 0) {
        return Err("app isolation requires the installed service identity".into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn request(data_dir: &Path, app: &str, release: Option<&Path>) -> Result<(), String> {
    app_components(app)?;
    let version = release
        .map(|path| {
            let version = path
                .file_name()
                .and_then(|v| v.to_str())
                .ok_or("invalid release path")?;
            component(version)?;
            if path
                != data_dir
                    .join("apps")
                    .join(app)
                    .join("releases")
                    .join(version)
            {
                return Err("release path is outside the app release directory".into());
            }
            Ok::<_, String>(version)
        })
        .transpose()?;
    #[cfg(test)]
    if super::fixture::contains(data_dir) {
        std::fs::create_dir_all(data_dir.join("apps").join(app)).map_err(|e| e.to_string())?;
        return prepare_filesystem(
            data_dir,
            app,
            version,
            unsafe { libc::geteuid() },
            &tako_spawn::UserIds {
                uid: unsafe { libc::geteuid() },
                gid: unsafe { libc::getegid() },
                supplementary_gids: Vec::new(),
            },
        );
    }
    validate_service_context(data_dir)?;
    let mut command = std::process::Command::new("/usr/bin/sudo");
    command
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .args(["-n", "--", HELPER, app]);
    if let Some(version) = version {
        command.arg(version);
    }
    let status = command
        .status()
        .map_err(|e| format!("provision app identity: {e}"))?;
    if !status.success() {
        return Err(format!("app provisioning failed: {status}"));
    }
    Ok(())
}

pub(crate) fn run_helper(args: &[String]) -> Result<(), String> {
    if !crate::unix::is_root() || !(1..=2).contains(&args.len()) {
        return Err(
            "provisioning requires root and an app/environment with optional release".into(),
        );
    }
    let lock = open_absolute(Path::new(CONFIG)).map_err(|e| e.to_string())?;
    // Serialize account creation and ownership changes across simultaneous spawns.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    app_components(&args[0])?;
    if let Some(version) = args.get(1) {
        component(version)?;
    }
    let (root, user) = config()?;
    let (service_uid, _) = crate::unix::lookup_user_ids(&user)
        .map_err(|e| e.to_string())?
        .ok_or("unknown isolation service user")?;
    if service_uid == 0 {
        return Err("isolation service user must be unprivileged".into());
    }
    let identity = super::ensure_app_unix_identity(&args[0])?;
    if identity.ids.uid == 0 || identity.ids.uid == service_uid {
        return Err("app identity must differ from the service and root".into());
    }
    prepare_cgroup(&identity.user_name).map_err(|e| {
        format!(
            "prepare cgroup v2 limits (requires writable cpu, memory and pids controllers): {e}"
        )
    })?;
    prepare_filesystem(
        Path::new(&root),
        &args[0],
        args.get(1).map(String::as_str),
        service_uid,
        &identity.ids,
    )
}

fn prepare_filesystem(
    root: &Path,
    app: &str,
    version: Option<&str>,
    service_uid: u32,
    ids: &tako_spawn::UserIds,
) -> Result<(), String> {
    let (name, environment) = app_components(app)?;
    let shared_access = DataAccess::Shared;
    // Synthetic fixtures share one identity and may run on filesystems without
    // ACLs. The installed isolation fixture exercises real group permissions.
    #[cfg(test)]
    let shared_access = if super::fixture::contains(root) {
        DataAccess::App
    } else {
        shared_access
    };
    let root = open_absolute(root).map_err(|e| e.to_string())?;
    let shared_root = ensure_directory(&root, "shared").map_err(|e| e.to_string())?;
    let shared_data = ensure_directory(&shared_root, "data").map_err(|e| e.to_string())?;
    let shared = ensure_directory(&shared_data, "app").map_err(|e| e.to_string())?;
    let shared_gid = ids.supplementary_gids.first().copied().unwrap_or(ids.gid);
    set_owner_mode(&shared_root, service_uid, shared_gid, 0o750).map_err(|e| e.to_string())?;
    set_owner_mode(&shared_data, service_uid, shared_gid, 0o750).map_err(|e| e.to_string())?;
    secure_data_tree(&shared, service_uid, shared_gid, shared_access)
        .map_err(|e| format!("prepare shared-data permissions (POSIX ACLs required): {e}"))?;
    let apps = open_child(&root, "apps", true).map_err(|e| e.to_string())?;
    let name = open_child(&apps, name, true).map_err(|e| e.to_string())?;
    let app = open_child(&name, environment, true).map_err(|e| e.to_string())?;
    set_owner_mode(&app, service_uid, ids.gid, 0o750).map_err(|e| e.to_string())?;
    for directory in ["releases", "shared", "data"] {
        let dir = ensure_directory(&app, directory).map_err(|e| e.to_string())?;
        set_owner_mode(&dir, service_uid, ids.gid, 0o750).map_err(|e| e.to_string())?;
    }
    let data = open_child(&app, "data", true).map_err(|e| e.to_string())?;
    let writable = ensure_directory(&data, "app").map_err(|e| e.to_string())?;
    secure_data_tree(&writable, service_uid, ids.gid, DataAccess::App)
        .map_err(|e| e.to_string())?;
    let private = ensure_directory(&data, "tako").map_err(|e| e.to_string())?;
    secure_data_tree(&private, service_uid, ids.gid, DataAccess::Private)
        .map_err(|e| e.to_string())?;
    let shared = open_child(&app, "shared", true).map_err(|e| e.to_string())?;
    let logs = ensure_directory(&shared, "logs").map_err(|e| e.to_string())?;
    secure_data_tree(&logs, service_uid, ids.gid, DataAccess::App).map_err(|e| e.to_string())?;
    if let Some(version) = version {
        let releases = open_child(&app, "releases", true).map_err(|e| e.to_string())?;
        let release = open_child(&releases, version, true).map_err(|e| e.to_string())?;
        secure_release(&release, service_uid, ids.gid, ids.uid, true).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn ensure_directory(parent: &File, name: &str) -> io::Result<File> {
    let name_c = CString::new(name).map_err(io::Error::other)?;
    // SAFETY: names are fixed internal directory names; existing entries are
    // opened with O_NOFOLLOW below, never trusted based on mkdir's result.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0
        && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
    {
        return Err(io::Error::last_os_error());
    }
    open_child(parent, name, true)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DataAccess {
    Private,
    App,
    Shared,
}

fn secure_data_tree(file: &File, uid: u32, gid: u32, access: DataAccess) -> io::Result<()> {
    let private = access == DataAccess::Private;
    let metadata = file.metadata()?;
    let mode = if metadata.is_dir() {
        if private { 0o700 } else { 0o2770 }
    } else if private {
        0o600
    } else if metadata.mode() & 0o111 != 0 {
        0o770
    } else {
        0o660
    };
    set_owner_mode(file, uid, gid, mode)?;
    if metadata.is_dir() {
        #[cfg(target_os = "linux")]
        if access == DataAccess::Shared {
            inherit_shared_group_permissions(file)?;
        }
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", file.as_raw_fd()))? {
            let name = entry?.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| io::Error::other("non-UTF8 data name"))?;
            match open_child(file, name, false) {
                Ok(child) => secure_data_tree(&child, uid, gid, access)?,
                Err(e) if e.raw_os_error() == Some(libc::ELOOP) => (),
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn inherit_shared_group_permissions(directory: &File) -> io::Result<()> {
    // SQLite eventually chmods WAL/SHM files to match the database, but another
    // app can open them read-only between creation under umask 027 and chmod.
    // A default ACL gives the owning shared group access at creation, including
    // in new subdirectories. Explicit modes such as 0600 still restrict access.
    // Linux POSIX ACL xattrs: version, then (tag, permissions, undefined id).
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, permissions) in [(0x01u16, 7u16), (0x04, 7), (0x10, 7), (0x20, 0)] {
        acl.extend_from_slice(&tag.to_le_bytes());
        acl.extend_from_slice(&permissions.to_le_bytes());
        acl.extend_from_slice(&u32::MAX.to_le_bytes());
    }
    // SAFETY: the directory fd is open, the name is NUL-terminated and acl
    // contains the complete Linux ACL representation for the supplied length.
    if unsafe {
        libc::fsetxattr(
            directory.as_raw_fd(),
            c"system.posix_acl_default".as_ptr(),
            acl.as_ptr().cast(),
            acl.len(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn prepare_cgroup(user: &str) -> io::Result<()> {
    std::fs::write(
        "/sys/fs/cgroup/cgroup.subtree_control",
        "+cpu +memory +pids",
    )?;
    let root = Path::new("/sys/fs/cgroup/tako-apps");
    std::fs::create_dir_all(root)?;
    std::fs::write(root.join("cgroup.subtree_control"), "+cpu +memory +pids")?;
    let app = root.join(user);
    std::fs::create_dir_all(&app)?;
    for (name, value) in [
        ("memory.max", "2147483648"),
        ("memory.swap.max", "0"),
        ("pids.max", "512"),
        ("cpu.max", "200000 100000"),
    ] {
        std::fs::write(app.join(name), value)?;
    }
    let file = open_absolute(&app.join("cgroup.procs"))?;
    // Migration runs as euid root before the permanent app identity drop. Keep
    // this file root-owned: the service deliberately has no CAP_DAC_OVERRIDE.
    if unsafe { libc::fchown(file.as_raw_fd(), 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_absolute(path: &Path) -> io::Result<File> {
    let mut file = File::open("/")?;
    let mut parts = path.components().peekable();
    if parts.next() != Some(Component::RootDir) {
        return Err(io::Error::other("expected absolute path"));
    }
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            return Err(io::Error::other("invalid path component"));
        };
        let name = name
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF8 path"))?;
        file = open_child(&file, name, parts.peek().is_some())?;
    }
    Ok(file)
}

fn open_child(parent: &File, name: &str, directory: bool) -> io::Result<File> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(io::Error::other("invalid path component"));
    }
    let name = CString::new(name).map_err(io::Error::other)?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent remains open, name is terminated, and returned fd is uniquely owned.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn set_owner_mode(file: &File, uid: u32, gid: u32, mode: u32) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_dir() && (!metadata.is_file() || metadata.nlink() != 1) {
        return Err(io::Error::other(
            "provisioning only accepts directories and singly linked regular files",
        ));
    }
    // SAFETY: operations target the opened inode, never a replaceable path.
    if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0
        || unsafe { libc::fchmod(file.as_raw_fd(), mode as _) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn secure_release(file: &File, uid: u32, gid: u32, app_uid: u32, root: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    // Package managers hardlink app-owned dependencies to their cache. Never
    // change those inodes through a privileged helper, including single links
    // which the app could hardlink concurrently. app.json has its own strict path.
    if !root && metadata.is_file() && metadata.uid() == app_uid {
        return Ok(());
    }
    let mode = if metadata.is_dir() {
        if root { 0o3770 } else { 0o2770 }
    } else if metadata.mode() & 0o111 != 0 {
        0o770
    } else {
        0o660
    };
    // Remove app write permission while sealing entries; only reopen the directory
    // once app.json is service-owned, so a concurrent app cannot replace it mid-walk.
    set_owner_mode(file, uid, gid, if metadata.is_dir() { 0o750 } else { mode })?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", file.as_raw_fd()))? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| io::Error::other("non-UTF8 release name"))?;
            if root && name == "app.json" {
                let manifest = open_child(file, name, false)?;
                if !manifest.metadata()?.is_file() {
                    return Err(io::Error::other("release app.json must be a regular file"));
                }
                set_owner_mode(&manifest, uid, gid, 0o640)?;
                continue;
            }
            match open_child(file, name, false) {
                Ok(child) => secure_release(&child, uid, gid, app_uid, false)?,
                // Symlinks remain unchanged; opening their targets is never privileged.
                Err(e) if e.raw_os_error() == Some(libc::ELOOP) => (),
                Err(e) => return Err(e),
            }
        }
        set_owner_mode(file, uid, gid, mode)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_owned_dependency_hardlinks_keep_inode_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let dependency = temp.path().join("dependency");
        std::fs::write(&cache, b"dependency").unwrap();
        std::fs::hard_link(&cache, &dependency).unwrap();
        let file = File::open(dependency).unwrap();
        let before = file.metadata().unwrap();
        secure_release(&file, 65533, 65534, before.uid(), false).unwrap();
        let after = file.metadata().unwrap();
        assert_eq!(
            (
                after.uid(),
                after.gid(),
                after.mode(),
                after.ino(),
                after.nlink()
            ),
            (before.uid(), before.gid(), before.mode(), before.ino(), 2)
        );
    }

    #[test]
    fn app_isolation_requires_the_installed_data_root_and_service_principal() {
        let root = Path::new("/opt/tako");
        assert!(check_service_context(root, root, 1000, 1000).is_ok());
        assert!(check_service_context(root, root, 1000, 0).is_ok());
        assert!(check_service_context(root, root, 1000, 1001).is_err());
        assert!(check_service_context(Path::new("/tmp/other"), root, 1000, 1000).is_err());
        assert!(check_service_context(root, root, 0, 0).is_err());
    }

    #[test]
    fn helper_rejects_paths_and_extra_operations() {
        for app in [
            "../production",
            "a/../../etc",
            "/etc",
            "a/b/c",
            "a/",
            "a\n/b",
        ] {
            assert!(app_components(app).is_err(), "{app}");
        }
        assert!(app_components("notes/production").is_ok());
        assert!(run_helper(&["notes/production".into(), "v1".into(), "sh".into()]).is_err());
    }

    #[test]
    fn descriptor_walk_rejects_symlink_components() {
        let temp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc", temp.path().join("escape")).unwrap();
        let parent = File::open(temp.path()).unwrap();
        assert!(open_child(&parent, "escape", true).is_err());
    }

    #[test]
    fn ownership_rejects_hard_linked_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("file"), b"private").unwrap();
        std::fs::hard_link(temp.path().join("file"), temp.path().join("link")).unwrap();
        let file = File::open(temp.path().join("link")).unwrap();
        assert!(
            set_owner_mode(
                &file,
                unsafe { libc::getuid() },
                unsafe { libc::getgid() },
                0o640
            )
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn app_data_symlink_does_not_change_private_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("app");
        let private = temp.path().join("tako");
        std::fs::create_dir(&app).unwrap();
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink("../tako", app.join("escape")).unwrap();
        secure_data_tree(
            &File::open(&app).unwrap(),
            unsafe { libc::geteuid() },
            unsafe { libc::getegid() },
            DataAccess::App,
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(private).unwrap().permissions().mode() & 0o7777,
            0o700
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shared_data_files_are_group_writable_at_creation_despite_umask() {
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::create_dir(shared.join("existing")).unwrap();
        match secure_data_tree(
            &File::open(&shared).unwrap(),
            unsafe { libc::geteuid() },
            unsafe { libc::getegid() },
            DataAccess::Shared,
        ) {
            Ok(()) => {}
            // Some development workspace mounts do not implement POSIX ACLs.
            // The installed Docker fixture must still exercise this behavior.
            Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => return,
            Err(error) => panic!("shared permissions: {error}"),
        }
        let status = std::process::Command::new("sh").current_dir(&shared).args([
            "-c", "set -eu; umask 027; mkdir new; printf data > new/wal; printf data > existing/wal; install -m 0600 /dev/null private"
        ]).status().unwrap();
        assert!(status.success());
        for file in ["new/wal", "existing/wal"] {
            assert_eq!(
                std::fs::metadata(shared.join(file)).unwrap().mode() & 0o777,
                0o660
            );
        }
        assert_eq!(
            std::fs::metadata(shared.join("private")).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn app_can_install_dependencies_but_cannot_replace_manifest() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::CommandExt;
        if !crate::unix::is_root() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let release = temp.path().join("release");
        std::fs::create_dir(&release).unwrap();
        std::fs::write(release.join("app.json"), b"protected").unwrap();
        std::fs::write(release.join("package.json"), b"{}").unwrap();
        secure_release(&File::open(&release).unwrap(), 65533, 65534, 65534, true).unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command.current_dir(&release).args(["-c", "set -eu; printf '{}' > package-lock.json; mkdir node_modules; printf '{}' > package.json; if printf bad > app.json 2>/dev/null; then exit 1; fi; if rm app.json 2>/dev/null; then exit 2; fi; if mv app.json replaced.json 2>/dev/null; then exit 3; fi"]);
        let policy = tako_spawn::ProcessIsolation {
            user: Some(tako_spawn::UserIds {
                uid: 65534,
                gid: 65534,
                supplementary_gids: vec![],
            }),
            ..Default::default()
        };
        unsafe {
            command.pre_exec(move || tako_spawn::install_process_isolation(&policy));
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(release.join("app.json")).unwrap(),
            b"protected"
        );
        assert!(release.join("node_modules").is_dir());
        secure_release(&File::open(&release).unwrap(), 65533, 65534, 65534, true).unwrap();
        let mut reinstall = std::process::Command::new("/bin/sh");
        reinstall.current_dir(&release).args(["-c", "set -eu; rm package-lock.json; mkdir node_modules/package; printf '{}' > node_modules/package/index.js; rm node_modules/package/index.js; rmdir node_modules/package; printf '{}' > package-lock.json"]);
        let policy = tako_spawn::ProcessIsolation {
            user: Some(tako_spawn::UserIds {
                uid: 65534,
                gid: 65534,
                supplementary_gids: vec![],
            }),
            ..Default::default()
        };
        unsafe {
            reinstall.pre_exec(move || tako_spawn::install_process_isolation(&policy));
        }
        assert!(reinstall.status().unwrap().success());
    }
}

//! Capture lives in a separate, filesystem-confined process. In particular,
//! closing a backup reader in the server must not release SQLite's POSIX locks
//! held by a capture connection to the same inode.
use std::os::unix::fs::MetadataExt;
use std::{fs::File, io, path::Path, process::Stdio};

/// Pre-create explicitly selected databases with group write permissions.
/// SQLite otherwise creates 0644 files, which a setgid directory cannot fix.
/// All traversal is relative to directory descriptors and refuses symlinks.
pub(crate) fn prepare_sources(root: &Path, databases: &[String]) -> Result<(), String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    tako_core::validate_database_paths(databases)?;
    for database in databases {
        let mut parent = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)
            .map_err(|e| e.to_string())?;
        let parts: Vec<_> = database.split('/').collect();
        for (i, component) in parts.iter().enumerate() {
            let name = std::ffi::CString::new(*component).map_err(|e| e.to_string())?;
            let directory = i + 1 < parts.len();
            if directory {
                let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o2770) };
                if result != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(io::Error::last_os_error().to_string());
                }
            }
            let flags = libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK
                | if directory {
                    libc::O_RDONLY | libc::O_DIRECTORY
                } else {
                    libc::O_RDWR | libc::O_CREAT
                };
            let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o660) };
            if fd < 0 {
                return Err(format!(
                    "prepare database {database}: {}",
                    io::Error::last_os_error()
                ));
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata().map_err(|e| e.to_string())?;
            if !directory && (!metadata.is_file() || metadata.nlink() != 1) {
                return Err("configured databases must be regular files without hard links".into());
            }
            if unsafe { libc::fchmod(file.as_raw_fd(), if directory { 0o2770 } else { 0o660 }) }
                != 0
            {
                return Err(format!(
                    "prepare database permissions: {}",
                    io::Error::last_os_error()
                ));
            }
            parent = file;
        }
    }
    Ok(())
}

pub(crate) fn validate_source(root: &Path, database: &str) -> Result<(), String> {
    tako_core::validate_database_paths(&[database.to_owned()])?;
    let mut path = root.to_path_buf();
    for component in Path::new(database).components() {
        path.push(component);
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if metadata.file_type().is_symlink()
            || (!metadata.is_dir() && (!metadata.is_file() || metadata.nlink() != 1))
        {
            return Err(
                "database paths must not contain symlinks, hard links, or special files".into(),
            );
        }
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        match std::fs::symlink_metadata(format!("{}{suffix}", path.display())) {
            Ok(metadata) if !metadata.is_file() || metadata.nlink() != 1 => {
                return Err("database sidecars must be regular files without hard links".into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

pub(crate) fn spawn(
    root: &Path,
    database: &str,
    spool: &Path,
) -> Result<tokio::process::Child, String> {
    validate_source(root, database)?;
    #[cfg(target_os = "linux")]
    let sandbox = {
        use landlock::{
            ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, Ruleset, RulesetAttr,
            RulesetCreatedAttr,
        };
        use std::os::fd::OwnedFd;
        let mut rules = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V3))
            .and_then(|rules| rules.create())
            .map_err(|e| format!("SQLite capture requires Landlock ABI 3: {e}"))?;
        for path in [root, spool] {
            rules = rules
                .add_rule(PathBeneath::new(
                    File::open(path).map_err(|e| e.to_string())?,
                    AccessFs::from_all(ABI::V3),
                ))
                .map_err(|e| e.to_string())?;
        }
        let fd: Option<OwnedFd> = rules.into();
        Stdio::from(fd.ok_or("missing capture ruleset")?)
    };
    #[cfg(not(target_os = "linux"))]
    let sandbox = Stdio::null();
    let error = File::create(spool.join("worker-error")).map_err(|e| e.to_string())?;
    let executable = if cfg!(target_os = "linux") {
        std::path::PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe().map_err(|e| e.to_string())?
    };
    let mut command = tokio::process::Command::new(executable);
    command.env_clear();
    #[cfg(not(test))]
    command
        .arg("--database-capture-worker")
        .arg(root.join(database))
        .arg(spool)
        .arg(std::process::id().to_string());
    #[cfg(test)]
    command
        .args([
            "--exact",
            "replica_worker::tests::capture_worker_entry",
            "--ignored",
            "--nocapture",
        ])
        .env(
            "TAKO_TEST_CAPTURE_ARGS",
            serde_json::json!([
                root.join(database),
                spool,
                std::path::PathBuf::from(std::process::id().to_string())
            ])
            .to_string(),
        );
    command
        .stdin(sandbox)
        .stdout(Stdio::null())
        .stderr(Stdio::from(error))
        .kill_on_drop(true);
    // App-created WAL/SHM files inherit the private app group's ownership.
    // Keep the service uid for the private spool, but grant the source group
    // before dropping capabilities. Shared data normally uses an existing group.
    let source_gid = std::fs::metadata(root).map_err(|e| e.to_string())?.gid();
    let group_count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if group_count < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    let mut groups = vec![0; group_count as usize];
    if unsafe { libc::getgroups(group_count, groups.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    let gid = unsafe { libc::getegid() };
    let user = (source_gid != gid && !groups.contains(&source_gid)).then(|| tako_spawn::UserIds {
        uid: unsafe { libc::geteuid() },
        gid,
        supplementary_gids: vec![source_gid],
    });
    let isolation = tako_spawn::ProcessIsolation {
        user,
        no_new_privs: true,
        clear_ambient_capabilities: true,
        parent_death_signal: Some(libc::SIGKILL),
        umask: Some(tako_spawn::APP_DATA_UMASK),
        ..Default::default()
    };
    // SAFETY: the common spawn policy makes only fork-safe system calls. The
    // capture process needs the service identity, but none of its capabilities.
    unsafe {
        command.pre_exec(move || tako_spawn::install_process_isolation(&isolation));
    }
    command
        .spawn()
        .map_err(|e| format!("start SQLite capture: {e}"))
}

pub(crate) fn run(
    database: &Path,
    spool: &Path,
    parent: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        // Only scalar, async-signal-safe syscalls before opening any app path.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0
            || unsafe { libc::getppid() } as u32 != parent
            || unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) } < 0
            || unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0
            || unsafe { libc::syscall(libc::SYS_landlock_restrict_self, libc::STDIN_FILENO, 0) } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
    }
    let original = std::fs::symlink_metadata(database)?;
    if !original.is_file() || original.nlink() != 1 || std::fs::canonicalize(database)? != database
    {
        return Err(
            io::Error::other("database paths must not contain aliases or special files").into(),
        );
    }
    // This process owns only one SQLite capture. An inode lock also prevents
    // aliases or a second spool from starting a second pipeline for that file.
    // Open it before SQLite and keep it until after SQLite's connections close.
    let source_lease = File::open(database)?;
    source_lease
        .try_lock()
        .map_err(|e| io::Error::other(format!("database already has a capture owner: {e}")))?;
    let mut capture = tako_backup::Capture::open(database, spool)?;
    loop {
        let available = crate::operations::replica_available_bytes(spool)?;
        if available < 512 * 1024 * 1024 {
            return Err(io::Error::other(
                "SQLite capture paused: less than 512 MiB of free spool space",
            )
            .into());
        }
        let current = std::fs::symlink_metadata(database)?;
        if !current.is_file()
            || current.nlink() != 1
            || (current.dev(), current.ino()) != (original.dev(), original.ino())
        {
            return Err(
                io::Error::other("database file was replaced while capture was active").into(),
            );
        }
        capture.sync()?;
        std::thread::sleep(tako_backup::BACKUP_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    #[ignore = "internal capture subprocess entry point"]
    fn capture_worker_entry() {
        let args: Vec<String> =
            serde_json::from_str(&std::env::var("TAKO_TEST_CAPTURE_ARGS").unwrap()).unwrap();
        run(
            Path::new(&args[0]),
            Path::new(&args[1]),
            args[2].parse().unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn isolated_capture_worker_holds_its_lease_until_stopped() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("data");
        let spool = temp.path().join("spool");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&spool).unwrap();
        let writer = rusqlite::Connection::open(source.join("app.sqlite")).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE items (value TEXT); INSERT INTO items VALUES ('saved');").unwrap();
        let mut child = match spawn(&source, "app.sqlite", &spool) {
            #[cfg(target_os = "linux")]
            Err(error) if error.starts_with("SQLite capture requires Landlock ABI 3:") => {
                // Kernels without Landlock must fail before any capture starts.
                // On supported hosts the remainder exercises the real worker.
                let abi = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, 0, 0, 1) };
                assert!(
                    abi < 3,
                    "supported kernel refused the capture rules: {error}"
                );
                assert!(
                    tako_backup::positions(&tako_backup::Spool::new(&spool))
                        .unwrap()
                        .0
                        .is_none()
                );
                return;
            }
            result => result.unwrap(),
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "capture exited {status}: {}",
                    std::fs::read_to_string(spool.join("worker-error")).unwrap()
                );
            }
            if tako_backup::positions(&tako_backup::Spool::new(&spool))
                .unwrap()
                .0
                .is_some()
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "capture worker did not publish a point"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(tako_backup::lock_capture(&spool).is_err());
        #[cfg(target_os = "linux")]
        {
            let status =
                std::fs::read_to_string(format!("/proc/{}/status", child.id().unwrap())).unwrap();
            for field in ["CapEff:", "CapPrm:", "CapInh:", "CapAmb:"] {
                assert_eq!(
                    status
                        .lines()
                        .find(|line| line.starts_with(field))
                        .unwrap()
                        .split_whitespace()
                        .nth(1),
                    Some("0000000000000000")
                );
            }
        }
        let other_spool = temp.path().join("other-spool");
        std::fs::create_dir(&other_spool).unwrap();
        let mut duplicate = spawn(&source, "app.sqlite", &other_spool).unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), duplicate.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
        assert!(
            std::fs::read_to_string(other_spool.join("worker-error"))
                .unwrap()
                .contains("already has a capture owner")
        );
        writer
            .execute("INSERT INTO items VALUES ('still writable')", [])
            .unwrap();
        child.kill().await.unwrap();
        assert!(tako_backup::lock_capture(&spool).is_ok());
    }

    #[test]
    fn selected_databases_are_group_writable_and_cannot_escape_through_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("shared");
        std::fs::create_dir(&root).unwrap();
        prepare_sources(&root, &["nested/app.sqlite".into()]).unwrap();
        assert_eq!(
            std::fs::metadata(root.join("nested/app.sqlite"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o660
        );
        assert_eq!(
            std::fs::metadata(root.join("nested"))
                .unwrap()
                .permissions()
                .mode()
                & 0o2770,
            0o2770
        );
        let outside = temp.path().join("private");
        std::fs::write(&outside, "private").unwrap();
        let original_mode = std::fs::metadata(&outside).unwrap().permissions().mode();
        symlink(&outside, root.join("alias.sqlite")).unwrap();
        assert!(prepare_sources(&root, &["alias.sqlite".into()]).is_err());
        std::fs::hard_link(&outside, root.join("linked.sqlite")).unwrap();
        assert!(prepare_sources(&root, &["linked.sqlite".into()]).is_err());
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode(),
            original_mode
        );
        assert!(validate_source(&root, "alias.sqlite").is_err());
        assert!(validate_source(&root, "linked.sqlite").is_err());
    }
}

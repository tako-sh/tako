//! One-shot Linux reader for a WAL database whose writer is already active.
//! The parent constructs the rules from open inodes, before SQLite resolves
//! any filename. Only this child enters the irreversible Landlock domain.

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, Ruleset, RulesetAttr,
    RulesetCreatedAttr,
};
use rusqlite::{Connection, OpenFlags, backup::Backup, backup::StepResult, config::DbConfig};
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::process::{Command, Stdio};

const SANDBOX_UNAVAILABLE: i32 = 77;

pub(crate) fn try_capture(
    directory: &File,
    name: &OsStr,
    sources: [&File; 3],
    staged: &Path,
) -> io::Result<bool> {
    // Older kernels and container policies may not support all these rights.
    // In that case the caller continues with its descriptor-based locked copy.
    let Ok(mut rules) = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V3))
        .and_then(|rules| rules.create())
    else {
        return Ok(false);
    };
    for file in sources {
        rules = rules
            .add_rule(PathBeneath::new(file, AccessFs::ReadFile))
            .map_err(io::Error::other)?;
    }
    let private = staged
        .parent()
        .ok_or_else(|| io::Error::other("missing staging directory"))?;
    let scratch = File::open(private)?;
    let scratch_access = AccessFs::ReadFile
        | AccessFs::WriteFile
        | AccessFs::ReadDir
        | AccessFs::MakeReg
        | AccessFs::RemoveFile
        | AccessFs::Truncate;
    let rules = rules
        .add_rule(PathBeneath::new(scratch, scratch_access))
        .map_err(io::Error::other)?;
    let fd: Option<OwnedFd> = rules.into();
    let fd = fd.ok_or_else(|| io::Error::other("missing filesystem ruleset"))?;
    let output = Command::new(std::env::current_exe()?)
        .arg("--snapshot-live-wal")
        .arg(Path::new(".").join(name))
        .arg(std::path::absolute(staged)?)
        .current_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))
        .env_clear()
        .stdin(Stdio::from(fd))
        .output()?;
    if output.status.code() == Some(SANDBOX_UNAVAILABLE) {
        return Ok(false);
    }
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "confined WAL snapshot failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(true)
}

pub(crate) fn run(source: &Path, staged: &Path) -> io::Result<()> {
    // Landlock applies to future opens. Drop any inherited service descriptors
    // before reading untrusted inputs. Keep the ruleset and output pipes.
    if unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) } < 0 {
        std::process::exit(SANDBOX_UNAVAILABLE);
    }
    // SAFETY: stdin is the inherited ruleset. Both calls accept only scalar
    // arguments. A missing or invalid ruleset fails before either path is opened.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0
        || unsafe { libc::syscall(libc::SYS_landlock_restrict_self, libc::STDIN_FILENO, 0) } < 0
    {
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS | libc::EOPNOTSUPP | libc::EPERM | libc::EACCES)
        ) {
            std::process::exit(SANDBOX_UNAVAILABLE);
        }
        return Err(error);
    }
    let db = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(io::Error::other)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)
        .map_err(io::Error::other)?;
    let mut dest = Connection::open(staged).map_err(io::Error::other)?;
    let backup = Backup::new(&db, &mut dest).map_err(io::Error::other)?;
    // One read transaction pins the snapshot. WAL writers may commit throughout.
    match backup.step(-1).map_err(io::Error::other)? {
        StepResult::Done => Ok(()),
        result => Err(io::Error::other(format!(
            "WAL backup incomplete: {result:?}"
        ))),
    }
}

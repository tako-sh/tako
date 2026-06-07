use std::path::Path;

/// Permissions for the tako data directory (typically `/opt/tako`).
///
/// `0o710` = `rwx--x---`: owner (`tako`) gets full access; group (`tako`,
/// which `tako-app` is a member of) gets traverse-only so app processes
/// spawned under `tako-app` can descend into `runtimes/` and
/// `apps/{name}/{env}/releases/{ver}/` to exec binaries and read
/// release files; world gets nothing.
///
/// Do not weaken to `0o700` — the kernel denies `tako-app` directory
/// traversal without the group `x` bit, and `execve` of any nested
/// binary returns `ENOENT`, which manifests as
/// `cold start spawn failed: No such file or directory`.
#[cfg(unix)]
const DATA_DIR_MODE: u32 = 0o710;

/// Create the tako data directory (idempotent) and set its permissions
/// so the `tako-app` sandbox user can traverse into release and runtime
/// subdirectories. See [`DATA_DIR_MODE`] for rationale.
#[cfg(unix)]
pub(crate) fn prepare_data_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(DATA_DIR_MODE))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn prepare_data_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

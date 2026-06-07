use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(super) struct ProjectDeployLock {
    _file: File,
    path: PathBuf,
}

impl Drop for ProjectDeployLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn git_repo_root(project_dir: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(project_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

pub(super) fn source_bundle_root(project_dir: &Path, runtime_id: &str) -> PathBuf {
    match git_repo_root(project_dir) {
        Some(root) if project_dir.starts_with(&root) => root,
        _ => tako_runtime::find_runtime_project_root(runtime_id, project_dir),
    }
}

pub(super) fn deploy_lock_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".tako/deploy.lock")
}

pub(super) fn acquire_project_deploy_lock(project_dir: &Path) -> Result<ProjectDeployLock, String> {
    let path = deploy_lock_path(project_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }

    let mut file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("Failed to open {}: {e}", path.display()))?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        write_deploy_lock_pid(&mut file)
            .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
        return Ok(ProjectDeployLock { _file: file, path });
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
        return Err(format!("Failed to lock {}: {err}", path.display()));
    }

    let owner_pid = read_deploy_lock_pid(&mut file);
    match owner_pid {
        Some(pid) => Err(format!(
            "Another deploy is already running for this project (PID {pid}). Wait for it to finish and try again."
        )),
        None => Err(
            "Another deploy is already running for this project. Wait for it to finish and try again."
                .to_string(),
        ),
    }
}

fn write_deploy_lock_pid(file: &mut File) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    write!(file, "{}", std::process::id())?;
    file.sync_all()?;
    Ok(())
}

fn read_deploy_lock_pid(file: &mut File) -> Option<u32> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut raw = String::new();
    file.read_to_string(&mut raw).ok()?;
    raw.trim().parse::<u32>().ok()
}

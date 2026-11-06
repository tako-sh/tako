//! Explicit local account substitute for behavior tests, never compiled into the server.
//! Installed Linux tests exercise real account creation, sudo, and cgroup delegation.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

static ROOTS: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);

pub(crate) struct TestDataDir(tempfile::TempDir);

impl TestDataDir {
    pub(crate) fn new() -> std::io::Result<Self> {
        let directory = tempfile::tempdir()?;
        ROOTS.lock().unwrap().insert(directory.path().to_path_buf());
        Ok(Self(directory))
    }

    pub(crate) fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for TestDataDir {
    fn drop(&mut self) {
        ROOTS.lock().unwrap().remove(self.path());
    }
}

#[cfg(target_os = "linux")]
pub(super) fn contains(path: &Path) -> bool {
    ROOTS.lock().unwrap().contains(path)
}

#[cfg(target_os = "linux")]
#[test]
fn fixture_only_applies_to_its_live_data_root() {
    let fixture = TestDataDir::new().unwrap();
    let root = fixture.path().to_path_buf();
    let app = "fixture/production";
    assert!(super::app_process_isolation(&root, app).is_ok());
    assert!(super::app_process_isolation(&root.join("other"), app).is_err());
    drop(fixture);
    assert!(super::app_process_isolation(&root, app).is_err());
}

/// A temporary directory that is private regardless of the process umask.
/// `tempfile::tempdir()` creates directories with mode 0o777 masked by the
/// umask, so under the common 022 the root is 0o755 and the private-directory
/// checks correctly refuse it.
#[cfg(test)]
pub(crate) fn private_tempdir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .expect("private temporary directory")
}

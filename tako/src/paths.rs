use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Get Tako's config directory (XDG-compliant).
///
/// - `TAKO_HOME` set → that directory (all-in-one).
/// - Debug builds from source checkout → `{repo}/local-dev/.tako` (all-in-one).
/// - Otherwise → `dirs::config_dir()/tako` (e.g. `~/.config/tako` on Linux,
///   `~/Library/Application Support/tako` on macOS).
pub fn tako_config_dir() -> Result<PathBuf, std::io::Error> {
    if let Some(home) = tako_home_override() {
        return Ok(home);
    }
    let base = dirs::config_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not determine config directory",
        )
    })?;
    Ok(base.join("tako"))
}

/// Get Tako's data directory (XDG-compliant).
///
/// - `TAKO_HOME` set → that directory (all-in-one).
/// - Debug builds from source checkout → `{repo}/local-dev/.tako` (all-in-one).
/// - Otherwise → `dirs::data_dir()/tako` (e.g. `~/.local/share/tako` on Linux,
///   `~/Library/Application Support/tako` on macOS).
pub fn tako_data_dir() -> Result<PathBuf, std::io::Error> {
    if let Some(home) = tako_home_override() {
        return Ok(home);
    }
    let base = dirs::data_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not determine data directory",
        )
    })?;
    Ok(base.join("tako"))
}

/// Get Tako's cache directory (XDG-compliant).
///
/// - `TAKO_HOME` set → that directory (all-in-one).
/// - Debug builds from source checkout → `{repo}/local-dev/.tako` (all-in-one).
/// - Otherwise → `dirs::cache_dir()/tako` (e.g. `~/.cache/tako` on Linux,
///   `~/Library/Caches/tako` on macOS).
pub fn tako_cache_dir() -> Result<PathBuf, std::io::Error> {
    if let Some(home) = tako_home_override() {
        return Ok(home);
    }
    let base = dirs::cache_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not determine cache directory",
        )
    })?;
    Ok(base.join("tako"))
}

/// Returns the override directory when `TAKO_HOME` is set or running from a
/// debug source checkout. Returns `None` when the XDG split should be used.
fn tako_home_override() -> Option<PathBuf> {
    let tako_home = std::env::var("TAKO_HOME").ok();
    let current_exe = std::env::current_exe().ok();
    tako_home_override_from(tako_home.as_deref(), current_exe.as_deref())
}

fn tako_home_override_from(tako_home: Option<&str>, current_exe: Option<&Path>) -> Option<PathBuf> {
    if let Some(v) = tako_home
        && !v.trim().is_empty()
    {
        return Some(PathBuf::from(v));
    }

    if cfg!(debug_assertions)
        && let Some(exe) = current_exe
        && let Some(dev_home) = dev_tako_home_from_exe(exe)
    {
        return Some(dev_home);
    }

    None
}

/// If `tako` is being run from a path under a `target/` directory, return the
/// repo root directory (the parent of `target/`).
pub fn repo_root_from_exe(exe_path: &Path) -> Option<PathBuf> {
    target_dir_from_exe(exe_path)?
        .parent()
        .map(|p| p.to_path_buf())
}

/// If `tako` is being run from a path under a `target/` directory, return that
/// `target/` directory path.
///
/// This works for:
/// - `.../target/debug/tako`
/// - `.../target/release/tako`
/// - `.../target/debug/deps/cli_integration-...`
pub fn target_dir_from_exe(exe_path: &Path) -> Option<PathBuf> {
    let mut cur = exe_path;
    loop {
        if cur.file_name().is_some_and(|n| n == "target") {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

/// Compute a dev-only Tako home directory under the repo root.
///
/// Example: `{repo}/local-dev/.tako`
pub fn dev_tako_home_from_exe(exe_path: &Path) -> Option<PathBuf> {
    repo_root_from_exe(exe_path).map(|root| root.join("local-dev").join(".tako"))
}

#[cfg(test)]
pub fn test_tako_home_env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn target_dir_from_exe_finds_target_for_normal_binary() {
        let exe = PathBuf::from("/Users/me/proj/target/debug/tako");
        assert_eq!(
            target_dir_from_exe(&exe).as_deref(),
            Some(Path::new("/Users/me/proj/target"))
        );
    }

    #[test]
    fn repo_root_from_exe_finds_repo_root() {
        let exe = PathBuf::from("/Users/me/proj/target/debug/tako");
        assert_eq!(
            repo_root_from_exe(&exe).as_deref(),
            Some(Path::new("/Users/me/proj"))
        );
    }

    #[test]
    fn target_dir_from_exe_finds_target_for_deps_binary() {
        let exe = PathBuf::from("/Users/me/proj/target/debug/deps/cli_integration-abc123");
        assert_eq!(
            target_dir_from_exe(&exe).as_deref(),
            Some(Path::new("/Users/me/proj/target"))
        );
    }

    #[test]
    fn dev_tako_home_is_under_target() {
        let exe = PathBuf::from("/Users/me/proj/target/debug/tako");
        assert_eq!(
            dev_tako_home_from_exe(&exe).as_deref(),
            Some(Path::new("/Users/me/proj/local-dev/.tako"))
        );
    }

    #[test]
    fn tako_home_override_returns_some_when_env_set() {
        let temp = TempDir::new().unwrap();
        let got = tako_home_override_from(temp.path().to_str(), None);
        assert_eq!(got, Some(temp.path().to_path_buf()));
    }

    #[test]
    fn tako_home_override_returns_none_when_env_unset_and_not_debug_exe() {
        let got = tako_home_override_from(None, Some(Path::new("/usr/local/bin/tako")));
        assert_eq!(got, None);
    }

    #[test]
    fn tako_home_override_uses_debug_exe_checkout_when_env_unset() {
        let got =
            tako_home_override_from(None, Some(Path::new("/Users/me/proj/target/debug/tako")));
        if cfg!(debug_assertions) {
            assert_eq!(
                got.as_deref(),
                Some(Path::new("/Users/me/proj/local-dev/.tako"))
            );
        } else {
            assert_eq!(got, None);
        }
    }
}

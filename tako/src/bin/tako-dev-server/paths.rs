use std::path::{Path, PathBuf};

/// Get Tako's data directory (XDG-compliant).
///
/// - `TAKO_HOME` set → that directory (all-in-one).
/// - Debug builds from source checkout → `{repo}/local-dev/.tako` (all-in-one).
/// - Otherwise → `dirs::data_dir()/tako`.
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

pub fn repo_root_from_exe(exe_path: &Path) -> Option<PathBuf> {
    target_dir_from_exe(exe_path)?
        .parent()
        .map(|p| p.to_path_buf())
}

pub fn target_dir_from_exe(exe_path: &Path) -> Option<PathBuf> {
    let mut cur = exe_path;
    loop {
        if cur.file_name().is_some_and(|n| n == "target") {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

pub fn dev_tako_home_from_exe(exe_path: &Path) -> Option<PathBuf> {
    repo_root_from_exe(exe_path).map(|root| root.join("local-dev").join(".tako"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn target_dir_from_exe_finds_target_for_normal_binary() {
        let exe = PathBuf::from("/Users/me/proj/target/debug/tako-dev-server");
        assert_eq!(
            target_dir_from_exe(&exe).as_deref(),
            Some(Path::new("/Users/me/proj/target"))
        );
    }

    #[test]
    fn repo_root_from_exe_finds_repo_root() {
        let exe = PathBuf::from("/Users/me/proj/target/debug/tako-dev-server");
        assert_eq!(
            repo_root_from_exe(&exe).as_deref(),
            Some(Path::new("/Users/me/proj"))
        );
    }

    #[test]
    fn dev_tako_home_is_under_local_dev() {
        let exe = PathBuf::from("/Users/me/proj/target/debug/tako-dev-server");
        assert_eq!(
            dev_tako_home_from_exe(&exe).as_deref(),
            Some(Path::new("/Users/me/proj/local-dev/.tako"))
        );
    }

    #[test]
    fn tako_home_override_returns_some_when_env_set() {
        let temp = TempDir::new().unwrap();
        let got = tako_home_override_from(temp.path().to_str(), None).unwrap();
        assert_eq!(got, temp.path());
    }
}

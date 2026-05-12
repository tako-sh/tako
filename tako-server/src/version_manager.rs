//! Runtime version management via direct binary download.
//!
//! Downloads runtime binaries directly from upstream releases
//! using the `[download]` spec in runtime TOML definitions. Replaces the previous
//! proto-based implementation.

use std::path::{Path, PathBuf};

/// Default install directory under the server data dir.
const RUNTIMES_SUBDIR: &str = "runtimes";

/// Resolve the runtimes install directory from the server data dir.
pub(crate) fn runtimes_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(RUNTIMES_SUBDIR)
}

/// Install a runtime and return the absolute binary path.
///
/// Resolution order:
/// 1. If already installed at `{data_dir}/runtimes/{tool}/{version}/`, return cached path
/// 2. If version is "latest", resolve from GitHub Releases API
/// 3. Download, verify checksum, extract, return binary path
///
/// Returns `None` if the runtime has no download spec or download fails.
fn is_safe_version(version: &str) -> bool {
    !version.is_empty()
        && !version.contains('/')
        && !version.contains('\\')
        && !version.contains("..")
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == '+')
}

pub(crate) async fn install_and_resolve(
    tool: &str,
    version: Option<&str>,
    data_dir: &Path,
) -> Option<String> {
    if let Some(v) = version
        && !is_safe_version(v)
    {
        tracing::warn!(tool, version = v, "Rejected unsafe runtime version string");
        return None;
    }

    let def = match tako_runtime::runtime_def_for(tool, None) {
        Some(def) => def,
        None => {
            tracing::warn!(tool, "Unknown runtime; cannot download binary");
            return None;
        }
    };

    if def.download.is_none() {
        tracing::warn!(
            tool,
            "Runtime has no [download] section; binary must be on PATH"
        );
        return None;
    }

    let version = match version {
        Some(v) if v != "latest" => v.to_string(),
        _ => match tako_runtime::resolve_latest_version(&def).await {
            Ok(v) => {
                tracing::info!(tool, version = %v, "Resolved latest version");
                v
            }
            Err(e) => {
                tracing::warn!(tool, error = %e, "Failed to resolve latest version");
                return None;
            }
        },
    };

    let install_dir = runtimes_dir(data_dir);

    // Ensure install dir has correct ownership when running as root.
    if let Err(e) = ensure_install_dir_ownership(&install_dir) {
        tracing::warn!(tool, error = %e, "Failed to prepare runtime install directory");
        return None;
    }

    let mgr = tako_runtime::DownloadManager::new(install_dir);

    // Check if already installed
    if let Some(bin) = mgr.resolve_bin(tool, &version, &def) {
        let bin_str = bin.to_string_lossy().to_string();
        tracing::info!(tool, version = %version, bin = %bin_str, "Runtime already installed");
        return Some(bin_str);
    }

    match mgr.install(tool, &version, &def).await {
        Ok(bin) => {
            let bin_str = bin.to_string_lossy().to_string();
            tracing::info!(tool, version = %version, bin = %bin_str, "Installed runtime binary");
            Some(bin_str)
        }
        Err(e) => {
            tracing::warn!(tool, version = %version, error = %e, "Runtime download failed");
            None
        }
    }
}

/// When running as root, ensure the install directory is owned by the `tako` service user
/// so that runtime binaries are accessible by the service.
fn ensure_install_dir_ownership(install_dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        if !crate::unix::is_root() {
            return Ok(());
        }
        std::fs::create_dir_all(install_dir)?;
        match crate::unix::lookup_user_ids("tako")? {
            Some((uid, gid)) => crate::unix::chown_path(install_dir, uid, gid),
            None => {
                tracing::warn!("tako user not found; runtime install directory owner unchanged");
                Ok(())
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = install_dir;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtimes_dir_is_under_data_dir() {
        let dir = runtimes_dir(Path::new("/opt/tako"));
        assert_eq!(dir, PathBuf::from("/opt/tako/runtimes"));
    }

    #[tokio::test]
    async fn install_and_resolve_returns_none_for_unknown_runtime() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = install_and_resolve("python", Some("3.12"), dir.path()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn install_and_resolve_returns_cached_path_when_installed() {
        let dir = tempfile::TempDir::new().unwrap();
        let version_dir = dir.path().join("runtimes/bun/1.0.0");
        std::fs::create_dir_all(&version_dir).unwrap();
        std::fs::write(version_dir.join("bun"), "fake").unwrap();

        let result = install_and_resolve("bun", Some("1.0.0"), dir.path()).await;
        assert!(result.is_some());
        assert!(result.unwrap().contains("bun"));
    }
}

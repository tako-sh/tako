mod archive;
mod checksum;
mod github;
mod http;
mod platform;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock};

use archive::extract_archive;
use checksum::verify_checksum;
use github::apply_github_api_headers;
use http::download_archive_bytes;
use platform::{resolve_arch_value, resolve_os_value};

use crate::types::RuntimeDef;

/// Manages downloading, extracting, and caching runtime binaries.
pub struct DownloadManager {
    install_dir: PathBuf,
}

static NEXT_INSTALL_ATTEMPT: AtomicU64 = AtomicU64::new(1);

impl DownloadManager {
    pub fn new(install_dir: PathBuf) -> Self {
        Self { install_dir }
    }

    /// Return the path to an already-installed runtime binary, or None.
    pub fn resolve_bin(&self, id: &str, version: &str, def: &RuntimeDef) -> Option<PathBuf> {
        let binary_name = extract_binary_name(def)?;
        let path = self.install_dir.join(id).join(version).join(binary_name);
        if path.is_file() { Some(path) } else { None }
    }

    /// Install a runtime binary and return its absolute path.
    /// If already installed, returns the cached path.
    pub async fn install(
        &self,
        id: &str,
        version: &str,
        def: &RuntimeDef,
    ) -> Result<PathBuf, String> {
        validate_version_string(version)?;
        if let Some(existing) = self.resolve_bin(id, version, def) {
            return Ok(existing);
        }

        let download = def
            .download
            .as_ref()
            .ok_or_else(|| format!("runtime '{id}' has no [download] section"))?;

        let os = resolve_os_value(&download.os_map)?;
        let arch = resolve_arch_value(&download.arch_map, &download.arch_variants)?;

        let url = download
            .url
            .as_ref()
            .ok_or_else(|| format!("runtime '{id}' has no download url"))?;
        let url = apply_template(url, version, &os, &arch);

        // Integrity verification is mandatory: a runtime with a [download]
        // section must also declare a checksum_url. Downloading a binary into
        // the release install dir without verification gives a compromised
        // mirror or hijacked redirect chain arbitrary code execution on the
        // deployment host.
        let checksum_url = download.checksum_url.as_ref().ok_or_else(|| {
            format!("runtime '{id}' has no checksum_url; integrity verification is required")
        })?;

        let archive_bytes = download_archive_bytes(&url).await?;

        let checksum_url = apply_template(checksum_url, version, &os, &arch);
        let checksum_format = download.checksum_format.as_deref().unwrap_or("shasums");
        verify_checksum(&archive_bytes, &checksum_url, checksum_format, &url).await?;

        // Atomic install: extract to temp dir, then rename to final path.
        // Prevents partial/corrupted installs from concurrent deploys.
        let version_dir = self.install_dir.join(id).join(version);
        let tmp_dir = temporary_install_dir(&self.install_dir, id, version);
        std::fs::create_dir_all(&tmp_dir)
            .map_err(|e| format!("failed to create {}: {e}", tmp_dir.display()))?;

        let format = download.format.as_deref().unwrap_or("tar.gz");
        extract_archive(
            &archive_bytes,
            format,
            &tmp_dir,
            download,
            version,
            &os,
            &arch,
        )?;

        // Create symlinks
        if let Some(ref extract) = download.extract {
            for symlink in &extract.symlinks {
                let link_path = tmp_dir.join(&symlink.name);
                let _ = std::fs::remove_file(&link_path);
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&symlink.target, &link_path).map_err(|e| {
                        format!(
                            "failed to create symlink {} -> {}: {e}",
                            link_path.display(),
                            symlink.target
                        )
                    })?;
                }
            }
        }

        // Make binary executable
        let binary_name = extract_binary_name(def)
            .ok_or_else(|| format!("runtime '{id}' has no extract.binary path"))?;
        let tmp_binary_path = tmp_dir.join(binary_name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&tmp_binary_path)
                .map_err(|e| format!("binary not found at {}: {e}", tmp_binary_path.display()))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&tmp_binary_path, perms).map_err(|e| {
                format!(
                    "failed to set permissions on {}: {e}",
                    tmp_binary_path.display()
                )
            })?;
        }

        self.complete_install(id, version, def, &tmp_dir, &version_dir, binary_name)
    }

    fn complete_install(
        &self,
        id: &str,
        version: &str,
        def: &RuntimeDef,
        tmp_dir: &Path,
        version_dir: &Path,
        binary_name: &str,
    ) -> Result<PathBuf, String> {
        if let Some(existing) = self.resolve_bin(id, version, def) {
            let _ = std::fs::remove_dir_all(tmp_dir);
            return Ok(existing);
        }

        match std::fs::rename(tmp_dir, version_dir) {
            Ok(()) => Ok(version_dir.join(binary_name)),
            Err(error) => {
                if let Some(existing) = self.resolve_bin(id, version, def) {
                    let _ = std::fs::remove_dir_all(tmp_dir);
                    return Ok(existing);
                }

                let _ = std::fs::remove_dir_all(tmp_dir);
                Err(format!(
                    "failed to finalize install at {}: {error}",
                    version_dir.display()
                ))
            }
        }
    }
}

/// Resolve the latest version from GitHub Releases API.
pub async fn resolve_latest_version(def: &RuntimeDef) -> Result<String, String> {
    let download = def
        .download
        .as_ref()
        .ok_or_else(|| format!("runtime '{}' has no [download] section", def.id))?;
    let source = download
        .version_source
        .as_ref()
        .ok_or_else(|| format!("runtime '{}' has no version_source", def.id))?;
    let repo = source
        .repo
        .as_ref()
        .ok_or_else(|| format!("runtime '{}' version_source has no repo", def.id))?;
    let tag_prefix = source.tag_prefix.as_deref().unwrap_or("");

    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let response = client.get(&url).header("User-Agent", "tako-server");
    let response = apply_github_api_headers(response)
        .send()
        .await
        .map_err(|e| format!("failed to fetch latest release for {repo}: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "GitHub API returned {} for {repo} latest release",
            response.status()
        ));
    }

    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("failed to parse GitHub release JSON: {e}"))?;

    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "GitHub release missing tag_name".to_string())?;

    let version = tag.strip_prefix(tag_prefix).unwrap_or(tag).to_string();
    if version.is_empty() {
        return Err(format!(
            "empty version after stripping prefix '{tag_prefix}' from tag '{tag}'"
        ));
    }
    validate_version_string(&version)?;
    Ok(version)
}

/// Reject version strings that could cause path traversal when used in filesystem paths.
fn validate_version_string(version: &str) -> Result<(), String> {
    if !version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
    {
        return Err(format!(
            "version string contains invalid characters: '{version}'"
        ));
    }
    Ok(())
}

fn temporary_install_dir(install_dir: &Path, id: &str, version: &str) -> PathBuf {
    let attempt = NEXT_INSTALL_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    install_dir
        .join(id)
        .join(format!(".{version}.{attempt}.installing"))
}

fn apply_template(template: &str, version: &str, os: &str, arch: &str) -> String {
    template
        .replace("{version}", version)
        .replace("{os}", os)
        .replace("{arch}", arch)
}

fn extract_binary_name(def: &RuntimeDef) -> Option<&str> {
    let extract = def.download.as_ref()?.extract.as_ref()?;
    let binary = extract.binary.as_deref()?;
    if extract.all {
        // With extract_all, directory structure is preserved. Use the full
        // relative path (e.g. "bin/node" stays as "bin/node").
        Some(binary)
    } else {
        // Without extract_all, only the binary is extracted (flattened).
        Some(
            binary
                .rsplit_once('/')
                .map_or(binary, |(_, file_name)| file_name),
        )
    }
}

#[cfg(test)]
mod tests;

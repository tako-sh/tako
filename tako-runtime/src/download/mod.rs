mod archive;
mod checksum;
mod github;
mod http;
mod platform;

use std::path::PathBuf;
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
        let tmp_dir = self
            .install_dir
            .join(id)
            .join(format!(".{version}.installing"));
        let _ = std::fs::remove_dir_all(&tmp_dir);
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

        // Atomic rename: move temp dir to final path.
        // If the final path already exists (concurrent install won), that's fine.
        let _ = std::fs::remove_dir_all(&version_dir);
        std::fs::rename(&tmp_dir, &version_dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            format!(
                "failed to finalize install at {}: {e}",
                version_dir.display()
            )
        })?;

        Ok(version_dir.join(binary_name))
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
mod tests {
    use super::archive::{extract_tar_gz, extract_zip};
    use super::github::{apply_github_auth_for_url, github_token_from_env};
    use super::platform::{resolve_arch, resolve_os};
    use super::*;
    use reqwest::header::AUTHORIZATION;
    use sha2::{Digest, Sha256};
    use std::io::Cursor;
    use std::path::PathBuf;
    use tempfile::TempDir;

    use crate::DownloadDef;

    fn github_token_env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn preserve_token_envs() -> (Option<std::ffi::OsString>, Option<std::ffi::OsString>) {
        (
            std::env::var_os("GH_TOKEN"),
            std::env::var_os("GITHUB_TOKEN"),
        )
    }

    fn restore_token_envs(previous: (Option<std::ffi::OsString>, Option<std::ffi::OsString>)) {
        match previous.0 {
            Some(value) => unsafe { std::env::set_var("GH_TOKEN", value) },
            None => unsafe { std::env::remove_var("GH_TOKEN") },
        }
        match previous.1 {
            Some(value) => unsafe { std::env::set_var("GITHUB_TOKEN", value) },
            None => unsafe { std::env::remove_var("GITHUB_TOKEN") },
        }
    }

    #[test]
    fn github_token_from_env_prefers_gh_token_over_github_token() {
        let _lock = github_token_env_lock();
        let previous = preserve_token_envs();
        unsafe {
            std::env::set_var("GH_TOKEN", "gh-token");
            std::env::set_var("GITHUB_TOKEN", "github-token");
        }

        let token = github_token_from_env();

        restore_token_envs(previous);
        assert_eq!(token.as_deref(), Some("gh-token"));
    }

    #[test]
    fn github_token_from_env_falls_back_when_gh_token_is_empty() {
        let _lock = github_token_env_lock();
        let previous = preserve_token_envs();
        unsafe {
            std::env::set_var("GH_TOKEN", " ");
            std::env::set_var("GITHUB_TOKEN", "github-token");
        }

        let token = github_token_from_env();

        restore_token_envs(previous);
        assert_eq!(token.as_deref(), Some("github-token"));
    }

    #[test]
    fn apply_github_auth_for_url_skips_non_github_urls() {
        let _lock = github_token_env_lock();
        let previous = preserve_token_envs();
        unsafe {
            std::env::set_var("GH_TOKEN", "secret");
        }

        let request = apply_github_auth_for_url(
            reqwest::Client::new().get("https://downloads.example.com/runtime.tar.gz"),
            "https://downloads.example.com/runtime.tar.gz",
        )
        .build()
        .unwrap();

        restore_token_envs(previous);
        assert!(request.headers().get(AUTHORIZATION).is_none());
    }

    #[test]
    fn apply_template_substitutes_all_variables() {
        assert_eq!(
            apply_template(
                "https://example.com/{version}/bin-{os}-{arch}.zip",
                "1.2.3",
                "darwin",
                "x64"
            ),
            "https://example.com/1.2.3/bin-darwin-x64.zip"
        );
    }

    #[test]
    fn resolve_os_returns_known_value() {
        let os = resolve_os();
        assert!(
            ["macos", "linux", "windows"].contains(&os),
            "unexpected OS: {os}"
        );
    }

    #[test]
    fn resolve_arch_returns_known_value() {
        let arch = resolve_arch();
        assert!(["x64", "arm64"].contains(&arch), "unexpected arch: {arch}");
    }

    #[test]
    fn extract_binary_name_gets_filename_from_path() {
        let def = crate::runtime_def_for("bun", None).unwrap();
        let name = extract_binary_name(&def).unwrap();
        assert_eq!(name, "bun");
    }

    #[test]
    fn resolve_bin_returns_none_when_not_installed() {
        let dir = TempDir::new().unwrap();
        let mgr = DownloadManager::new(dir.path().to_path_buf());
        let def = crate::runtime_def_for("bun", None).unwrap();
        assert!(mgr.resolve_bin("bun", "1.0.0", &def).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn install_rejects_download_without_checksum_url() {
        use crate::types::{
            DownloadDef, EntrypointDef, EnvsDef, PackageManagerDef, PresetDef, RuntimeDef,
            ServerDef,
        };

        let dir = TempDir::new().unwrap();
        let mgr = DownloadManager::new(dir.path().to_path_buf());

        let def = RuntimeDef {
            id: "fakert".into(),
            language: "fake".into(),
            entrypoint: EntrypointDef {
                candidates: vec!["main.js".into()],
                manifest: None,
            },
            preset: PresetDef::default(),
            server: ServerDef {
                entrypoint_path: None,
                launch_args: vec![],
            },
            envs: EnvsDef::default(),
            package_manager: PackageManagerDef {
                id: "fake".into(),
                name: None,
                lockfiles: vec![],
                add: None,
                install: None,
                development: None,
            },
            download: Some(DownloadDef {
                version_source: None,
                url: Some("https://example.com/fake-{version}.tar.gz".into()),
                format: Some("tar.gz".into()),
                checksum_url: None,
                checksum_format: None,
                os_map: std::collections::HashMap::from([
                    ("macos".into(), "darwin".into()),
                    ("linux".into(), "linux".into()),
                ]),
                arch_map: std::collections::HashMap::from([
                    ("x64".into(), "x64".into()),
                    ("arm64".into(), "arm64".into()),
                ]),
                arch_variants: Default::default(),
                extract: None,
            }),
        };

        let err = mgr.install("fakert", "1.0.0", &def).await.unwrap_err();
        assert!(
            err.contains("checksum_url"),
            "expected checksum_url requirement error, got: {err}"
        );
    }

    #[test]
    fn resolve_bin_returns_path_when_installed() {
        let dir = TempDir::new().unwrap();
        let version_dir = dir.path().join("bun").join("1.0.0");
        std::fs::create_dir_all(&version_dir).unwrap();
        std::fs::write(version_dir.join("bun"), "fake binary").unwrap();

        let mgr = DownloadManager::new(dir.path().to_path_buf());
        let def = crate::runtime_def_for("bun", None).unwrap();
        let path = mgr.resolve_bin("bun", "1.0.0", &def).unwrap();
        assert_eq!(path, version_dir.join("bun"));
    }

    #[test]
    fn zip_extraction_works() {
        use std::io::Write;
        let dir = TempDir::new().unwrap();
        // Create a minimal zip in memory
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut writer = zip::ZipWriter::new(cursor);
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("bun-linux-x64/bun", options).unwrap();
            writer.write_all(b"fake bun binary").unwrap();
            writer.finish().unwrap();
        }

        let download = DownloadDef {
            version_source: None,
            url: None,
            format: Some("zip".to_string()),
            checksum_url: None,
            checksum_format: None,
            os_map: Default::default(),
            arch_map: Default::default(),
            arch_variants: Default::default(),
            extract: Some(crate::types::ExtractDef {
                binary: Some("bun-{os}-{arch}/bun".to_string()),
                strip_components: None,
                all: false,
                symlinks: vec![],
            }),
        };

        extract_zip(&buf, dir.path(), &download, "1.0.0", "linux", "x64").unwrap();
        let extracted = std::fs::read_to_string(dir.path().join("bun")).unwrap();
        assert_eq!(extracted, "fake bun binary");
    }

    #[test]
    fn tar_gz_extraction_works() {
        let dir = TempDir::new().unwrap();

        // Create a tar.gz in memory
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let data = b"fake node binary";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "node-v22.0.0-linux-x64/bin/node", &data[..])
                .unwrap();
            builder.finish().unwrap();
        }

        let mut gz_buf = Vec::new();
        {
            use flate2::write::GzEncoder;
            use std::io::Write;
            let mut encoder = GzEncoder::new(&mut gz_buf, flate2::Compression::fast());
            encoder.write_all(&tar_buf).unwrap();
            encoder.finish().unwrap();
        }

        let download = DownloadDef {
            version_source: None,
            url: None,
            format: Some("tar.gz".to_string()),
            checksum_url: None,
            checksum_format: None,
            os_map: Default::default(),
            arch_map: Default::default(),
            arch_variants: Default::default(),
            extract: Some(crate::types::ExtractDef {
                binary: Some("node-v{version}-{os}-{arch}/bin/node".to_string()),
                strip_components: None,
                all: false,
                symlinks: vec![],
            }),
        };

        extract_tar_gz(&gz_buf, dir.path(), &download, "22.0.0", "linux", "x64").unwrap();
        let extracted = std::fs::read_to_string(dir.path().join("node")).unwrap();
        assert_eq!(extracted, "fake node binary");
    }

    #[test]
    fn zip_extraction_rejects_paths_that_escape_destination() {
        use std::io::Write;

        let sandbox = TempDir::new().unwrap();
        let dest = sandbox.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();

        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut writer = zip::ZipWriter::new(cursor);
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("../escape.txt", options).unwrap();
            writer.write_all(b"should not write outside").unwrap();
            writer.finish().unwrap();
        }

        let download = DownloadDef {
            version_source: None,
            url: None,
            format: Some("zip".to_string()),
            checksum_url: None,
            checksum_format: None,
            os_map: Default::default(),
            arch_map: Default::default(),
            arch_variants: Default::default(),
            extract: Some(crate::types::ExtractDef {
                binary: None,
                strip_components: None,
                all: true,
                symlinks: vec![],
            }),
        };

        let err = extract_zip(&buf, &dest, &download, "1.0.0", "linux", "x64").unwrap_err();
        assert!(err.contains("escapes extraction directory"));
        assert!(!sandbox.path().join("escape.txt").exists());
    }

    #[test]
    fn tar_gz_extraction_rejects_symlink_escape_targets() {
        use std::io::Write;

        let sandbox = TempDir::new().unwrap();
        let dest = sandbox.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let escaped_dir = sandbox.path().join("escaped");
        std::fs::create_dir_all(&escaped_dir).unwrap();

        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);

            let mut link_header = tar::Header::new_gnu();
            link_header.set_entry_type(tar::EntryType::Symlink);
            link_header.set_size(0);
            link_header.set_mode(0o777);
            link_header.set_link_name("../escaped").unwrap();
            link_header.set_cksum();
            builder
                .append_data(&mut link_header, "bin", std::io::empty())
                .unwrap();

            let data = b"should not escape";
            let mut file_header = tar::Header::new_gnu();
            file_header.set_size(data.len() as u64);
            file_header.set_mode(0o644);
            file_header.set_cksum();
            builder
                .append_data(&mut file_header, "bin/pwned.txt", &data[..])
                .unwrap();

            builder.finish().unwrap();
        }

        let mut gz_buf = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_buf, flate2::Compression::fast());
            encoder.write_all(&tar_buf).unwrap();
            encoder.finish().unwrap();
        }

        let download = DownloadDef {
            version_source: None,
            url: None,
            format: Some("tar.gz".to_string()),
            checksum_url: None,
            checksum_format: None,
            os_map: Default::default(),
            arch_map: Default::default(),
            arch_variants: Default::default(),
            extract: Some(crate::types::ExtractDef {
                binary: None,
                strip_components: None,
                all: true,
                symlinks: vec![],
            }),
        };

        let err = extract_tar_gz(&gz_buf, &dest, &download, "1.0.0", "linux", "x64").unwrap_err();
        assert!(err.contains("symlink target escapes extraction directory"));
        assert!(!escaped_dir.join("pwned.txt").exists());
    }

    #[test]
    fn tar_gz_extraction_allows_internal_relative_symlinks() {
        use std::io::Write;

        let dir = TempDir::new().unwrap();

        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);

            let lib_data = b"npm cli";
            let mut lib_header = tar::Header::new_gnu();
            lib_header.set_size(lib_data.len() as u64);
            lib_header.set_mode(0o644);
            lib_header.set_cksum();
            builder
                .append_data(&mut lib_header, "lib/npm-cli.js", &lib_data[..])
                .unwrap();

            let mut link_header = tar::Header::new_gnu();
            link_header.set_entry_type(tar::EntryType::Symlink);
            link_header.set_size(0);
            link_header.set_mode(0o777);
            link_header.set_link_name("../lib/npm-cli.js").unwrap();
            link_header.set_cksum();
            builder
                .append_data(&mut link_header, "bin/npm", std::io::empty())
                .unwrap();

            builder.finish().unwrap();
        }

        let mut gz_buf = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_buf, flate2::Compression::fast());
            encoder.write_all(&tar_buf).unwrap();
            encoder.finish().unwrap();
        }

        let download = DownloadDef {
            version_source: None,
            url: None,
            format: Some("tar.gz".to_string()),
            checksum_url: None,
            checksum_format: None,
            os_map: Default::default(),
            arch_map: Default::default(),
            arch_variants: Default::default(),
            extract: Some(crate::types::ExtractDef {
                binary: None,
                strip_components: None,
                all: true,
                symlinks: vec![],
            }),
        };

        extract_tar_gz(&gz_buf, dir.path(), &download, "1.0.0", "linux", "x64").unwrap();

        let link_path = dir.path().join("bin/npm");
        let target = std::fs::read_link(&link_path).unwrap();
        assert_eq!(target, PathBuf::from("../lib/npm-cli.js"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("lib/npm-cli.js")).unwrap(),
            "npm cli"
        );
    }

    #[test]
    fn sha256_hash_is_consistent() {
        let data = b"hello world";
        let hash1 = {
            let mut h = Sha256::new();
            h.update(data);
            format!("{:x}", h.finalize())
        };
        let hash2 = {
            let mut h = Sha256::new();
            h.update(data);
            format!("{:x}", h.finalize())
        };
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64); // SHA-256 hex length
    }

    #[test]
    fn os_map_resolution_for_all_runtimes() {
        for id in &["bun", "node"] {
            let def = crate::runtime_def_for(id, None).unwrap();
            let download = def.download.as_ref().unwrap();
            let os = resolve_os();
            assert!(
                download.os_map.contains_key(os),
                "runtime {id} missing os_map entry for '{os}'"
            );
        }
    }

    #[test]
    fn arch_map_resolution_for_all_runtimes() {
        for id in &["bun", "node"] {
            let def = crate::runtime_def_for(id, None).unwrap();
            let download = def.download.as_ref().unwrap();
            let arch = resolve_arch();
            assert!(
                download.arch_map.contains_key(arch),
                "runtime {id} missing arch_map entry for '{arch}'"
            );
        }
    }
}

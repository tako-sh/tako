use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use crate::output;

pub(super) async fn download_and_install(
    url: &str,
    install_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let tmp_base = std::env::temp_dir();
    let tmp_dir = tmp_base.join(format!("tako-upgrade-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir)?;

    let result = download_and_install_inner(url, install_dir, &tmp_dir).await;

    let _ = std::fs::remove_dir_all(&tmp_dir);

    result
}

async fn download_and_install_inner(
    url: &str,
    install_dir: &Path,
    tmp_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let archive_path = tmp_dir.join("tako.tar.gz");

    download_archive(url, &archive_path).await?;

    let sha_url = format!("{url}.sha256");
    let expected = fetch_sha256(&sha_url)
        .await
        .map_err(|e| format!("SHA256 checksum unavailable for {sha_url}: {e}"))?;
    verify_sha256(&archive_path, &expected)?;

    let extract_dir = tmp_dir.join("extract");
    {
        let _t = output::timed("Extract archive");
        std::fs::create_dir_all(&extract_dir)?;
        extract_tarball(&archive_path, &extract_dir)?;
    }

    let _t = output::timed(&format!("Install binaries to {}", install_dir.display()));
    let tako_bin =
        find_binary(&extract_dir, "tako").ok_or("archive did not contain a tako binary")?;
    let dev_server_bin = find_binary(&extract_dir, "tako-dev-server")
        .ok_or("archive did not contain a tako-dev-server binary")?;
    let dev_proxy_bin = find_binary(&extract_dir, "tako-dev-proxy")
        .ok_or("archive did not contain a tako-dev-proxy binary")?;

    std::fs::create_dir_all(install_dir)?;
    install_binary(&tako_bin, install_dir, "tako")?;
    install_binary(&dev_server_bin, install_dir, "tako-dev-server")?;
    install_binary(&dev_proxy_bin, install_dir, "tako-dev-proxy")?;

    Ok(())
}

async fn download_archive(url: &str, dest: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let _t = output::timed(&format!("Download release archive from {url}"));
    let client = reqwest::Client::new();
    let mut resp =
        crate::github::apply_auth_for_url(client.get(url).header("User-Agent", "tako-cli"), url)
            .send()
            .await?
            .error_for_status()
            .map_err(|e| format!("download failed: {e}"))?;

    let total = resp.content_length().unwrap_or(0);
    tracing::debug!(
        "Download started, content_length={}",
        if total > 0 {
            format!("{total} bytes")
        } else {
            "unknown".to_string()
        }
    );
    let mut file = std::fs::File::create(dest)?;
    let mut downloaded = 0u64;

    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk)?;
        downloaded += chunk.len() as u64;
    }

    tracing::debug!("Downloaded {} bytes to {}", downloaded, dest.display());
    Ok(())
}

async fn fetch_sha256(url: &str) -> Result<String, String> {
    let client = reqwest::Client::new();
    let resp =
        crate::github::apply_auth_for_url(client.get(url).header("User-Agent", "tako-cli"), url)
            .send()
            .await
            .map_err(|e| format!("{e}"))?
            .error_for_status()
            .map_err(|e| format!("{e}"))?;
    let text = resp.text().await.map_err(|e| format!("{e}"))?;
    Ok(text.split_whitespace().next().unwrap_or("").to_string())
}

fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("failed to read archive: {e}"))?;
    let hash = Sha256::digest(&data);
    let actual = hex::encode(hash);

    if actual != expected {
        return Err(format!(
            "SHA256 mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(())
}

fn extract_tarball(archive: &Path, dest: &Path) -> Result<(), String> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .map_err(|e| format!("failed to run tar: {e}"))?;

    if !status.success() {
        return Err("failed to extract archive".to_string());
    }
    Ok(())
}

fn find_binary(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_file() && path.file_name().map(|n| n == name).unwrap_or(false) {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = find_binary(&path, name)
        {
            return Some(found);
        }
    }
    None
}

fn install_binary(src: &Path, dest_dir: &Path, name: &str) -> Result<(), String> {
    let dest = dest_dir.join(name);
    let tmp_dest = temporary_install_path(dest_dir, name);

    let result = (|| {
        std::fs::copy(src, &tmp_dest)
            .map_err(|e| format!("failed to stage {name} at {}: {e}", tmp_dest.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp_dest, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("failed to set permissions on {name}: {e}"))?;
        }

        std::fs::rename(&tmp_dest, &dest)
            .map_err(|e| format!("failed to install {name} to {}: {e}", dest.display()))?;

        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_dest);
    }

    result
}

fn temporary_install_path(dest_dir: &Path, name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    dest_dir.join(format!(".{name}.tako-install-{}-{now}", std::process::id()))
}

pub(super) fn detect_platform() -> Result<(&'static str, &'static str), String> {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        return Err("unsupported OS".to_string());
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        return Err("unsupported architecture".to_string());
    };

    Ok((os, arch))
}

pub(super) fn resolve_install_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        return dir.to_path_buf();
    }

    dirs::home_dir()
        .map(|h| h.join(".local").join("bin"))
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_platform_returns_valid_pair() {
        let (os, arch) = detect_platform().unwrap();
        assert!(os == "darwin" || os == "linux");
        assert!(arch == "x86_64" || arch == "aarch64");
    }

    #[test]
    fn verify_sha256_rejects_mismatch() {
        let dir = std::env::temp_dir().join("tako-test-sha");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.bin");
        std::fs::write(&path, b"hello").unwrap();

        let err = verify_sha256(
            &path,
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap_err();
        assert!(err.contains("SHA256 mismatch"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_sha256_accepts_correct_hash() {
        let dir = std::env::temp_dir().join("tako-test-sha-ok");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.bin");
        std::fs::write(&path, b"hello").unwrap();

        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        verify_sha256(&path, expected).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_binary_locates_file_in_subdirectory() {
        let dir = std::env::temp_dir().join("tako-test-find");
        let sub = dir.join("subdir");
        let _ = std::fs::create_dir_all(&sub);
        std::fs::write(sub.join("tako"), b"binary").unwrap();

        let found = find_binary(&dir, "tako");
        assert!(found.is_some());
        assert!(found.unwrap().ends_with("tako"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_binary_returns_none_when_missing() {
        let dir = std::env::temp_dir().join("tako-test-find-none");
        let _ = std::fs::create_dir_all(&dir);

        let found = find_binary(&dir, "nonexistent");
        assert!(found.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn install_binary_replaces_existing_destination_with_new_file() {
        use std::os::unix::fs::MetadataExt;

        let dir =
            std::env::temp_dir().join(format!("tako-test-install-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dest_dir = dir.join("bin");
        std::fs::create_dir_all(&dest_dir).unwrap();

        let src = dir.join("src-tako");
        let dest = dest_dir.join("tako");
        std::fs::write(&src, b"new binary").unwrap();
        std::fs::write(&dest, b"old binary").unwrap();
        let old_inode = std::fs::metadata(&dest).unwrap().ino();

        install_binary(&src, &dest_dir, "tako").unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"new binary");
        let new_inode = std::fs::metadata(&dest).unwrap().ino();
        assert_ne!(
            new_inode, old_inode,
            "installed binary should be swapped in as a fresh file"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

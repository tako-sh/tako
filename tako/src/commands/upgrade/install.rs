use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use crate::output;

pub(super) async fn download_and_install(
    url: &str,
    install_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = create_upgrade_temp_dir()?;

    download_and_install_inner(url, install_dir, tmp_dir.path()).await
}

fn create_upgrade_temp_dir() -> Result<tempfile::TempDir, std::io::Error> {
    tempfile::Builder::new().prefix("tako-upgrade-").tempdir()
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
    let dev_server_bin = find_binary(&extract_dir, "tako-dev-server")
        .ok_or("archive did not contain a tako-dev-server binary")?;
    let dev_proxy_bin = find_binary(&extract_dir, "tako-dev-proxy")
        .ok_or("archive did not contain a tako-dev-proxy binary")?;

    std::fs::create_dir_all(install_dir)?;

    #[cfg(target_os = "macos")]
    {
        let tako_app =
            find_app_bundle(&extract_dir, "Tako.app").ok_or("archive did not contain Tako.app")?;
        verify_macos_signature_if_needed(&dev_server_bin, "tako-dev-server")?;
        verify_macos_signature_if_needed(&dev_proxy_bin, "tako-dev-proxy")?;
        install_macos_app_bundle(&tako_app, install_dir)?;
        install_binary(&dev_server_bin, install_dir, "tako-dev-server")?;
        install_binary(&dev_proxy_bin, install_dir, "tako-dev-proxy")?;
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    {
        let tako_bin =
            find_binary(&extract_dir, "tako").ok_or("archive did not contain a tako binary")?;
        install_binary(&tako_bin, install_dir, "tako")?;
        install_binary(&dev_server_bin, install_dir, "tako-dev-server")?;
        install_binary(&dev_proxy_bin, install_dir, "tako-dev-proxy")?;
        Ok(())
    }
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

#[cfg(target_os = "macos")]
fn find_app_bundle(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_dir() && path.file_name().map(|n| n == name).unwrap_or(false) {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = find_app_bundle(&path, name)
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

        verify_macos_signature_if_needed(&tmp_dest, name)?;

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

fn verify_macos_signature_if_needed(path: &Path, name: &str) -> Result<(), String> {
    if !is_macho_file(path)? {
        return Ok(());
    }

    verify_macos_signature_for_macho(path, name)
}

#[cfg(target_os = "macos")]
fn verify_macos_signature_for_macho(path: &Path, name: &str) -> Result<(), String> {
    let output = macos_codesign_verify_command(path, false)
        .output()
        .map_err(|e| format!("failed to start codesign for {name}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }

    Err(format!(
        "refusing to install {name}: macOS code signature verification failed: {}",
        command_failure_detail(&output)
    ))
}

#[cfg(not(target_os = "macos"))]
fn verify_macos_signature_for_macho(path: &Path, name: &str) -> Result<(), String> {
    let _ = path;
    let _ = name;
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_macos_app_bundle(src_app: &Path, bin_dir: &Path) -> Result<(), String> {
    let target_app = resolve_macos_app_path();
    let app_parent = target_app
        .parent()
        .ok_or_else(|| format!("invalid macOS app path {}", target_app.display()))?;
    std::fs::create_dir_all(app_parent)
        .map_err(|e| format!("failed to create {}: {e}", app_parent.display()))?;

    let tmp_app = temporary_install_path(app_parent, "Tako.app");
    let _ = std::fs::remove_dir_all(&tmp_app);
    let result = (|| {
        copy_macos_app_bundle(src_app, &tmp_app)?;
        verify_macos_app_bundle(&tmp_app)?;
        atomic_replace_path(&tmp_app, &target_app)?;
        install_macos_cli_symlink(bin_dir, &target_app)
    })();

    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp_app);
    }

    result
}

#[cfg(target_os = "macos")]
fn copy_macos_app_bundle(src_app: &Path, tmp_app: &Path) -> Result<(), String> {
    let output = Command::new("ditto")
        .arg(src_app)
        .arg(tmp_app)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to start ditto: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "failed to copy Tako.app: {}",
            command_failure_detail(&output)
        ))
    }
}

#[cfg(target_os = "macos")]
fn verify_macos_app_bundle(app: &Path) -> Result<(), String> {
    let output = macos_codesign_verify_command(app, true)
        .output()
        .map_err(|e| format!("failed to start codesign for Tako.app: {e}"))?;
    if output.status.success() {
        return Ok(());
    }

    Err(format!(
        "refusing to install Tako.app: macOS code signature verification failed: {}",
        command_failure_detail(&output)
    ))
}

#[cfg(target_os = "macos")]
fn macos_codesign_verify_command(path: &Path, deep: bool) -> Command {
    let mut command = Command::new("codesign");
    command.args(["--verify", "--strict", "--verbose=4"]);
    if deep {
        command.arg("--deep");
    }
    command
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[cfg(target_os = "macos")]
fn install_macos_cli_symlink(bin_dir: &Path, target_app: &Path) -> Result<(), String> {
    let target = target_app.join("Contents").join("MacOS").join("tako");
    let link = bin_dir.join("tako");
    let tmp_link = temporary_install_path(bin_dir, "tako");
    let _ = std::fs::remove_file(&tmp_link);
    std::os::unix::fs::symlink(&target, &tmp_link).map_err(|e| {
        format!(
            "failed to stage tako symlink {} -> {}: {e}",
            tmp_link.display(),
            target.display()
        )
    })?;

    let result = std::fs::rename(&tmp_link, &link)
        .map_err(|e| format!("failed to link tako to {}: {e}", target.display()));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_link);
    }
    result
}

#[cfg(target_os = "macos")]
fn atomic_replace_path(src: &Path, dest: &Path) -> Result<(), String> {
    if !dest.exists() {
        return std::fs::rename(src, dest)
            .map_err(|e| format!("failed to install {}: {e}", dest.display()));
    }

    let src_c = std::ffi::CString::new(src.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("path contains an interior NUL byte: {}", src.display()))?;
    let dest_c = std::ffi::CString::new(dest.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("path contains an interior NUL byte: {}", dest.display()))?;

    // SAFETY: both C strings are NUL-terminated and derived from live `Path`
    // values. `RENAME_SWAP` atomically exchanges two existing same-volume paths.
    let rc = unsafe { libc::renamex_np(src_c.as_ptr(), dest_c.as_ptr(), libc::RENAME_SWAP) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!(
            "failed to swap {} into place: {err}",
            dest.display()
        ));
    }

    std::fs::remove_dir_all(src).map_err(|e| {
        format!(
            "installed {}, but failed to remove previous app bundle at {}: {e}",
            dest.display(),
            src.display()
        )
    })
}

#[cfg(target_os = "macos")]
fn resolve_macos_app_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(app) = macos_app_bundle_from_exe(&exe)
    {
        return app;
    }

    if let Ok(dir) = std::env::var("TAKO_MACOS_APP_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("Tako.app");
        }
    }

    dirs::home_dir()
        .map(|home| home.join("Applications").join("Tako.app"))
        .unwrap_or_else(|| PathBuf::from("Tako.app"))
}

#[cfg(target_os = "macos")]
fn command_failure_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if !detail.is_empty() {
        return detail.to_string();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = stdout.trim();
    if !detail.is_empty() {
        return detail.to_string();
    }

    format!("command exited with {}", output.status)
}

fn is_macho_file(path: &Path) -> Result<bool, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("failed to inspect {path:?}: {e}"))?;
    let mut magic = [0u8; 4];
    let read = file
        .read(&mut magic)
        .map_err(|e| format!("failed to inspect {path:?}: {e}"))?;
    Ok(read == magic.len() && has_macho_magic(&magic))
}

fn has_macho_magic(bytes: &[u8]) -> bool {
    matches!(
        bytes.get(..4),
        Some([0xfe, 0xed, 0xfa, 0xce])
            | Some([0xce, 0xfa, 0xed, 0xfe])
            | Some([0xfe, 0xed, 0xfa, 0xcf])
            | Some([0xcf, 0xfa, 0xed, 0xfe])
            | Some([0xca, 0xfe, 0xba, 0xbe])
            | Some([0xbe, 0xba, 0xfe, 0xca])
            | Some([0xca, 0xfe, 0xba, 0xbf])
            | Some([0xbf, 0xba, 0xfe, 0xca])
    )
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

    validate_supported_platform(os, arch)?;

    Ok((os, arch))
}

fn validate_supported_platform(os: &str, arch: &str) -> Result<(), String> {
    if os == "darwin" && arch != "aarch64" {
        return Err("macOS is supported on Apple Silicon only".to_string());
    }

    Ok(())
}

pub(super) fn resolve_install_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    if let Some(dir) = resolve_macos_bin_dir() {
        return dir;
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        return dir.to_path_buf();
    }

    dirs::home_dir()
        .map(|h| h.join(".local").join("bin"))
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin"))
}

#[cfg(target_os = "macos")]
fn resolve_macos_bin_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if macos_app_bundle_from_exe(&exe).is_some() {
        return find_path_entry_for_exe(&exe)
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("bin")));
    }

    exe.parent().map(Path::to_path_buf)
}

#[cfg(target_os = "macos")]
fn find_path_entry_for_exe(exe: &Path) -> Option<PathBuf> {
    let canonical_exe = std::fs::canonicalize(exe).ok()?;
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("tako");
        if std::fs::canonicalize(&candidate).ok().as_ref() == Some(&canonical_exe) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn macos_app_bundle_from_exe(exe: &Path) -> Option<PathBuf> {
    let suffix = Path::new("Tako.app")
        .join("Contents")
        .join("MacOS")
        .join("tako");
    if exe.ends_with(&suffix) {
        return exe.parent()?.parent()?.parent().map(Path::to_path_buf);
    }

    let canonical_exe = std::fs::canonicalize(exe).ok()?;
    if !canonical_exe.ends_with(suffix) {
        return None;
    }

    canonical_exe
        .parent()?
        .parent()?
        .parent()
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_platform_returns_valid_pair() {
        let (os, arch) = detect_platform().unwrap();
        assert!(os == "darwin" || os == "linux");
        assert!(arch == "x86_64" || arch == "aarch64");
        if os == "darwin" {
            assert_eq!(arch, "aarch64");
        }
    }

    #[test]
    fn validate_supported_platform_rejects_intel_macos() {
        assert_eq!(
            validate_supported_platform("darwin", "x86_64"),
            Err("macOS is supported on Apple Silicon only".to_string())
        );
        assert_eq!(validate_supported_platform("linux", "x86_64"), Ok(()));
        assert_eq!(validate_supported_platform("darwin", "aarch64"), Ok(()));
    }

    #[test]
    fn upgrade_temp_dir_is_unique_and_removed_on_drop() {
        let first = create_upgrade_temp_dir().unwrap();
        let second = create_upgrade_temp_dir().unwrap();

        assert_ne!(first.path(), second.path());
        assert!(
            first
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("tako-upgrade-")
        );

        let first_path = first.path().to_path_buf();
        drop(first);
        assert!(!first_path.exists());
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

    #[test]
    fn has_macho_magic_recognizes_thin_and_fat_binaries() {
        assert!(has_macho_magic(&[0xcf, 0xfa, 0xed, 0xfe]));
        assert!(has_macho_magic(&[0xca, 0xfe, 0xba, 0xbe]));
        assert!(has_macho_magic(&[0xca, 0xfe, 0xba, 0xbf]));
    }

    #[test]
    fn has_macho_magic_rejects_short_or_plain_files() {
        assert!(!has_macho_magic(&[]));
        assert!(!has_macho_magic(&[0xcf, 0xfa, 0xed]));
        assert!(!has_macho_magic(b"#!/bin/sh"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_app_bundle_from_exe_detects_tako_app_main_binary() {
        let exe = Path::new("/Users/me/Applications/Tako.app/Contents/MacOS/tako");
        assert_eq!(
            macos_app_bundle_from_exe(exe).as_deref(),
            Some(Path::new("/Users/me/Applications/Tako.app"))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_app_bundle_from_exe_rejects_loose_binary() {
        let exe = Path::new("/Users/me/.local/bin/tako");
        assert!(macos_app_bundle_from_exe(exe).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_app_bundle_from_exe_follows_cli_symlink() {
        let dir =
            std::env::temp_dir().join(format!("tako-test-app-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app_bin = dir.join("Apps/Tako.app/Contents/MacOS");
        std::fs::create_dir_all(&app_bin).unwrap();
        std::fs::write(app_bin.join("tako"), b"binary").unwrap();
        let symlink_dir = dir.join("bin");
        std::fs::create_dir_all(&symlink_dir).unwrap();
        let symlink = symlink_dir.join("tako");
        std::os::unix::fs::symlink(app_bin.join("tako"), &symlink).unwrap();

        let expected = std::fs::canonicalize(dir.join("Apps/Tako.app")).unwrap();
        assert_eq!(macos_app_bundle_from_exe(&symlink), Some(expected));

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

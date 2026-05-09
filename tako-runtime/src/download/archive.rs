use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

use crate::types::DownloadDef;

use super::apply_template;

pub(super) fn extract_archive(
    data: &[u8],
    format: &str,
    dest: &Path,
    download: &DownloadDef,
    version: &str,
    os: &str,
    arch: &str,
) -> Result<(), String> {
    match format {
        "zip" => extract_zip(data, dest, download, version, os, arch),
        "tar.gz" => extract_tar_gz(data, dest, download, version, os, arch),
        other => Err(format!("unsupported archive format: {other}")),
    }
}

pub(super) fn extract_zip(
    data: &[u8],
    dest: &Path,
    download: &DownloadDef,
    version: &str,
    os: &str,
    arch: &str,
) -> Result<(), String> {
    let cursor = Cursor::new(data);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("failed to open zip archive: {e}"))?;

    let extract = download.extract.as_ref();
    let binary_template = extract.and_then(|e| e.binary.as_deref());
    let extract_all = extract.is_some_and(|e| e.all);
    let strip = extract.and_then(|e| e.strip_components).unwrap_or(0) as usize;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("failed to read zip entry {i}: {e}"))?;

        if file.is_dir() {
            continue;
        }

        let entry_name = file.name().to_string();

        let output_rel = if extract_all {
            strip_path_components(&entry_name, strip)
        } else if let Some(template) = binary_template {
            let expected = apply_template(template, version, os, arch);
            if entry_name == expected || entry_name.trim_start_matches('/') == expected {
                Some(
                    expected
                        .rsplit_once('/')
                        .map_or(expected.as_str(), |(_, file_name)| file_name)
                        .to_string(),
                )
            } else {
                None
            }
        } else {
            Some(
                entry_name
                    .rsplit_once('/')
                    .map_or(entry_name.as_str(), |(_, file_name)| file_name)
                    .to_string(),
            )
        };

        let Some(rel) = output_rel else {
            continue;
        };
        let rel_path = normalize_archive_relative_path(&rel)?;
        let output_path = archive_output_path(dest, &rel_path, true)?;
        if let Some(parent) = output_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|e| format!("failed to read zip entry '{entry_name}': {e}"))?;
        std::fs::write(&output_path, &buf)
            .map_err(|e| format!("failed to write {}: {e}", output_path.display()))?;
    }

    Ok(())
}

pub(super) fn extract_tar_gz(
    data: &[u8],
    dest: &Path,
    download: &DownloadDef,
    version: &str,
    os: &str,
    arch: &str,
) -> Result<(), String> {
    let gz = flate2::read::GzDecoder::new(Cursor::new(data));
    let mut archive = tar::Archive::new(gz);

    let extract = download.extract.as_ref();
    let binary_template = extract.and_then(|e| e.binary.as_deref());
    let extract_all = extract.is_some_and(|e| e.all);
    let strip = extract.and_then(|e| e.strip_components).unwrap_or(0) as usize;

    for entry in archive
        .entries()
        .map_err(|e| format!("failed to read tar entries: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("failed to read tar entry: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("invalid tar entry path: {e}"))?
            .to_path_buf();
        let path_str = path.to_string_lossy().to_string();

        if entry.header().entry_type().is_dir() {
            continue;
        }

        let output_rel = if extract_all {
            strip_path_components(&path_str, strip)
        } else if let Some(template) = binary_template {
            let expected = apply_template(template, version, os, arch);
            if path_str == expected || path_str.trim_start_matches('/') == expected {
                Some(
                    expected
                        .rsplit_once('/')
                        .map_or(expected.as_str(), |(_, file_name)| file_name)
                        .to_string(),
                )
            } else {
                None
            }
        } else {
            Some(
                path_str
                    .rsplit_once('/')
                    .map_or(path_str.as_str(), |(_, file_name)| file_name)
                    .to_string(),
            )
        };

        let Some(rel) = output_rel else {
            continue;
        };
        let rel_path = normalize_archive_relative_path(&rel)?;

        // Preserve symlinks from the archive (e.g. node's bin/npm -> ../lib/...)
        if entry.header().entry_type() == tar::EntryType::Symlink {
            #[cfg(unix)]
            if let Ok(target) = entry.link_name()
                && let Some(target) = target
            {
                validate_archive_symlink_target(&rel_path, target.as_ref())?;
                let link_path = archive_output_path(dest, &rel_path, true)?;
                if let Some(parent) = link_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::os::unix::fs::symlink(target.as_ref(), &link_path).map_err(|e| {
                    format!("failed to create symlink {}: {e}", link_path.display())
                })?;
            }
            continue;
        }

        let output_path = archive_output_path(dest, &rel_path, true)?;
        if let Some(parent) = output_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| format!("failed to read tar entry '{path_str}': {e}"))?;
        std::fs::write(&output_path, &buf)
            .map_err(|e| format!("failed to write {}: {e}", output_path.display()))?;

        // Preserve executable permissions
        #[cfg(unix)]
        if let Ok(mode) = entry.header().mode()
            && mode & 0o111 != 0
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&output_path, std::fs::Permissions::from_mode(mode));
        }
    }

    Ok(())
}

/// Strip N path components from an archive entry path.
/// "node-v22/bin/node" with strip=1 -> "bin/node"
fn strip_path_components(path: &str, n: usize) -> Option<String> {
    if n == 0 {
        return Some(path.to_string());
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= n {
        return None;
    }
    Some(parts[n..].join("/"))
}

fn normalize_archive_relative_path(raw: &str) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!("archive path '{raw}' escapes extraction directory"));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("archive path '{raw}' must be relative"));
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(format!("archive path '{raw}' is empty"));
    }

    Ok(normalized)
}

fn archive_output_path(
    dest: &Path,
    rel_path: &Path,
    include_final_component: bool,
) -> Result<PathBuf, String> {
    let components: Vec<_> = rel_path.components().collect();
    let mut current = dest.to_path_buf();

    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            continue;
        };
        current.push(part);

        let is_final = index + 1 == components.len();
        if !include_final_component && is_final {
            break;
        }

        if let Ok(metadata) = std::fs::symlink_metadata(&current)
            && metadata.file_type().is_symlink()
        {
            return Err(format!(
                "archive entry '{}' resolves through symlink '{}'",
                rel_path.display(),
                current.display()
            ));
        }
    }

    Ok(dest.join(rel_path))
}

fn validate_archive_symlink_target(link_rel_path: &Path, target: &Path) -> Result<(), String> {
    if target.is_absolute() {
        return Err(format!(
            "archive symlink target escapes extraction directory: {}",
            target.display()
        ));
    }

    let link_parent = link_rel_path.parent().unwrap_or_else(|| Path::new(""));
    normalize_archive_relative_path(&link_parent.join(target).to_string_lossy())
        .map(|_| ())
        .map_err(|_| {
            format!(
                "archive symlink target escapes extraction directory: {}",
                target.display()
            )
        })
}

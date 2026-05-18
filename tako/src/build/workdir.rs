use std::path::{Component, Path};

use super::BuildError;

/// Copy the project tree from `source_root` to `workdir`, respecting .gitignore.
/// Force-excludes `.git/`, `.tako/`, `.env*`, and `node_modules/`.
pub fn create_workdir(source_root: &Path, workdir: &Path) -> Result<(), BuildError> {
    if workdir.exists() {
        std::fs::remove_dir_all(workdir)?;
    }
    std::fs::create_dir_all(workdir)?;

    let mut walker = ignore::WalkBuilder::new(source_root);
    walker
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false);

    for entry in walker.build() {
        let entry = entry.map_err(|e| BuildError::ArchiveError(e.to_string()))?;
        let path = entry.path();
        let relative = path.strip_prefix(source_root).map_err(|e| {
            BuildError::ArchiveError(format!(
                "Failed to compute relative path for {}: {}",
                path.display(),
                e
            ))
        })?;

        if relative.as_os_str().is_empty() {
            continue;
        }

        if should_force_exclude(relative) {
            continue;
        }

        let dest = workdir.join(relative);
        if let Some(ft) = entry.file_type() {
            if ft.is_dir() {
                std::fs::create_dir_all(&dest)?;
            } else if ft.is_file() {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(path, &dest)?;
            }
        }
    }

    Ok(())
}

/// Symlink `node_modules/` directories from the original tree into the workdir.
/// Only symlinks `node_modules/` directories that have a sibling `package.json`.
pub fn symlink_node_modules(source_root: &Path, workdir: &Path) -> Result<(), BuildError> {
    symlink_node_modules_recursive(source_root, source_root, workdir)
}

fn symlink_node_modules_recursive(
    current: &Path,
    source_root: &Path,
    workdir: &Path,
) -> Result<(), BuildError> {
    let entries = match std::fs::read_dir(current) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_type = entry.file_type()?;

        if file_name == "node_modules" && file_type.is_dir() {
            // Only symlink if sibling package.json exists
            if current.join("package.json").is_file() {
                let relative = current.strip_prefix(source_root).map_err(|e| {
                    BuildError::ArchiveError(format!("Failed to compute relative path: {}", e))
                })?;
                let link_path = workdir.join(relative).join("node_modules");
                if !link_path.exists() {
                    if let Some(parent) = link_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(&path, &link_path)?;
                    #[cfg(windows)]
                    std::os::windows::fs::symlink_dir(&path, &link_path)?;
                }
            }
            // Don't recurse into node_modules
            continue;
        }

        if file_name == ".git" || file_name == ".tako" {
            continue;
        }

        if file_type.is_dir() {
            symlink_node_modules_recursive(&path, source_root, workdir)?;
        }
    }

    Ok(())
}

/// Remove the workdir directory.
pub fn cleanup_workdir(workdir: &Path) {
    let _ = std::fs::remove_dir_all(workdir);
}

fn should_force_exclude(relative_path: &Path) -> bool {
    for component in relative_path.components() {
        if let Component::Normal(name) = component {
            match name.to_str() {
                Some(".git") | Some(".tako") | Some("node_modules") => return true,
                Some(name) if name.starts_with(".env") => return true,
                _ => {}
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn setup_project(temp: &TempDir) -> PathBuf {
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join(".tako/cache")).unwrap();
        fs::create_dir_all(root.join("node_modules/some-pkg")).unwrap();
        fs::write(root.join("package.json"), r#"{"name":"test"}"#).unwrap();
        fs::write(root.join("src/index.ts"), "export {}").unwrap();
        fs::write(root.join(".git/config"), "git config").unwrap();
        fs::write(root.join(".tako/cache/data"), "cache").unwrap();
        fs::write(root.join(".env"), "SECRET=foo").unwrap();
        fs::write(root.join(".env.production"), "SECRET=bar").unwrap();
        fs::write(root.join("node_modules/some-pkg/index.js"), "module").unwrap();
        fs::write(root.join("README.md"), "readme").unwrap();
        root
    }

    #[test]
    fn create_workdir_copies_non_excluded_files() {
        let temp = TempDir::new().unwrap();
        let root = setup_project(&temp);
        let workdir = temp.path().join("workdir");

        create_workdir(&root, &workdir).unwrap();

        assert!(workdir.join("src/index.ts").exists());
        assert!(workdir.join("package.json").exists());
        assert!(workdir.join("README.md").exists());
    }

    #[test]
    fn create_workdir_excludes_git_tako_env_node_modules() {
        let temp = TempDir::new().unwrap();
        let root = setup_project(&temp);
        let workdir = temp.path().join("workdir");

        create_workdir(&root, &workdir).unwrap();

        assert!(!workdir.join(".git").exists());
        assert!(!workdir.join(".tako").exists());
        assert!(!workdir.join(".env").exists());
        assert!(!workdir.join(".env.production").exists());
        assert!(!workdir.join("node_modules").exists());
    }

    #[test]
    fn symlink_node_modules_creates_symlinks_with_package_json() {
        let temp = TempDir::new().unwrap();
        let root = setup_project(&temp);
        let workdir = temp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();

        symlink_node_modules(&root, &workdir).unwrap();

        let link = workdir.join("node_modules");
        assert!(link.exists());
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        // Symlink target should be the original node_modules
        let target = fs::read_link(&link).unwrap();
        assert_eq!(target, root.join("node_modules"));
    }

    #[test]
    fn symlink_node_modules_skips_without_package_json() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "ok").unwrap();
        // No package.json at root
        let workdir = temp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();

        symlink_node_modules(&root, &workdir).unwrap();

        assert!(!workdir.join("node_modules").exists());
    }

    #[test]
    fn symlink_node_modules_handles_nested_packages() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("project");
        // Root-level
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("package.json"), "{}").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "ok").unwrap();
        // Nested package
        fs::create_dir_all(root.join("packages/web/node_modules/web-pkg")).unwrap();
        fs::write(root.join("packages/web/package.json"), "{}").unwrap();
        fs::write(
            root.join("packages/web/node_modules/web-pkg/index.js"),
            "ok",
        )
        .unwrap();

        let workdir = temp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();

        symlink_node_modules(&root, &workdir).unwrap();

        assert!(
            workdir
                .join("node_modules")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            workdir
                .join("packages/web/node_modules")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_node_modules_does_not_recurse_into_directory_symlink() {
        use std::os::unix::fs as unix_fs;

        let temp = TempDir::new().unwrap();
        let root = temp.path().join("project");
        let outside = temp.path().join("outside");
        let workdir = temp.path().join("workdir");

        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(outside.join("node_modules/pkg")).unwrap();
        fs::write(outside.join("package.json"), "{}").unwrap();
        fs::write(outside.join("node_modules/pkg/index.js"), "ok").unwrap();
        unix_fs::symlink(&outside, root.join("linked")).unwrap();
        fs::create_dir_all(&workdir).unwrap();

        symlink_node_modules(&root, &workdir).unwrap();

        assert!(!workdir.join("linked/node_modules").exists());
    }

    #[test]
    fn cleanup_workdir_removes_directory() {
        let temp = TempDir::new().unwrap();
        let workdir = temp.path().join("workdir");
        fs::create_dir_all(workdir.join("sub")).unwrap();
        fs::write(workdir.join("sub/file.txt"), "test").unwrap();

        cleanup_workdir(&workdir);

        assert!(!workdir.exists());
    }

    #[test]
    fn cleanup_workdir_noop_for_missing_dir() {
        let temp = TempDir::new().unwrap();
        let workdir = temp.path().join("nonexistent");
        cleanup_workdir(&workdir); // Should not panic
    }

    #[cfg(unix)]
    #[test]
    fn create_workdir_preserves_executable_permission() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("script.sh"), "#!/bin/sh\necho hi").unwrap();
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(root.join("script.sh"), perms).unwrap();

        let workdir = temp.path().join("workdir");
        create_workdir(&root, &workdir).unwrap();

        let mode = fs::metadata(workdir.join("script.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111); // Executable bits preserved
    }
}

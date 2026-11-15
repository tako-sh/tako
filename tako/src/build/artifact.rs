use std::path::{Component, Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use super::BuildError;

fn compile_patterns(
    source_root: &Path,
    patterns: &[String],
    kind: &str,
) -> Result<Option<Gitignore>, BuildError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GitignoreBuilder::new(source_root);
    for pattern in patterns {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            return Err(BuildError::ArchiveError(format!(
                "artifact {} patterns cannot contain empty entries",
                kind
            )));
        }
        builder.add_line(None, trimmed).map_err(|e| {
            BuildError::ArchiveError(format!("invalid {} glob '{}': {}", kind, trimmed, e))
        })?;
    }
    let gitignore = builder.build().map_err(|e| {
        BuildError::ArchiveError(format!("failed to build {} matcher: {}", kind, e))
    })?;
    Ok(Some(gitignore))
}

/// Create an archive from a workdir (already filtered — no gitignore needed).
/// Skips `node_modules/` (symlinks), `.git/`, `.tako/`, `.env*`.
pub fn create_workdir_archive(
    workdir: &Path,
    output_path: &Path,
    include_patterns: &[String],
    exclude_patterns: &[String],
) -> Result<u64, BuildError> {
    let includes = compile_patterns(workdir, include_patterns, "include")?;
    let excludes = compile_patterns(workdir, exclude_patterns, "exclude")?;

    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    // Walk without gitignore — workdir is already filtered
    let mut walker = ignore::WalkBuilder::new(workdir);
    walker
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .require_git(false);

    for entry in walker.build() {
        let entry = entry.map_err(|e| BuildError::ArchiveError(e.to_string()))?;
        let file_type = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        // Skip directories and symlinks (node_modules are symlinks in workdir)
        if !file_type.is_file() {
            continue;
        }

        let path = entry.path();
        let relative_path = path.strip_prefix(workdir).map_err(|e| {
            BuildError::ArchiveError(format!(
                "Failed to compute relative path for {}: {}",
                path.display(),
                e
            ))
        })?;

        if should_workdir_force_exclude(relative_path) {
            continue;
        }

        // Always include the deploy manifest (app.json) regardless of
        // include/exclude patterns — the server needs it to start the app.
        let is_deploy_manifest = relative_path == Path::new("app.json");

        if !is_deploy_manifest {
            if let Some(include_matcher) = &includes
                && !include_matcher
                    .matched_path_or_any_parents(relative_path, false)
                    .is_ignore()
            {
                continue;
            }
            if let Some(exclude_matcher) = &excludes
                && exclude_matcher
                    .matched_path_or_any_parents(relative_path, false)
                    .is_ignore()
            {
                continue;
            }
        }

        files.push((path.to_path_buf(), relative_path.to_path_buf()));
    }

    files.sort_by(|a, b| a.1.cmp(&b.1));

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = std::fs::File::create(output_path)?;
    let encoder = zstd::stream::write::Encoder::new(file, 3).map_err(|e| {
        BuildError::ArchiveError(format!("Failed to initialize zstd encoder: {}", e))
    })?;
    let mut archive = tar::Builder::new(encoder);
    archive.follow_symlinks(false);
    // The upload protocol accepts regular files, including binaries with filesystem holes.
    archive.sparse(false);

    for (full_path, relative_path) in files {
        archive
            .append_path_with_name(&full_path, &relative_path)
            .map_err(|e| {
                BuildError::ArchiveError(format!("Failed to add {}: {}", full_path.display(), e))
            })?;
    }

    let encoder = archive
        .into_inner()
        .map_err(|e| BuildError::ArchiveError(format!("Failed to finish archive: {}", e)))?;
    encoder
        .finish()
        .map_err(|e| BuildError::ArchiveError(format!("Failed to compress: {}", e)))?;

    let metadata = std::fs::metadata(output_path)?;
    Ok(metadata.len())
}

fn should_workdir_force_exclude(relative_path: &Path) -> bool {
    let mut previous_component: Option<&str> = None;
    for component in relative_path.components() {
        if let Component::Normal(name) = component {
            let component_name = name.to_str();
            if previous_component == Some(".next") && component_name == Some("cache") {
                return true;
            }
            match component_name {
                // Version control & project meta
                Some(".git") | Some(".tako") => return true,
                // Dependencies & package caches
                Some("node_modules")
                | Some(".npm")
                | Some(".pnp.cjs")
                | Some(".pnp.loader.mjs") => return true,
                // Build & lint caches
                Some(".turbo")
                | Some(".cache")
                | Some(".parcel-cache")
                | Some(".eslintcache")
                | Some(".stylelintcache") => return true,
                // Test coverage
                Some("coverage") | Some(".nyc_output") => return true,
                // Secrets
                Some(name) if name.starts_with(".env") => return true,
                _ => {}
            }
            previous_component = component_name;
        }
    }
    if let Some(ext) = relative_path.extension().and_then(|e| e.to_str())
        && matches!(ext, "log" | "tsbuildinfo" | "pid" | "lcov")
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::BuildExecutor;
    use std::fs;
    use std::io::Read;
    use std::path::Path;
    use tempfile::TempDir;

    fn assert_zstd_magic(path: &Path) {
        let mut file = fs::File::open(path).unwrap();
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic).unwrap();
        assert_eq!(magic, [0x28, 0xB5, 0x2F, 0xFD], "archive should be zstd");
    }

    #[test]
    fn workdir_archive_encodes_sparse_files_as_regular_files() {
        use std::io::{Seek, SeekFrom, Write};

        let temp = TempDir::new().unwrap();
        let workdir = temp.path().join("workdir");
        let archive = temp.path().join("out.tar.zst");
        fs::create_dir(&workdir).unwrap();
        let path = workdir.join("app");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(b"head").unwrap();
        file.seek(SeekFrom::Start(128 * 1024)).unwrap();
        file.write_all(b"tail").unwrap();
        drop(file);

        create_workdir_archive(&workdir, &archive, &[], &[]).unwrap();
        let stream = zstd::Decoder::new(fs::File::open(archive).unwrap()).unwrap();
        let mut archive = tar::Archive::new(stream);
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(entry.header().entry_type(), tar::EntryType::Regular);
        assert_eq!(entry.path().unwrap().as_ref(), Path::new("app"));
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, fs::read(path).unwrap());
    }

    #[test]
    fn workdir_archive_always_includes_app_json() {
        let temp = TempDir::new().unwrap();
        let workdir = temp.path().join("workdir");
        let archive = temp.path().join("out.tar.zst");
        let dest = temp.path().join("dest");

        fs::create_dir_all(workdir.join("dist")).unwrap();
        fs::write(workdir.join("dist/index.js"), "ok").unwrap();
        fs::write(workdir.join("app.json"), r#"{"main":"index.ts"}"#).unwrap();
        fs::write(workdir.join("README.md"), "readme").unwrap();

        create_workdir_archive(&workdir, &archive, &[String::from("dist/**")], &[]).unwrap();
        assert_zstd_magic(&archive);

        BuildExecutor::extract_archive(&archive, &dest).unwrap();
        assert!(
            dest.join("dist/index.js").exists(),
            "included file should be present"
        );
        assert!(
            dest.join("app.json").exists(),
            "app.json must always be included"
        );
        assert!(
            !dest.join("README.md").exists(),
            "non-included file should be absent"
        );
    }

    #[test]
    fn workdir_archive_excludes_next_cache_and_turbo() {
        let temp = TempDir::new().unwrap();
        let workdir = temp.path().join("workdir");
        let archive = temp.path().join("out.tar.zst");
        let dest = temp.path().join("dest");

        fs::create_dir_all(workdir.join(".next/cache")).unwrap();
        fs::create_dir_all(workdir.join(".next/static")).unwrap();
        fs::create_dir_all(workdir.join(".turbo")).unwrap();
        fs::write(workdir.join(".next/cache/fetch-cache"), "cache").unwrap();
        fs::write(workdir.join(".next/static/chunk.js"), "static").unwrap();
        fs::write(workdir.join(".turbo/state.json"), "turbo").unwrap();
        fs::write(workdir.join("app.json"), r#"{"main":"index.ts"}"#).unwrap();

        create_workdir_archive(&workdir, &archive, &[String::from("**/*")], &[]).unwrap();

        BuildExecutor::extract_archive(&archive, &dest).unwrap();
        assert!(dest.join(".next/static/chunk.js").exists());
        assert!(!dest.join(".next/cache/fetch-cache").exists());
        assert!(!dest.join(".turbo/state.json").exists());
    }
}

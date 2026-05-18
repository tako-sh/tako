//! Build executor - runs build commands and creates archives

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use thiserror::Error;

/// Errors that can occur during build
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Build command failed: {0}")]
    CommandFailed(String),

    #[error("Build command not found: {0}")]
    CommandNotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to create archive: {0}")]
    ArchiveError(String),

    #[error("Git error: {0}")]
    GitError(String),
}

/// Result of running a build command
#[derive(Debug)]
pub struct BuildResult {
    /// Whether the build succeeded
    pub success: bool,
    /// Combined stdout output
    pub stdout: String,
    /// Combined stderr output
    pub stderr: String,
    /// Exit code
    pub exit_code: Option<i32>,
}

/// Build executor
pub struct BuildExecutor {
    /// Working directory
    cwd: PathBuf,
}

impl BuildExecutor {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    /// Run a build command
    pub fn run_build(&self, command: &str) -> Result<BuildResult, BuildError> {
        if command.trim().is_empty() {
            return Err(BuildError::CommandFailed("Empty command".to_string()));
        }

        let output = Command::new("sh")
            .args(["-c", command])
            .current_dir(&self.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(BuildError::Io)?;

        Ok(BuildResult {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code(),
        })
    }

    /// Get the current git commit hash (short form)
    pub fn get_git_commit(&self) -> Result<String, BuildError> {
        let output = Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(&self.cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .map_err(|e| BuildError::GitError(e.to_string()))?;

        if !output.status.success() {
            return Err(BuildError::GitError(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Check if git working tree is dirty (has uncommitted changes)
    pub fn is_git_dirty(&self) -> Result<bool, BuildError> {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&self.cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .map_err(|e| BuildError::GitError(e.to_string()))?;

        if !output.status.success() {
            return Err(BuildError::GitError(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        // If output is non-empty, there are uncommitted changes
        Ok(!output.stdout.is_empty())
    }

    /// Generate version string for deployment
    /// Format: {commit} or {commit}_{content_hash} if dirty
    pub fn generate_version(&self, content_hash: Option<&str>) -> Result<String, BuildError> {
        let commit = match self.get_git_commit() {
            Ok(commit) => commit,
            Err(_) => {
                // Fallback for directories without commits/repos.
                let suffix = if let Some(hash) = content_hash {
                    short_hash(hash).to_string()
                } else {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs().to_string())
                        .unwrap_or_else(|_| "0".to_string())
                };
                return Ok(format!("nogit_{}", suffix));
            }
        };
        let dirty = self.is_git_dirty()?;

        if dirty {
            // Include content hash to differentiate dirty builds
            let hash = content_hash.unwrap_or("dirty");
            Ok(format!("{}_{}", commit, short_hash(hash)))
        } else {
            Ok(commit)
        }
    }

    /// Create a deployment archive (.tar.zst)
    pub fn create_archive(
        &self,
        source_dir: &Path,
        output_path: &Path,
        exclude_patterns: &[&str],
    ) -> Result<u64, BuildError> {
        self.create_archive_with_extra_files(source_dir, output_path, exclude_patterns, &[])
    }

    /// Create a deployment archive (.tar.zst) with additional virtual files.
    pub fn create_archive_with_extra_files(
        &self,
        source_dir: &Path,
        output_path: &Path,
        exclude_patterns: &[&str],
        extra_files: &[(&str, &[u8])],
    ) -> Result<u64, BuildError> {
        use tar::Header;

        // Create parent directory if needed
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = std::fs::File::create(output_path)?;
        let encoder = zstd::stream::write::Encoder::new(file, 3).map_err(|e| {
            BuildError::ArchiveError(format!("Failed to initialize zstd encoder: {}", e))
        })?;
        let mut archive = tar::Builder::new(encoder);
        archive.follow_symlinks(false);

        // Default exclusions
        let default_excludes = [
            ".git",
            "node_modules",
            ".tako",
            "target",
            ".env",
            ".env.local",
            "*.log",
        ];

        // Walk directory and add files
        self.add_dir_to_archive(
            &mut archive,
            source_dir,
            source_dir,
            &default_excludes,
            exclude_patterns,
        )?;

        for (path, bytes) in extra_files {
            let mut header = Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, path, &mut std::io::Cursor::new(*bytes))?;
        }

        let encoder = archive
            .into_inner()
            .map_err(|e| BuildError::ArchiveError(format!("Failed to finish archive: {}", e)))?;

        encoder
            .finish()
            .map_err(|e| BuildError::ArchiveError(format!("Failed to compress: {}", e)))?;

        // Return file size
        let metadata = std::fs::metadata(output_path)?;
        Ok(metadata.len())
    }

    /// Create a source deployment archive (.tar.zst).
    ///
    /// File selection rules:
    /// - Base ignore semantics from `.gitignore`
    /// - Non-overridable excludes for safety/perf: `.git/`, `.tako/`, `.env*`, `node_modules/`, `target/`
    pub fn create_source_archive_with_extra_files(
        &self,
        source_root: &Path,
        output_path: &Path,
        extra_files: &[(&str, &[u8])],
    ) -> Result<u64, BuildError> {
        use tar::Header;

        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = std::fs::File::create(output_path)?;
        let encoder = zstd::stream::write::Encoder::new(file, 3).map_err(|e| {
            BuildError::ArchiveError(format!("Failed to initialize zstd encoder: {}", e))
        })?;
        let mut archive = tar::Builder::new(encoder);
        archive.follow_symlinks(false);

        let files = collect_source_archive_files(source_root)?;

        for (full_path, relative_path) in files {
            archive
                .append_path_with_name(&full_path, &relative_path)
                .map_err(|e| {
                    BuildError::ArchiveError(format!(
                        "Failed to add {}: {}",
                        full_path.display(),
                        e
                    ))
                })?;
        }

        for (path, bytes) in extra_files {
            let mut header = Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, path, &mut std::io::Cursor::new(*bytes))?;
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

    /// Compute SHA256 hash over filtered source payload (same file selection as source archive).
    pub fn compute_source_hash(&self, source_root: &Path) -> Result<String, BuildError> {
        use sha2::{Digest, Sha256};

        let files = collect_source_archive_files(source_root)?;
        let mut hasher = Sha256::new();

        for (full_path, relative_path) in files {
            hasher.update(relative_path.to_string_lossy().as_bytes());
            let metadata = std::fs::symlink_metadata(&full_path)?;
            if metadata.file_type().is_symlink() {
                // Source archives preserve symlinks; hash the link target so the source hash
                // tracks symlink changes without following directory links.
                let target = std::fs::read_link(&full_path)?;
                hasher.update(b"symlink:");
                hasher.update(target.to_string_lossy().as_bytes());
            } else {
                let mut file = std::fs::File::open(&full_path)?;
                let mut buffer = [0u8; 8192];
                loop {
                    let bytes_read = file.read(&mut buffer)?;
                    if bytes_read == 0 {
                        break;
                    }
                    hasher.update(&buffer[..bytes_read]);
                }
            }
        }

        Ok(hex::encode(hasher.finalize()))
    }

    fn add_dir_to_archive<W: Write>(
        &self,
        archive: &mut tar::Builder<W>,
        base_dir: &Path,
        current_dir: &Path,
        default_excludes: &[&str],
        custom_excludes: &[&str],
    ) -> Result<(), BuildError> {
        let entries = std::fs::read_dir(current_dir)?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            let file_name = path.file_name().unwrap().to_string_lossy();

            // Check exclusions
            let should_exclude = default_excludes.iter().any(|p| {
                if let Some(suffix) = p.strip_prefix('*') {
                    file_name.ends_with(suffix)
                } else {
                    file_name == *p
                }
            }) || custom_excludes.iter().any(|p| {
                if let Some(suffix) = p.strip_prefix('*') {
                    file_name.ends_with(suffix)
                } else {
                    file_name == *p
                }
            });

            if should_exclude {
                continue;
            }

            let relative_path = path.strip_prefix(base_dir).unwrap();

            if file_type.is_dir() {
                self.add_dir_to_archive(
                    archive,
                    base_dir,
                    &path,
                    default_excludes,
                    custom_excludes,
                )?;
            } else if file_type.is_file() || file_type.is_symlink() {
                archive
                    .append_path_with_name(&path, relative_path)
                    .map_err(|e| {
                        BuildError::ArchiveError(format!("Failed to add {}: {}", path.display(), e))
                    })?;
            }
        }

        Ok(())
    }

    /// Extract an archive to a directory
    pub fn extract_archive(archive_path: &Path, dest_dir: &Path) -> Result<(), BuildError> {
        std::fs::create_dir_all(dest_dir)?;

        let file = std::fs::File::open(archive_path)?;
        let decoder = zstd::stream::read::Decoder::new(file).map_err(|e| {
            BuildError::ArchiveError(format!("Failed to initialize zstd decoder: {}", e))
        })?;
        let mut archive = tar::Archive::new(decoder);

        archive
            .unpack(dest_dir)
            .map_err(|e| BuildError::ArchiveError(format!("Failed to extract: {}", e)))?;

        Ok(())
    }
}

/// Compute SHA256 hash of file contents
pub fn compute_file_hash(path: &Path) -> Result<String, BuildError> {
    use sha2::{Digest, Sha256};

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    let result = hasher.finalize();
    Ok(hex::encode(result))
}

/// Compute SHA256 hash of directory contents (for dirty detection)
pub fn compute_dir_hash(dir: &Path, exclude_patterns: &[&str]) -> Result<String, BuildError> {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let mut paths: Vec<PathBuf> = Vec::new();

    // Collect all file paths
    collect_files(dir, &mut paths, exclude_patterns)?;

    // Sort for deterministic ordering
    paths.sort();

    // Hash each file's path and content
    for path in paths {
        let relative = path.strip_prefix(dir).unwrap();
        hasher.update(relative.to_string_lossy().as_bytes());

        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?;
            hasher.update(b"symlink:");
            hasher.update(target.to_string_lossy().as_bytes());
        } else {
            let mut file = std::fs::File::open(&path)?;
            let mut buffer = [0u8; 8192];
            loop {
                let bytes_read = file.read(&mut buffer)?;
                if bytes_read == 0 {
                    break;
                }
                hasher.update(&buffer[..bytes_read]);
            }
        }
    }

    let result = hasher.finalize();
    Ok(hex::encode(result))
}

fn short_hash(s: &str) -> &str {
    &s[..8.min(s.len())]
}

fn should_force_exclude_from_source_archive(relative_path: &Path) -> bool {
    for component in relative_path.components() {
        if let Component::Normal(name) = component {
            match name.to_str() {
                Some(".git") | Some(".tako") | Some("node_modules") | Some("target") => {
                    return true;
                }
                Some(name) if name.starts_with(".env") => return true,
                _ => {}
            }
        }
    }
    false
}

fn collect_source_archive_files(source_root: &Path) -> Result<Vec<(PathBuf, PathBuf)>, BuildError> {
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
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
        let file_type = match entry.file_type() {
            Some(file_type) => file_type,
            None => continue,
        };
        if !file_type.is_file() && !file_type.is_symlink() {
            continue;
        }

        let path = entry.path();
        let relative_path = path.strip_prefix(source_root).map_err(|e| {
            BuildError::ArchiveError(format!(
                "Failed to compute relative path for {}: {}",
                path.display(),
                e
            ))
        })?;

        if should_force_exclude_from_source_archive(relative_path) {
            continue;
        }

        files.push((path.to_path_buf(), relative_path.to_path_buf()));
    }

    files.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(files)
}

fn collect_files(
    dir: &Path,
    paths: &mut Vec<PathBuf>,
    exclude_patterns: &[&str],
) -> Result<(), BuildError> {
    let default_excludes = [".git", "node_modules", ".tako", "target"];

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let file_name = path.file_name().unwrap().to_string_lossy();

        // Check exclusions
        let should_exclude = default_excludes.iter().any(|p| file_name == *p)
            || exclude_patterns.iter().any(|p| {
                if let Some(suffix) = p.strip_prefix('*') {
                    file_name.ends_with(suffix)
                } else {
                    file_name == *p
                }
            });

        if should_exclude {
            continue;
        }

        if file_type.is_dir() {
            collect_files(&path, paths, exclude_patterns)?;
        } else if file_type.is_file() || file_type.is_symlink() {
            paths.push(path);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests;

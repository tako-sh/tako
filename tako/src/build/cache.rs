//! Build cache - stores and retrieves build artifacts

use super::executor::{BuildError, compute_dir_hash};
use std::path::{Path, PathBuf};

/// Build cache for storing deployment archives
pub struct BuildCache {
    /// Cache directory (e.g., .tako/build/)
    cache_dir: PathBuf,
}

impl BuildCache {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
        }
    }

    /// Get the cache directory path
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Initialize cache directory
    pub fn init(&self) -> Result<(), BuildError> {
        std::fs::create_dir_all(&self.cache_dir)?;
        Ok(())
    }

    /// Generate a cache key (content hash) for a source directory
    pub fn get_cache_key(&self, source_dir: &Path) -> Result<String, BuildError> {
        compute_dir_hash(source_dir, &[])
    }

    /// Get the archive path for a given version
    pub fn archive_path(&self, version: &str) -> PathBuf {
        self.cache_dir.join(format!("{}.tar.zst", version))
    }

    /// Check if a build exists in cache
    pub fn has_build(&self, version: &str) -> bool {
        self.archive_path(version).exists()
    }

    /// Get cached build if it exists
    pub fn get_cached_build(&self, version: &str) -> Option<PathBuf> {
        let path = self.archive_path(version);
        if path.exists() { Some(path) } else { None }
    }

    /// Store a build in cache (moves the file)
    pub fn store_build(&self, source_path: &Path, version: &str) -> Result<PathBuf, BuildError> {
        self.init()?;
        let dest = self.archive_path(version);
        std::fs::rename(source_path, &dest)?;
        Ok(dest)
    }

    /// Copy a build to cache (keeps original)
    pub fn cache_build(&self, source_path: &Path, version: &str) -> Result<PathBuf, BuildError> {
        self.init()?;
        let dest = self.archive_path(version);
        std::fs::copy(source_path, &dest)?;
        Ok(dest)
    }

    /// List all cached builds
    pub fn list_builds(&self) -> Result<Vec<CachedBuild>, BuildError> {
        if !self.cache_dir.exists() {
            return Ok(vec![]);
        }

        let mut builds = Vec::new();

        for entry in std::fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().map(|e| e == "zst").unwrap_or(false)
                && let Some(name) = path.file_stem()
                && let Some(name) = name.to_str()
            {
                let version = name.strip_suffix(".tar").unwrap_or(name);

                let metadata = std::fs::metadata(&path)?;
                builds.push(CachedBuild {
                    version: version.to_string(),
                    path: path.clone(),
                    size: metadata.len(),
                    created: metadata.created().ok(),
                });
            }
        }

        // Sort by creation time (newest first)
        builds.sort_by_key(|build| std::cmp::Reverse(build.created));

        Ok(builds)
    }

    /// Clean old builds, keeping only the N most recent
    pub fn clean_old_builds(&self, keep_count: usize) -> Result<usize, BuildError> {
        let builds = self.list_builds()?;
        let mut removed = 0;

        for build in builds.into_iter().skip(keep_count) {
            std::fs::remove_file(&build.path)?;
            removed += 1;
        }

        Ok(removed)
    }

    /// Get total cache size in bytes
    pub fn total_size(&self) -> Result<u64, BuildError> {
        let builds = self.list_builds()?;
        Ok(builds.iter().map(|b| b.size).sum())
    }

    /// Clear entire cache
    pub fn clear(&self) -> Result<usize, BuildError> {
        let builds = self.list_builds()?;
        let count = builds.len();

        for build in builds {
            std::fs::remove_file(&build.path)?;
        }

        Ok(count)
    }
}

/// Information about a cached build
#[derive(Debug, Clone)]
pub struct CachedBuild {
    /// Version identifier
    pub version: String,
    /// Path to the archive
    pub path: PathBuf,
    /// Size in bytes
    pub size: u64,
    /// Creation time
    pub created: Option<std::time::SystemTime>,
}

impl CachedBuild {
    /// Format size as human-readable string
    pub fn size_human(&self) -> String {
        if self.size < 1024 {
            format!("{} B", self.size)
        } else if self.size < 1024 * 1024 {
            format!("{:.1} KB", self.size as f64 / 1024.0)
        } else if self.size < 1024 * 1024 * 1024 {
            format!("{:.1} MB", self.size as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.1} GB", self.size as f64 / (1024.0 * 1024.0 * 1024.0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_cache_init() {
        let temp = TempDir::new().unwrap();
        let cache = BuildCache::new(temp.path().join("builds"));

        assert!(!cache.cache_dir().exists());
        cache.init().unwrap();
        assert!(cache.cache_dir().exists());
    }

    #[test]
    fn test_archive_path() {
        let temp = TempDir::new().unwrap();
        let cache = BuildCache::new(temp.path().join("builds"));

        let path = cache.archive_path("abc1234");
        assert!(path.ends_with("abc1234.tar.zst"));
    }

    #[test]
    fn test_has_build() {
        let temp = TempDir::new().unwrap();
        let cache = BuildCache::new(temp.path().join("builds"));
        cache.init().unwrap();

        assert!(!cache.has_build("abc1234"));

        // Create a fake build
        fs::write(cache.archive_path("abc1234"), "fake archive").unwrap();
        assert!(cache.has_build("abc1234"));
    }

    #[test]
    fn test_get_cached_build() {
        let temp = TempDir::new().unwrap();
        let cache = BuildCache::new(temp.path().join("builds"));
        cache.init().unwrap();

        assert!(cache.get_cached_build("abc1234").is_none());

        fs::write(cache.archive_path("abc1234"), "fake archive").unwrap();
        let path = cache.get_cached_build("abc1234").unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_store_build() {
        let temp = TempDir::new().unwrap();
        let cache = BuildCache::new(temp.path().join("builds"));

        let source = temp.path().join("source.tar.zst");
        fs::write(&source, "archive content").unwrap();

        let dest = cache.store_build(&source, "def5678").unwrap();
        assert!(dest.exists());
        assert!(!source.exists()); // Source was moved
        assert_eq!(fs::read_to_string(&dest).unwrap(), "archive content");
    }

    #[test]
    fn test_list_builds() {
        let temp = TempDir::new().unwrap();
        let cache = BuildCache::new(temp.path().join("builds"));
        cache.init().unwrap();

        // Empty cache
        assert!(cache.list_builds().unwrap().is_empty());

        // Add some builds
        fs::write(cache.archive_path("v1"), "build1").unwrap();
        fs::write(cache.archive_path("v2"), "build22").unwrap();
        fs::write(cache.archive_path("v3"), "build333").unwrap();

        let builds = cache.list_builds().unwrap();
        assert_eq!(builds.len(), 3);
    }

    #[test]
    fn test_clean_old_builds() {
        let temp = TempDir::new().unwrap();
        let cache = BuildCache::new(temp.path().join("builds"));
        cache.init().unwrap();

        // Create 5 builds with different times
        for i in 1..=5 {
            let path = cache.archive_path(&format!("v{}", i));
            fs::write(&path, format!("build{}", i)).unwrap();
            // Add small delay to ensure different timestamps
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert_eq!(cache.list_builds().unwrap().len(), 5);

        // Keep only 2
        let removed = cache.clean_old_builds(2).unwrap();
        assert_eq!(removed, 3);
        assert_eq!(cache.list_builds().unwrap().len(), 2);
    }

    #[test]
    fn test_total_size() {
        let temp = TempDir::new().unwrap();
        let cache = BuildCache::new(temp.path().join("builds"));
        cache.init().unwrap();

        fs::write(cache.archive_path("v1"), "12345").unwrap(); // 5 bytes
        fs::write(cache.archive_path("v2"), "1234567890").unwrap(); // 10 bytes

        let size = cache.total_size().unwrap();
        assert_eq!(size, 15);
    }

    #[test]
    fn test_clear_cache() {
        let temp = TempDir::new().unwrap();
        let cache = BuildCache::new(temp.path().join("builds"));
        cache.init().unwrap();

        fs::write(cache.archive_path("v1"), "build1").unwrap();
        fs::write(cache.archive_path("v2"), "build2").unwrap();

        let cleared = cache.clear().unwrap();
        assert_eq!(cleared, 2);
        assert!(cache.list_builds().unwrap().is_empty());
    }

    #[test]
    fn test_size_human() {
        let build = CachedBuild {
            version: "v1".to_string(),
            path: PathBuf::from("/tmp/v1.tar.zst"),
            size: 500,
            created: None,
        };
        assert_eq!(build.size_human(), "500 B");

        let build = CachedBuild {
            version: "v2".to_string(),
            path: PathBuf::from("/tmp/v2.tar.zst"),
            size: 2048,
            created: None,
        };
        assert_eq!(build.size_human(), "2.0 KB");

        let build = CachedBuild {
            version: "v3".to_string(),
            path: PathBuf::from("/tmp/v3.tar.zst"),
            size: 5 * 1024 * 1024,
            created: None,
        };
        assert_eq!(build.size_human(), "5.0 MB");
    }
}

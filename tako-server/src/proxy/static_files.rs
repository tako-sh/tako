//! Static file serving
//!
//! Serves static files directly from the proxy for configured apps.
//! Files are served from the app's `public/` directory.

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

/// Errors that can occur during static file serving
#[derive(Debug, Error)]
pub enum StaticFileError {
    #[error("File not found: {0}")]
    NotFound(String),

    #[error("Path traversal detected: {0}")]
    PathTraversal(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid path: {0}")]
    InvalidPath(String),
}

/// Configuration for static file serving
#[derive(Debug, Clone)]
pub struct StaticConfig {
    /// Whether to enable static file serving
    pub enabled: bool,
    /// Name of the public directory (relative to app root)
    pub public_dir: String,
    /// Cache-Control max-age in seconds for static files
    pub cache_max_age: u64,
    /// Whether to serve index.html for directories
    pub serve_index: bool,
}

impl Default for StaticConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            public_dir: "public".to_string(),
            cache_max_age: 3600, // 1 hour
            serve_index: true,
        }
    }
}

/// A static file ready to be served
#[derive(Debug)]
pub struct StaticFile {
    /// Opened during resolution; callers must never reopen the pathname.
    pub handle: File,
    /// Full path to the file
    pub path: PathBuf,
    /// MIME type
    pub content_type: String,
    /// File size in bytes
    pub size: u64,
    /// ETag (based on size and modified time)
    pub etag: String,
    /// Cache-Control header value
    pub cache_control: String,
}

/// Static file server for an app
pub struct AppStaticServer {
    /// Root directory for static files
    root: PathBuf,
    /// Configuration
    config: StaticConfig,
    /// MIME type map
    mime_types: HashMap<String, String>,
}

impl AppStaticServer {
    pub fn new(app_root: PathBuf, config: StaticConfig) -> Self {
        let root = app_root.join(&config.public_dir);

        Self {
            root,
            config,
            mime_types: Self::default_mime_types(),
        }
    }

    /// Get default MIME type mappings
    fn default_mime_types() -> HashMap<String, String> {
        let mut map = HashMap::new();

        // Text types
        map.insert("html".to_string(), "text/html; charset=utf-8".to_string());
        map.insert("htm".to_string(), "text/html; charset=utf-8".to_string());
        map.insert("css".to_string(), "text/css; charset=utf-8".to_string());
        map.insert(
            "js".to_string(),
            "application/javascript; charset=utf-8".to_string(),
        );
        map.insert("mjs".to_string(), "application/javascript".to_string());
        map.insert("json".to_string(), "application/json".to_string());
        map.insert("txt".to_string(), "text/plain; charset=utf-8".to_string());
        map.insert("xml".to_string(), "application/xml".to_string());
        map.insert("csv".to_string(), "text/csv".to_string());

        // Images
        map.insert("png".to_string(), "image/png".to_string());
        map.insert("jpg".to_string(), "image/jpeg".to_string());
        map.insert("jpeg".to_string(), "image/jpeg".to_string());
        map.insert("gif".to_string(), "image/gif".to_string());
        map.insert("svg".to_string(), "image/svg+xml".to_string());
        map.insert("ico".to_string(), "image/x-icon".to_string());
        map.insert("webp".to_string(), "image/webp".to_string());
        map.insert("avif".to_string(), "image/avif".to_string());

        // Fonts
        map.insert("woff".to_string(), "font/woff".to_string());
        map.insert("woff2".to_string(), "font/woff2".to_string());
        map.insert("ttf".to_string(), "font/ttf".to_string());
        map.insert(
            "eot".to_string(),
            "application/vnd.ms-fontobject".to_string(),
        );
        map.insert("otf".to_string(), "font/otf".to_string());

        // Video/Audio
        map.insert("mp4".to_string(), "video/mp4".to_string());
        map.insert("webm".to_string(), "video/webm".to_string());
        map.insert("mp3".to_string(), "audio/mpeg".to_string());
        map.insert("wav".to_string(), "audio/wav".to_string());
        map.insert("ogg".to_string(), "audio/ogg".to_string());

        // Documents
        map.insert("pdf".to_string(), "application/pdf".to_string());

        // Source maps
        map.insert("map".to_string(), "application/json".to_string());

        // Manifest
        map.insert(
            "webmanifest".to_string(),
            "application/manifest+json".to_string(),
        );

        map
    }

    /// Check if this server has static files enabled and the directory exists
    pub fn is_available(&self) -> bool {
        self.config.enabled && self.root.is_dir()
    }

    /// Get the MIME type for a file extension
    fn get_mime_type(&self, extension: &str) -> String {
        self.mime_types
            .get(extension)
            .cloned()
            .unwrap_or_else(|| "application/octet-stream".to_string())
    }

    /// Resolve a request path to a file
    pub fn resolve(&self, request_path: &str) -> Result<StaticFile, StaticFileError> {
        // Normalize the path
        let clean_path = self.normalize_path(request_path)?;

        // Walk directory descriptors without following symlinks. Keeping the
        // opened file also closes the check/open race with app-writable releases.
        let root = open_without_symlinks(&self.root)
            .map_err(|error| map_open_error(error, request_path))?;
        let mut handle = open_relative(&root, Path::new(&clean_path))
            .map_err(|error| map_open_error(error, request_path))?;
        let mut target_path = self.root.join(&clean_path);
        if handle.metadata()?.is_dir() && self.config.serve_index {
            handle = open_relative(&handle, Path::new("index.html"))
                .map_err(|error| map_open_error(error, request_path))?;
            target_path.push("index.html");
        }
        let metadata = handle.metadata()?;
        if !metadata.is_file() {
            return Err(StaticFileError::NotFound(request_path.to_string()));
        }
        let size = metadata.len();
        let last_modified = metadata.modified().unwrap_or(SystemTime::now());

        // Get extension and MIME type
        let extension = target_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let content_type = self.get_mime_type(&extension);

        // Generate ETag
        let etag = self.generate_etag(size, &last_modified);

        // Build cache control header
        let cache_control = format!("public, max-age={}", self.config.cache_max_age);

        Ok(StaticFile {
            handle,
            path: target_path,
            content_type,
            size,
            etag,
            cache_control,
        })
    }

    /// Normalize a URL path (remove leading slash, percent-decode, reject traversal)
    fn normalize_path(&self, path: &str) -> Result<String, StaticFileError> {
        // Remove leading slash
        let path = path.trim_start_matches('/');

        // Full percent-decode so encoded traversal sequences (%2e%2e) are caught
        let decoded = percent_encoding::percent_decode_str(path)
            .decode_utf8()
            .map_err(|_| StaticFileError::InvalidPath("invalid UTF-8 in path".to_string()))?;

        // Reject null bytes (after decoding so %00 is caught)
        if decoded.contains('\0') {
            return Err(StaticFileError::InvalidPath(
                "null byte in path".to_string(),
            ));
        }

        // Check for path traversal attempts (after decoding)
        if decoded.contains("..") {
            return Err(StaticFileError::PathTraversal(decoded.to_string()));
        }

        Ok(decoded.to_string())
    }

    /// Generate an ETag for caching
    fn generate_etag(&self, size: u64, modified: &SystemTime) -> String {
        let modified_secs = modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        format!("\"{}-{}\"", size, modified_secs)
    }

    /// Get root directory
    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn map_open_error(error: std::io::Error, path: &str) -> StaticFileError {
    match error.raw_os_error() {
        Some(libc::ELOOP) => StaticFileError::PathTraversal(path.into()),
        Some(libc::ENOENT | libc::ENOTDIR) => StaticFileError::NotFound(path.into()),
        _ => StaticFileError::Io(error),
    }
}

fn open_without_symlinks(path: &Path) -> std::io::Result<File> {
    let absolute = std::path::absolute(path)?;
    open_relative(&File::open("/")?, absolute.strip_prefix("/").unwrap())
}

fn open_relative(root: &File, path: &Path) -> std::io::Result<File> {
    let mut file = root.try_clone()?;
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid static path",
            ));
        };
        let name = std::ffi::CString::new(name.as_bytes())?;
        // O_NONBLOCK prevents a planted FIFO from blocking the proxy. The final
        // descriptor is checked for regular-file type before it is served.
        // SAFETY: file owns a live directory fd and name is a NUL-terminated component.
        let fd = unsafe {
            libc::openat(
                file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat returned a new descriptor whose ownership moves into File.
        file = unsafe { File::from_raw_fd(fd) };
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_files(dir: &Path) {
        let public = dir.join("public");
        fs::create_dir_all(&public).unwrap();

        // Create test files
        fs::write(public.join("index.html"), "<html></html>").unwrap();
        fs::write(public.join("style.css"), "body { }").unwrap();
        fs::write(public.join("app.js"), "console.log()").unwrap();
        fs::write(public.join("logo.png"), [0x89, 0x50, 0x4E, 0x47]).unwrap();

        // Create subdirectory
        let sub = public.join("assets");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("image.jpg"), [0xFF, 0xD8, 0xFF]).unwrap();
    }

    #[test]
    fn test_static_config_default() {
        let config = StaticConfig::default();
        assert!(config.enabled);
        assert_eq!(config.public_dir, "public");
        assert_eq!(config.cache_max_age, 3600);
        assert!(config.serve_index);
    }

    #[test]
    fn test_resolve_index_html() {
        let temp = TempDir::new().unwrap();
        create_test_files(temp.path());

        let config = StaticConfig::default();
        let server = AppStaticServer::new(temp.path().to_path_buf(), config);

        assert!(server.is_available());

        let file = server.resolve("/").unwrap();
        assert!(file.content_type.contains("text/html"));
        assert!(file.path.ends_with("index.html"));
    }

    #[test]
    fn test_resolve_css_file() {
        let temp = TempDir::new().unwrap();
        create_test_files(temp.path());

        let config = StaticConfig::default();
        let server = AppStaticServer::new(temp.path().to_path_buf(), config);

        let file = server.resolve("/style.css").unwrap();
        assert!(file.content_type.contains("text/css"));
    }

    #[test]
    fn test_resolve_js_file() {
        let temp = TempDir::new().unwrap();
        create_test_files(temp.path());

        let config = StaticConfig::default();
        let server = AppStaticServer::new(temp.path().to_path_buf(), config);

        let file = server.resolve("/app.js").unwrap();
        assert!(file.content_type.contains("javascript"));
    }

    #[test]
    fn test_resolve_image_file() {
        let temp = TempDir::new().unwrap();
        create_test_files(temp.path());

        let config = StaticConfig::default();
        let server = AppStaticServer::new(temp.path().to_path_buf(), config);

        let file = server.resolve("/logo.png").unwrap();
        assert_eq!(file.content_type, "image/png");
    }

    #[test]
    fn test_resolve_subdirectory_file() {
        let temp = TempDir::new().unwrap();
        create_test_files(temp.path());

        let config = StaticConfig::default();
        let server = AppStaticServer::new(temp.path().to_path_buf(), config);

        let file = server.resolve("/assets/image.jpg").unwrap();
        assert_eq!(file.content_type, "image/jpeg");
    }

    #[test]
    fn test_resolve_not_found() {
        let temp = TempDir::new().unwrap();
        create_test_files(temp.path());

        let config = StaticConfig::default();
        let server = AppStaticServer::new(temp.path().to_path_buf(), config);

        let result = server.resolve("/nonexistent.txt");
        assert!(matches!(result, Err(StaticFileError::NotFound(_))));
    }

    #[test]
    fn test_path_traversal_blocked() {
        let temp = TempDir::new().unwrap();
        create_test_files(temp.path());

        let config = StaticConfig::default();
        let server = AppStaticServer::new(temp.path().to_path_buf(), config);

        let result = server.resolve("/../../../etc/passwd");
        assert!(matches!(result, Err(StaticFileError::PathTraversal(_))));
    }

    #[test]
    fn rejects_index_and_directory_symlinks_outside_public() {
        let temp = TempDir::new().unwrap();
        create_test_files(temp.path());
        let private = temp.path().join("private");
        fs::create_dir(&private).unwrap();
        fs::write(private.join("secret.txt"), "private fixture").unwrap();
        let public = temp.path().join("public");
        fs::create_dir(public.join("leak.css")).unwrap();
        std::os::unix::fs::symlink(
            private.join("secret.txt"),
            public.join("leak.css/index.html"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&private, public.join("linked")).unwrap();
        let server = AppStaticServer::new(temp.path().into(), StaticConfig::default());
        assert!(server.resolve("/leak.css").is_err());
        assert!(server.resolve("/linked/secret.txt").is_err());
    }

    #[test]
    fn rejects_symlinked_public_root() {
        let temp = TempDir::new().unwrap();
        let private = temp.path().join("private");
        fs::create_dir(&private).unwrap();
        fs::write(private.join("secret.txt"), "private fixture").unwrap();
        std::os::unix::fs::symlink(private, temp.path().join("public")).unwrap();
        let server = AppStaticServer::new(temp.path().into(), StaticConfig::default());
        assert!(server.resolve("/secret.txt").is_err());
    }

    #[test]
    fn opened_static_file_survives_path_replacement() {
        use std::io::Read;
        let temp = TempDir::new().unwrap();
        create_test_files(temp.path());
        let server = AppStaticServer::new(temp.path().into(), StaticConfig::default());
        let mut file = server.resolve("/style.css").unwrap();
        fs::remove_file(&file.path).unwrap();
        let private = temp.path().join("private.txt");
        fs::write(&private, "private fixture").unwrap();
        std::os::unix::fs::symlink(private, &file.path).unwrap();
        let mut body = String::new();
        file.handle.read_to_string(&mut body).unwrap();
        assert_eq!(body, "body { }");
    }

    #[test]
    fn test_etag_generation() {
        let temp = TempDir::new().unwrap();
        create_test_files(temp.path());

        let config = StaticConfig::default();
        let server = AppStaticServer::new(temp.path().to_path_buf(), config);

        let file = server.resolve("/index.html").unwrap();
        assert!(file.etag.starts_with('"'));
        assert!(file.etag.ends_with('"'));
        // Size and mtime are separated so distinct (size, mtime) pairs
        // cannot collide on concatenation.
        assert!(file.etag.contains('-'));
    }

    #[test]
    fn test_cache_control_header() {
        let temp = TempDir::new().unwrap();
        create_test_files(temp.path());

        let config = StaticConfig {
            cache_max_age: 7200,
            ..Default::default()
        };
        let server = AppStaticServer::new(temp.path().to_path_buf(), config);

        let file = server.resolve("/index.html").unwrap();
        assert!(file.cache_control.contains("max-age=7200"));
    }

    #[test]
    fn test_mime_types() {
        let temp = TempDir::new().unwrap();
        let config = StaticConfig::default();
        let server = AppStaticServer::new(temp.path().to_path_buf(), config);

        // Test various MIME types
        assert!(server.get_mime_type("html").contains("text/html"));
        assert!(server.get_mime_type("css").contains("text/css"));
        assert!(server.get_mime_type("js").contains("javascript"));
        assert_eq!(server.get_mime_type("png"), "image/png");
        assert_eq!(server.get_mime_type("jpg"), "image/jpeg");
        assert_eq!(server.get_mime_type("svg"), "image/svg+xml");
        assert_eq!(server.get_mime_type("woff2"), "font/woff2");
        assert_eq!(server.get_mime_type("pdf"), "application/pdf");
        assert_eq!(server.get_mime_type("unknown"), "application/octet-stream");
    }

    #[test]
    fn test_disabled_static_files() {
        let temp = TempDir::new().unwrap();
        create_test_files(temp.path());

        let config = StaticConfig {
            enabled: false,
            ..Default::default()
        };
        let server = AppStaticServer::new(temp.path().to_path_buf(), config);

        assert!(!server.is_available());
    }
}

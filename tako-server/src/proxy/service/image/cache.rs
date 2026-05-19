use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tako_images::{ImageCrop, ImageFit, OutputFormat, TransformOptions};
use tokio::io::AsyncWriteExt;

const CACHE_VERSION: &str = "tako-image-transform-v1";
const CACHE_DIR_NAME: &str = "tako-image-cache";

#[derive(Debug, PartialEq, Eq)]
pub(super) struct CachedTransform {
    pub(super) bytes: Vec<u8>,
    pub(super) content_type: &'static str,
}

pub(super) fn default_cache_root() -> PathBuf {
    std::env::temp_dir().join(CACHE_DIR_NAME)
}

pub(super) fn transform_cache_key(source: &[u8], options: &TransformOptions) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CACHE_VERSION.as_bytes());
    hasher.update(b"\nsource\n");
    hasher.update(source);
    hasher.update(b"\nformat\n");
    hasher.update(output_format_code(options.format).as_bytes());
    hasher.update(b"\nwidth\n");
    hasher.update(options.width.to_be_bytes());
    hasher.update(b"\nheight\n");
    hasher.update(options.height.unwrap_or_default().to_be_bytes());
    hasher.update(b"\nfit\n");
    hasher.update(options.fit.map(fit_code).unwrap_or("").as_bytes());
    hasher.update(b"\ncrop\n");
    hasher.update(options.crop.map(crop_code).unwrap_or("").as_bytes());
    hasher.update(b"\nquality\n");
    hasher.update([options.quality]);
    hex::encode(hasher.finalize())
}

pub(super) async fn read(root: &Path, key: &str, format: OutputFormat) -> Option<CachedTransform> {
    let path = cache_path(root, key)?;
    let bytes = tokio::fs::read(path).await.ok()?;
    Some(CachedTransform {
        bytes,
        content_type: content_type_for_format(format),
    })
}

pub(super) async fn write(root: &Path, key: &str, bytes: &[u8]) {
    let Some(path) = cache_path(root, key) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if tokio::fs::create_dir_all(parent).await.is_err() {
        return;
    }
    let tmp_id = TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = path.with_extension(format!("tmp-{}-{tmp_id}", std::process::id()));
    let mut file = match tokio::fs::File::create(&tmp_path).await {
        Ok(file) => file,
        Err(_) => return,
    };
    if file.write_all(bytes).await.is_err() || file.flush().await.is_err() {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return;
    }
    drop(file);
    if tokio::fs::rename(&tmp_path, &path).await.is_err() {
        let _ = tokio::fs::remove_file(&tmp_path).await;
    }
}

static TMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn cache_path(root: &Path, key: &str) -> Option<PathBuf> {
    if key.len() < 4 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(root.join(&key[..2]).join(&key[2..]))
}

fn output_format_code(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Avif => "avif",
        OutputFormat::Webp => "webp",
    }
}

fn content_type_for_format(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Avif => "image/avif",
        OutputFormat::Webp => "image/webp",
    }
}

fn fit_code(fit: ImageFit) -> &'static str {
    match fit {
        ImageFit::Cover => "cover",
        ImageFit::Contain => "contain",
    }
}

fn crop_code(crop: ImageCrop) -> &'static str {
    match crop {
        ImageCrop::Center => "center",
        ImageCrop::Smart => "smart",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(format: OutputFormat, width: u32) -> TransformOptions {
        TransformOptions {
            format,
            width,
            height: None,
            fit: None,
            crop: None,
            quality: 75,
        }
    }

    #[test]
    fn default_cache_root_uses_system_temp_directory() {
        assert_eq!(
            default_cache_root(),
            std::env::temp_dir().join(CACHE_DIR_NAME)
        );
    }

    #[test]
    fn cache_key_changes_with_source_bytes() {
        let left = transform_cache_key(b"first", &options(OutputFormat::Avif, 640));
        let right = transform_cache_key(b"second", &options(OutputFormat::Avif, 640));

        assert_ne!(left, right);
    }

    #[test]
    fn cache_key_changes_with_transform_options() {
        let left = transform_cache_key(b"source", &options(OutputFormat::Avif, 640));
        let right = transform_cache_key(b"source", &options(OutputFormat::Webp, 640));

        assert_ne!(left, right);
    }

    #[tokio::test]
    async fn cache_write_then_read_returns_bytes_with_transform_content_type() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = transform_cache_key(b"source", &options(OutputFormat::Webp, 640));

        write(temp.path(), &key, b"cached-image").await;
        let cached = read(temp.path(), &key, OutputFormat::Webp)
            .await
            .expect("cached transform");

        assert_eq!(
            cached,
            CachedTransform {
                bytes: b"cached-image".to_vec(),
                content_type: "image/webp",
            }
        );
    }
}

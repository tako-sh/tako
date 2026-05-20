use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime};
use tako_images::{ImageCrop, ImageFit, OutputFormat, TransformOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

const CACHE_VERSION: &str = "tako-image-transform-v1";
const CACHE_DIR_NAME: &str = "tako-image-cache";
const GIB: u64 = 1024 * 1024 * 1024;
const TRANSFORM_CACHE_MIN_BYTES: u64 = GIB;
const TRANSFORM_CACHE_FALLBACK_BYTES: u64 = 2 * GIB;
const TRANSFORM_CACHE_MAX_BYTES: u64 = 4 * GIB;
const TRANSFORM_CACHE_FILESYSTEM_FRACTION: u64 = 20;
const TRANSFORM_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, PartialEq, Eq)]
pub(super) struct CachedTransform {
    pub(super) bytes: Vec<u8>,
    pub(super) content_type: &'static str,
}

pub(super) fn default_cache_root() -> PathBuf {
    std::env::temp_dir().join(CACHE_DIR_NAME)
}

pub(super) fn transform_cache_key(
    app_name: &str,
    app_root: &Path,
    source: &[u8],
    options: &TransformOptions,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CACHE_VERSION.as_bytes());
    hasher.update(b"\napp\n");
    hasher.update(app_name.as_bytes());
    hasher.update(b"\nroot\n");
    hasher.update(app_root.to_string_lossy().as_bytes());
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
    if tokio::fs::rename(&tmp_path, &path).await.is_ok() {
        prune_with_policy(root, TransformCachePolicy::for_root(root)).await;
    } else {
        let _ = tokio::fs::remove_file(&tmp_path).await;
    }
}

static TMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct TransformCachePolicy {
    max_bytes: u64,
    max_age: Duration,
}

impl TransformCachePolicy {
    fn for_root(root: &Path) -> Self {
        Self {
            max_bytes: transform_cache_max_bytes(root),
            max_age: TRANSFORM_CACHE_MAX_AGE,
        }
    }
}

#[derive(Debug)]
struct CacheFile {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
}

async fn prune_with_policy(root: &Path, policy: TransformCachePolicy) {
    let mut files = collect_cache_files(root).await;
    let now = SystemTime::now();
    let mut retained = Vec::with_capacity(files.len());
    let mut total_bytes = 0_u64;

    for file in files.drain(..) {
        if cache_file_is_expired(&file, now, policy.max_age) {
            let _ = tokio::fs::remove_file(&file.path).await;
        } else {
            total_bytes = total_bytes.saturating_add(file.len);
            retained.push(file);
        }
    }

    if total_bytes > policy.max_bytes {
        retained.sort_by_key(|file| file.modified);
        for file in retained {
            if total_bytes <= policy.max_bytes {
                break;
            }
            if tokio::fs::remove_file(&file.path).await.is_ok() {
                total_bytes = total_bytes.saturating_sub(file.len);
            }
        }
    }

    remove_empty_cache_dirs(root).await;
}

fn cache_file_is_expired(file: &CacheFile, now: SystemTime, max_age: Duration) -> bool {
    now.duration_since(file.modified).unwrap_or_default() >= max_age
}

fn transform_cache_max_bytes(root: &Path) -> u64 {
    filesystem_total_bytes(root)
        .map(transform_cache_max_bytes_for_filesystem_bytes)
        .unwrap_or(TRANSFORM_CACHE_FALLBACK_BYTES)
}

fn transform_cache_max_bytes_for_filesystem_bytes(total_bytes: u64) -> u64 {
    total_bytes
        .saturating_div(TRANSFORM_CACHE_FILESYSTEM_FRACTION)
        .clamp(TRANSFORM_CACHE_MIN_BYTES, TRANSFORM_CACHE_MAX_BYTES)
}

#[cfg(unix)]
fn filesystem_total_bytes(path: &Path) -> Result<u64, String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = nearest_existing_path(path).ok_or_else(|| {
        format!(
            "no existing parent found for transform cache path {}",
            path.display()
        )
    })?;
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("path contains interior nul: {}", path.display()))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_blocks as u64).saturating_mul(stat.f_frsize))
}

#[cfg(not(unix))]
fn filesystem_total_bytes(_path: &Path) -> Result<u64, String> {
    Err("filesystem size checks require Unix".to_string())
}

fn nearest_existing_path(path: &Path) -> Option<&Path> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.as_os_str().is_empty() {
            return None;
        }
        if candidate.exists() {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

async fn collect_cache_files(root: &Path) -> Vec<CacheFile> {
    let mut files = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return files;
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };

        if !metadata.is_dir() {
            continue;
        }

        let Ok(mut children) = tokio::fs::read_dir(&path).await else {
            continue;
        };
        while let Ok(Some(child)) = children.next_entry().await {
            let child_path = child.path();
            let Ok(child_metadata) = child.metadata().await else {
                continue;
            };
            if child_metadata.is_file() && is_cache_file_path(root, &child_path) {
                files.push(cache_file(child_path, child_metadata));
            }
        }
    }

    files
}

fn cache_file(path: PathBuf, metadata: std::fs::Metadata) -> CacheFile {
    CacheFile {
        path,
        len: metadata.len(),
        modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    }
}

fn is_cache_file_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };

    let mut components = relative.components();
    let Some(std::path::Component::Normal(prefix)) = components.next() else {
        return false;
    };
    let Some(std::path::Component::Normal(suffix)) = components.next() else {
        return false;
    };
    if components.next().is_some() {
        return false;
    }

    let Some(prefix) = prefix.to_str() else {
        return false;
    };
    let Some(suffix) = suffix.to_str() else {
        return false;
    };

    prefix.len() == 2
        && !suffix.is_empty()
        && prefix.len() + suffix.len() >= 4
        && prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
        && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn remove_empty_cache_dirs(root: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return;
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        if metadata.is_dir() {
            let _ = tokio::fs::remove_dir(path).await;
        }
    }
}

pub(super) enum TransformLease {
    Owner(TransformOwner),
    Waiter(TransformWaiter),
}

pub(super) struct TransformOwner {
    key: String,
    entry: Arc<InFlightTransform>,
}

impl Drop for TransformOwner {
    fn drop(&mut self) {
        let mut transforms = lock_in_flight_transforms();
        if transforms
            .get(&self.key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
        {
            transforms.remove(&self.key);
        }
        let _ = self.entry.done.send(true);
    }
}

pub(super) struct TransformWaiter {
    done: watch::Receiver<bool>,
}

impl TransformWaiter {
    pub(super) async fn wait(mut self) {
        while !*self.done.borrow_and_update() {
            if self.done.changed().await.is_err() {
                break;
            }
        }
    }
}

struct InFlightTransform {
    done: watch::Sender<bool>,
}

pub(super) fn acquire_transform_lease(key: &str) -> TransformLease {
    let mut transforms = lock_in_flight_transforms();
    if let Some(entry) = transforms.get(key) {
        return TransformLease::Waiter(TransformWaiter {
            done: entry.done.subscribe(),
        });
    }

    let (done, _receiver) = watch::channel(false);
    let entry = Arc::new(InFlightTransform { done });
    transforms.insert(key.to_string(), entry.clone());
    TransformLease::Owner(TransformOwner {
        key: key.to_string(),
        entry,
    })
}

fn in_flight_transforms() -> &'static Mutex<HashMap<String, Arc<InFlightTransform>>> {
    static TRANSFORMS: OnceLock<Mutex<HashMap<String, Arc<InFlightTransform>>>> = OnceLock::new();
    TRANSFORMS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_in_flight_transforms() -> MutexGuard<'static, HashMap<String, Arc<InFlightTransform>>> {
    match in_flight_transforms().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

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
    use tokio::time::{Duration, timeout};

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

    fn test_key(name: &str) -> String {
        format!(
            "test-{name}-{}-{}",
            std::process::id(),
            TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn test_app_root() -> &'static Path {
        Path::new("/opt/tako/apps/demo/production/releases/v1")
    }

    async fn cache_total_bytes(root: &Path) -> u64 {
        collect_cache_files(root)
            .await
            .into_iter()
            .map(|file| file.len)
            .sum()
    }

    #[test]
    fn duplicate_transform_lease_waits_for_existing_owner() {
        let key = test_key("duplicate");
        let _owner = match acquire_transform_lease(&key) {
            TransformLease::Owner(owner) => owner,
            TransformLease::Waiter(_) => panic!("first lease should own transform"),
        };

        match acquire_transform_lease(&key) {
            TransformLease::Owner(_) => panic!("duplicate lease should wait"),
            TransformLease::Waiter(_) => {}
        }
    }

    #[test]
    fn different_transform_lease_keys_get_independent_owners() {
        let left_key = test_key("left");
        let right_key = test_key("right");
        let _left = match acquire_transform_lease(&left_key) {
            TransformLease::Owner(owner) => owner,
            TransformLease::Waiter(_) => panic!("left lease should own transform"),
        };

        match acquire_transform_lease(&right_key) {
            TransformLease::Owner(_) => {}
            TransformLease::Waiter(_) => panic!("right lease should own transform"),
        }
    }

    #[tokio::test]
    async fn transform_waiter_completes_after_owner_drops() {
        let key = test_key("wait");
        let owner = match acquire_transform_lease(&key) {
            TransformLease::Owner(owner) => owner,
            TransformLease::Waiter(_) => panic!("first lease should own transform"),
        };
        let waiter = match acquire_transform_lease(&key) {
            TransformLease::Owner(_) => panic!("duplicate lease should wait"),
            TransformLease::Waiter(waiter) => waiter,
        };
        let mut waiting = tokio::spawn(waiter.wait());

        assert!(
            timeout(Duration::from_millis(25), &mut waiting)
                .await
                .is_err()
        );

        drop(owner);
        timeout(Duration::from_secs(1), waiting)
            .await
            .expect("waiter should complete after owner drop")
            .expect("waiter task");
    }

    #[test]
    fn transform_lease_owner_drop_releases_key() {
        let key = test_key("release");
        let owner = match acquire_transform_lease(&key) {
            TransformLease::Owner(owner) => owner,
            TransformLease::Waiter(_) => panic!("first lease should own transform"),
        };
        drop(owner);

        match acquire_transform_lease(&key) {
            TransformLease::Owner(_) => {}
            TransformLease::Waiter(_) => panic!("released key should be ownable"),
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
    fn transform_cache_cap_uses_minimum_for_tiny_filesystems() {
        assert_eq!(
            transform_cache_max_bytes_for_filesystem_bytes(10 * GIB),
            GIB
        );
    }

    #[test]
    fn transform_cache_cap_uses_five_percent_for_common_vps_filesystems() {
        assert_eq!(
            transform_cache_max_bytes_for_filesystem_bytes(40 * GIB),
            2 * GIB
        );
        assert_eq!(
            transform_cache_max_bytes_for_filesystem_bytes(80 * GIB),
            4 * GIB
        );
    }

    #[test]
    fn transform_cache_cap_uses_maximum_for_large_filesystems() {
        assert_eq!(
            transform_cache_max_bytes_for_filesystem_bytes(200 * GIB),
            4 * GIB
        );
    }

    #[test]
    fn transform_cache_cap_falls_back_when_filesystem_probe_fails() {
        assert_eq!(
            transform_cache_max_bytes(Path::new("")),
            TRANSFORM_CACHE_FALLBACK_BYTES
        );
    }

    #[test]
    fn cache_key_changes_with_source_bytes() {
        let left = transform_cache_key(
            "demo",
            test_app_root(),
            b"first",
            &options(OutputFormat::Avif, 640),
        );
        let right = transform_cache_key(
            "demo",
            test_app_root(),
            b"second",
            &options(OutputFormat::Avif, 640),
        );

        assert_ne!(left, right);
    }

    #[test]
    fn cache_key_changes_with_transform_options() {
        let left = transform_cache_key(
            "demo",
            test_app_root(),
            b"source",
            &options(OutputFormat::Avif, 640),
        );
        let right = transform_cache_key(
            "demo",
            test_app_root(),
            b"source",
            &options(OutputFormat::Webp, 640),
        );

        assert_ne!(left, right);
    }

    #[test]
    fn cache_key_changes_with_app_name() {
        let left = transform_cache_key(
            "demo",
            test_app_root(),
            b"source",
            &options(OutputFormat::Avif, 640),
        );
        let right = transform_cache_key(
            "other",
            test_app_root(),
            b"source",
            &options(OutputFormat::Avif, 640),
        );

        assert_ne!(left, right);
    }

    #[test]
    fn cache_key_changes_with_app_root() {
        let left = transform_cache_key(
            "demo",
            Path::new("/opt/tako/apps/demo/production/releases/v1"),
            b"source",
            &options(OutputFormat::Avif, 640),
        );
        let right = transform_cache_key(
            "demo",
            Path::new("/opt/tako/apps/demo/production/releases/v2"),
            b"source",
            &options(OutputFormat::Avif, 640),
        );

        assert_ne!(left, right);
    }

    #[tokio::test]
    async fn cache_write_then_read_returns_bytes_with_transform_content_type() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = transform_cache_key(
            "demo",
            test_app_root(),
            b"source",
            &options(OutputFormat::Webp, 640),
        );

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

    #[tokio::test]
    async fn prune_removes_expired_transform_cache_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = transform_cache_key(
            "demo",
            test_app_root(),
            b"expired",
            &options(OutputFormat::Webp, 640),
        );

        write(temp.path(), &key, b"expired-image").await;
        prune_with_policy(
            temp.path(),
            TransformCachePolicy {
                max_bytes: u64::MAX,
                max_age: Duration::ZERO,
            },
        )
        .await;

        assert_eq!(cache_total_bytes(temp.path()).await, 0);
    }

    #[tokio::test]
    async fn prune_removes_transform_cache_files_to_fit_byte_limit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let transform_options = options(OutputFormat::Webp, 640);

        for source in [
            b"first".as_slice(),
            b"second".as_slice(),
            b"third".as_slice(),
        ] {
            let key = transform_cache_key("demo", test_app_root(), source, &transform_options);
            write(temp.path(), &key, b"1234").await;
        }

        prune_with_policy(
            temp.path(),
            TransformCachePolicy {
                max_bytes: 8,
                max_age: Duration::from_secs(60 * 60),
            },
        )
        .await;

        assert!(cache_total_bytes(temp.path()).await <= 8);
    }

    #[tokio::test]
    async fn prune_ignores_temporary_transform_cache_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = transform_cache_key(
            "demo",
            test_app_root(),
            b"in-progress",
            &options(OutputFormat::Webp, 640),
        );
        let path = cache_path(temp.path(), &key).expect("cache path");
        let parent = path.parent().expect("cache parent");
        tokio::fs::create_dir_all(parent)
            .await
            .expect("create cache dir");
        let tmp_path = path.with_extension("tmp-test");
        tokio::fs::write(&tmp_path, b"in-progress")
            .await
            .expect("write tmp cache file");

        prune_with_policy(
            temp.path(),
            TransformCachePolicy {
                max_bytes: 0,
                max_age: Duration::ZERO,
            },
        )
        .await;

        assert!(tmp_path.exists());
        assert_eq!(cache_total_bytes(temp.path()).await, 0);
    }
}

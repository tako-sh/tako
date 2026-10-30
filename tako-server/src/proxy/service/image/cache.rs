use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime};
use tako_images::{ImageCrop, ImageFit, OutputFormat, TransformOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

const CACHE_VERSION: &str = "tako-image-transform-v1";
const CACHE_DIR_NAME: &str = "tako-image-cache-v2";
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
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        // Canonicalize the OS temp directory once (e.g. /var -> /private/var on
        // macOS), but never canonicalize or adopt an existing cache directory.
        let temp = std::env::temp_dir();
        let temp = temp.canonicalize().unwrap_or(temp);
        #[cfg(unix)]
        let name = format!("{CACHE_DIR_NAME}-{}", unsafe { libc::geteuid() });
        #[cfg(not(unix))]
        let name = CACHE_DIR_NAME;
        temp.join(name)
    })
    .clone()
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
    if !cache_file_is_trusted(root, &path).await {
        return None;
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file = options.open(path).await.ok()?;
    // Validate the opened inode, not just a path checked before open.
    if !trusted_cache_file(&file.metadata().await.ok()?) {
        return None;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.ok()?;
    let content_type = cached_content_type(&bytes, format);
    Some(CachedTransform {
        bytes,
        content_type,
    })
}

pub(super) async fn write(root: &Path, key: &str, bytes: &[u8]) {
    let Some(path) = cache_path(root, key) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if !ensure_private_cache_dir(root).await || !ensure_private_cache_dir(parent).await {
        return;
    }
    match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if !trusted_cache_file(&metadata) => return,
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return,
        _ => {}
    }
    let tmp_id = TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = path.with_extension(format!("tmp-{}-{tmp_id}", std::process::id()));
    let mut file = match create_cache_file(&tmp_path).await {
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

async fn create_cache_file(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path).await
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
    if !cache_dir_is_private(root).await {
        return files;
    }
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return files;
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };

        if !file_type.is_dir() || file_type.is_symlink() || !cache_dir_is_private(&path).await {
            continue;
        }

        let Ok(mut children) = tokio::fs::read_dir(&path).await else {
            continue;
        };
        while let Ok(Some(child)) = children.next_entry().await {
            let child_path = child.path();
            let Ok(file_type) = child.file_type().await else {
                continue;
            };
            if !file_type.is_file()
                || file_type.is_symlink()
                || !is_cache_file_path(root, &child_path)
            {
                continue;
            }
            let Ok(child_metadata) = tokio::fs::symlink_metadata(&child_path).await else {
                continue;
            };
            if trusted_cache_file(&child_metadata) {
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
    if !cache_dir_is_private(root).await {
        return;
    }
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return;
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        if file_type.is_dir() && !file_type.is_symlink() && cache_dir_is_private(&path).await {
            let _ = tokio::fs::remove_dir(path).await;
        }
    }
}

async fn cache_file_is_trusted(root: &Path, path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if !cache_dir_is_private(root).await || !cache_dir_is_private(parent).await {
        return false;
    }
    let Ok(metadata) = tokio::fs::symlink_metadata(path).await else {
        return false;
    };
    trusted_cache_file(&metadata)
}

#[cfg(unix)]
fn trusted_cache_file(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_file()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o022 == 0
        && metadata.nlink() == 1
}

#[cfg(not(unix))]
fn trusted_cache_file(_metadata: &std::fs::Metadata) -> bool {
    false // No Unix ownership contract: transform normally without a disk cache.
}

// Validate from the filesystem root down. Every ancestor must be controlled by
// root or this service UID; writable ancestors are allowed only with the sticky
// bit, which prevents other users from replacing our owned child. Once checked,
// an unrelated UID cannot swap a component between validation and I/O. Same-UID
// processes and root are trusted, as for the rest of the service's private data.
#[cfg(unix)]
async fn trusted_ancestors(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return false;
    }
    let uid = unsafe { libc::geteuid() };
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        let Ok(metadata) = tokio::fs::symlink_metadata(&current).await else {
            return false;
        };
        if !metadata.is_dir()
            || (metadata.uid() != 0 && metadata.uid() != uid)
            || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
        {
            return false;
        }
    }
    true
}

#[cfg(not(unix))]
async fn trusted_ancestors(_path: &Path) -> bool {
    false
}

async fn cache_dir_is_private(path: &Path) -> bool {
    if !trusted_ancestors(path).await {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(metadata) = tokio::fs::symlink_metadata(path).await else {
            return false;
        };
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o700
    }
    #[cfg(not(unix))]
    false
}

async fn ensure_private_cache_dir(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if !trusted_ancestors(parent).await {
        return false;
    }
    let mut builder = tokio::fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    match builder.create(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return false,
    }
    // Never chmod/chown an existing directory: it may already contain poison.
    cache_dir_is_private(path).await
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

fn cached_content_type(bytes: &[u8], requested_format: OutputFormat) -> &'static str {
    if is_webp(bytes) {
        return "image/webp";
    }
    if is_avif(bytes) {
        return "image/avif";
    }
    content_type_for_format(requested_format)
}

fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
}

fn is_avif(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && bytes[8..]
            .windows(4)
            .any(|brand| brand == b"avif" || brand == b"avis")
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

    fn private_tempdir() -> tempfile::TempDir {
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        builder.tempdir().expect("private tempdir")
    }

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

    #[cfg(unix)]
    #[tokio::test]
    async fn untrusted_cache_storage_cannot_supply_responses() {
        use std::os::unix::fs::PermissionsExt;

        // Exercise the real read/write API, including the old predictable root
        // shape: moving the default path alone must not make this test pass.
        for component in ["root", "partition", "file", "ancestor"] {
            let temp = private_tempdir();
            let root = temp.path().join("tako-image-cache");
            let partition = root.join("aa");
            std::fs::create_dir_all(&partition).unwrap();
            for dir in [&root, &partition] {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let file = partition.join("1234");
            std::fs::write(&file, b"injected-image").unwrap();
            let unsafe_path = match component {
                "root" => &root,
                "partition" => &partition,
                "file" => &file,
                _ => temp.path(),
            };
            std::fs::set_permissions(unsafe_path, std::fs::Permissions::from_mode(0o777)).unwrap();

            assert_eq!(
                read(&root, "aa1234", OutputFormat::Webp).await,
                None,
                "{component}"
            );
            write(&root, "aa1234", b"trusted-image").await;
            assert_eq!(
                std::fs::read(&file).unwrap(),
                b"injected-image",
                "{component}"
            );
            prune_with_policy(
                &root,
                TransformCachePolicy {
                    max_bytes: 0,
                    max_age: Duration::ZERO,
                },
            )
            .await;
            assert!(file.exists(), "must not prune untrusted {component}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cache_creates_private_storage_and_reuses_it() {
        use std::os::unix::fs::MetadataExt;
        let temp = private_tempdir();
        let root = temp.path().join("new-cache");
        write(&root, "aa1234", b"trusted-image").await;
        for (path, mode) in [
            (&root, 0o700),
            (&root.join("aa"), 0o700),
            (&root.join("aa/1234"), 0o600),
        ] {
            let metadata = std::fs::symlink_metadata(path).unwrap();
            assert_eq!(metadata.mode() & 0o777, mode);
            assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        }
        assert_eq!(
            read(&root, "aa1234", OutputFormat::Webp)
                .await
                .unwrap()
                .bytes,
            b"trusted-image"
        );
        write(&root, "aa1234", b"replacement-image").await;
        assert_eq!(
            read(&root, "aa1234", OutputFormat::Webp)
                .await
                .unwrap()
                .bytes,
            b"replacement-image"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn misowned_cache_storage_is_never_adopted() {
        use std::os::unix::fs::{PermissionsExt, chown};
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("ownership regression requires root to create foreign-owned fixtures");
            return;
        }
        for component in ["root", "partition", "file", "ancestor"] {
            let temp = private_tempdir();
            let root = temp.path().join("cache");
            let partition = root.join("aa");
            std::fs::create_dir_all(&partition).unwrap();
            for dir in [&root, &partition] {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let file = partition.join("1234");
            std::fs::write(&file, b"foreign-image").unwrap();
            let unsafe_path = match component {
                "root" => &root,
                "partition" => &partition,
                "file" => &file,
                _ => temp.path(),
            };
            chown(unsafe_path, Some(65534), None).unwrap();
            assert_eq!(
                read(&root, "aa1234", OutputFormat::Webp).await,
                None,
                "{component}"
            );
            write(&root, "aa1234", b"trusted-image").await;
            prune_with_policy(
                &root,
                TransformCachePolicy {
                    max_bytes: 0,
                    max_age: Duration::ZERO,
                },
            )
            .await;
            assert_eq!(
                std::fs::read(&file).unwrap(),
                b"foreign-image",
                "{component}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn another_uid_cannot_write_or_replace_private_cache_in_sticky_temp() {
        use std::os::unix::{fs::PermissionsExt, process::CommandExt};
        use std::process::{Command, Stdio};
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("cross-user regression requires root to launch another UID");
            return;
        }
        let temp = private_tempdir();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o1777)).unwrap();
        let root = temp.path().join("cache");
        write(&root, "aa1234", b"trusted-image").await;
        for script in [
            "printf poison > \"$1/aa/1234\"",
            "mv \"$1\" \"$1-replaced\"",
        ] {
            let status = Command::new("sh")
                .args(["-c", script, "cache-attacker"])
                .arg(&root)
                .uid(65534)
                .gid(65534)
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(
                !status.success(),
                "untrusted UID must not control the cache"
            );
        }
        assert_eq!(
            read(&root, "aa1234", OutputFormat::Webp)
                .await
                .unwrap()
                .bytes,
            b"trusted-image"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cache_rejects_hard_links_and_exclusive_write_collisions() {
        use std::os::unix::fs::symlink;
        let temp = private_tempdir();
        write(temp.path(), "aa1234", b"original").await;
        let file = temp.path().join("aa/1234");
        let alias = temp.path().join("alias");
        std::fs::hard_link(&file, &alias).unwrap();
        assert_eq!(read(temp.path(), "aa1234", OutputFormat::Webp).await, None);
        write(temp.path(), "aa1234", b"replacement").await;
        assert_eq!(std::fs::read(&alias).unwrap(), b"original");
        assert!(create_cache_file(&file).await.is_err());
        let link = temp.path().join("tmp-link");
        symlink(&file, &link).unwrap();
        assert!(create_cache_file(&link).await.is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cache_rejects_symlinked_roots_and_ancestors() {
        use std::os::unix::fs::symlink;
        let temp = private_tempdir();
        let real = temp.path().join("real");
        write(&real, "aa1234", b"original").await;
        let link = temp.path().join("link");
        symlink(&real, &link).unwrap();
        assert_eq!(read(&link, "aa1234", OutputFormat::Webp).await, None);
        write(&link, "aa1234", b"replacement").await;
        write(&link.join("nested"), "aa1234", b"replacement").await;
        assert!(!real.join("nested").exists());
        assert_eq!(std::fs::read(real.join("aa/1234")).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn default_cache_root_is_versioned_and_scoped_to_service_uid() {
        let root = default_cache_root();
        assert_eq!(
            root.parent().unwrap(),
            std::env::temp_dir().canonicalize().unwrap()
        );
        assert_eq!(
            root.file_name().unwrap().to_str().unwrap(),
            format!("tako-image-cache-v2-{}", unsafe { libc::geteuid() })
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
        let temp = private_tempdir();
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
    async fn cache_read_detects_webp_bytes_for_avif_request_fallbacks() {
        let temp = private_tempdir();
        let key = transform_cache_key(
            "demo",
            test_app_root(),
            b"source",
            &options(OutputFormat::Avif, 640),
        );
        let webp_bytes = b"RIFF\0\0\0\0WEBPcached-image";

        write(temp.path(), &key, webp_bytes).await;
        let cached = read(temp.path(), &key, OutputFormat::Avif)
            .await
            .expect("cached transform");

        assert_eq!(
            cached,
            CachedTransform {
                bytes: webp_bytes.to_vec(),
                content_type: "image/webp",
            }
        );
    }

    #[tokio::test]
    async fn prune_removes_expired_transform_cache_files() {
        let temp = private_tempdir();
        let key = transform_cache_key(
            "demo",
            test_app_root(),
            b"expired",
            &options(OutputFormat::Webp, 640),
        );

        write(temp.path(), &key, b"expired-image").await;
        assert_eq!(cache_total_bytes(temp.path()).await, 13);
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
        let temp = private_tempdir();
        let transform_options = options(OutputFormat::Webp, 640);

        for source in [
            b"first".as_slice(),
            b"second".as_slice(),
            b"third".as_slice(),
        ] {
            let key = transform_cache_key("demo", test_app_root(), source, &transform_options);
            write(temp.path(), &key, b"1234").await;
        }

        assert_eq!(cache_total_bytes(temp.path()).await, 12);
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
        let temp = private_tempdir();
        let key = transform_cache_key(
            "demo",
            test_app_root(),
            b"in-progress",
            &options(OutputFormat::Webp, 640),
        );
        let path = cache_path(temp.path(), &key).expect("cache path");
        let parent = path.parent().expect("cache parent");
        assert!(ensure_private_cache_dir(parent).await);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn cache_write_does_not_follow_symlinked_partition_dirs() {
        use std::os::unix::fs::symlink;

        let temp = private_tempdir();
        let external = private_tempdir();
        let partition = temp.path().join("aa");
        symlink(external.path(), &partition).expect("symlink cache partition");

        write(temp.path(), "aa1234", b"cached-image").await;

        assert!(!external.path().join("1234").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cache_read_ignores_symlinked_cache_files() {
        use std::os::unix::fs::symlink;

        let temp = private_tempdir();
        let target = temp.path().join("target");
        tokio::fs::write(&target, b"external-image")
            .await
            .expect("write target");
        let parent = temp.path().join("aa");
        assert!(ensure_private_cache_dir(&parent).await);
        symlink(&target, parent.join("1234")).expect("symlink cache file");

        assert_eq!(read(temp.path(), "aa1234", OutputFormat::Webp).await, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prune_does_not_follow_symlinked_partition_dirs() {
        use std::os::unix::fs::symlink;

        let temp = private_tempdir();
        let external = private_tempdir();
        let external_cache_file = external.path().join("1234");
        tokio::fs::write(&external_cache_file, b"external-cache")
            .await
            .expect("write external cache-shaped file");
        symlink(external.path(), temp.path().join("aa")).expect("symlink cache partition");

        prune_with_policy(
            temp.path(),
            TransformCachePolicy {
                max_bytes: 0,
                max_age: Duration::ZERO,
            },
        )
        .await;

        assert!(external_cache_file.exists());
        assert!(collect_cache_files(temp.path()).await.is_empty());
    }
}

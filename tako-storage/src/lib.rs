//! Tako binding configuration and HTTP authorization around Apache object_store.

use std::{fs, path::Path, sync::Arc, time::Duration};

use object_store::{
    ClientOptions, ObjectStore, RetryConfig,
    aws::{AmazonS3, AmazonS3Builder, S3ConditionalPut},
    local::LocalFileSystem,
};
use tako_core::{StorageBinding, StorageProvider};

#[cfg(feature = "http")]
pub mod local_http;
#[cfg(feature = "http")]
mod migration;

/// A local backend with atomic publication and synced data and directory entries.
/// The caller must supply a root that untrusted application processes cannot mutate.
pub fn local(root: &Path) -> Result<LocalFileSystem, String> {
    create_directory(root).map_err(|e| format!("create storage root: {e}"))?;
    LocalFileSystem::new_with_prefix(root)
        .map(|store| store.with_fsync(true))
        .map_err(|e| format!("open local storage: {e}"))
}

fn create_directory(path: &Path) -> std::io::Result<()> {
    if path.as_os_str().is_empty() || path.is_dir() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("storage parent missing"))?;
    create_directory(parent)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => fs::File::open(if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        })?
        .sync_all(),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

/// Build an S3 backend using only the explicit binding, never ambient credentials.
pub fn s3(binding: &StorageBinding) -> Result<AmazonS3, String> {
    s3_builder(binding)?
        .build()
        .map_err(|e| format!("configure s3 storage: {e}"))
}

/// Apply Tako's explicit credentials and bounded retries before client customization.
pub fn s3_builder(binding: &StorageBinding) -> Result<AmazonS3Builder, String> {
    if binding.provider != StorageProvider::S3 {
        return Err("storage binding is not configured for s3".into());
    }
    let endpoint = required(&binding.endpoint, "endpoint")?;
    let mut endpoint_url =
        url::Url::parse(endpoint).map_err(|e| format!("invalid storage endpoint: {e}"))?;
    if endpoint_url.scheme() != "https"
        || endpoint_url.host_str().is_none()
        || !endpoint_url.username().is_empty()
        || endpoint_url.password().is_some()
        || endpoint_url.query().is_some()
        || endpoint_url.fragment().is_some()
    {
        return Err(
            "storage endpoint must be an https URL without credentials, query or fragment".into(),
        );
    }
    let bucket = required(&binding.bucket, "bucket")?;
    if !binding.force_path_style {
        let host = format!(
            "{bucket}.{}",
            endpoint_url
                .host_str()
                .ok_or("storage endpoint host missing")?
        );
        endpoint_url
            .set_host(Some(&host))
            .map_err(|_| "invalid bucket host")?;
    }
    Ok(AmazonS3Builder::new()
        .with_bucket_name(bucket)
        .with_endpoint(endpoint_url.as_str().trim_end_matches('/'))
        .with_region(required(&binding.region, "region")?)
        .with_access_key_id(required(&binding.access_key_id, "access_key_id")?)
        .with_secret_access_key(required(&binding.secret_access_key, "secret_access_key")?)
        .with_virtual_hosted_style_request(!binding.force_path_style)
        .with_conditional_put(S3ConditionalPut::ETagMatch)
        // Background callers own retry/backoff, avoiding nested retry request bursts.
        .with_retry(RetryConfig {
            max_retries: 0,
            ..Default::default()
        })
        .with_client_options(
            ClientOptions::new()
                .with_timeout(Duration::from_secs(60))
                .with_connect_timeout(Duration::from_secs(5)),
        ))
}

/// Open a configured binding. Local paths are relative to a Tako-managed root.
pub fn from_binding(
    binding: &StorageBinding,
    local_root: &Path,
) -> Result<Arc<dyn ObjectStore>, String> {
    match binding.provider {
        StorageProvider::S3 => Ok(Arc::new(s3(binding)?)),
        StorageProvider::Local => {
            let path = required(&binding.path, "path")?;
            validate_relative(path)?;
            Ok(Arc::new(local(&local_root.join(path))?))
        }
    }
}

fn required<'a>(field: &'a Option<String>, name: &str) -> Result<&'a str, String> {
    field
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("storage is missing {name}"))
}

fn validate_relative(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.contains('\\')
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".." || p.contains('\0'))
    {
        return Err("storage path must be a non-empty relative path without dot segments".into());
    }
    Ok(())
}

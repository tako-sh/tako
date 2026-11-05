use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use tokio::io::AsyncWriteExt;

use super::*;

const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
static UPLOAD_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(super) struct Metadata {
    pub app: String,
    version: String,
    size: u64,
    sha256: String,
}

impl Metadata {
    pub fn parse(headers: &hyper::HeaderMap) -> Result<Self, &'static str> {
        let get = |name| header(headers, name).ok_or("Invalid artifact metadata");
        let metadata = Self {
            app: get("x-tako-app")?.into(),
            version: get("x-tako-version")?.into(),
            size: get("x-tako-artifact-size")?
                .parse()
                .map_err(|_| "Invalid artifact size")?,
            sha256: get("x-tako-artifact-sha256")?.into(),
        };
        if metadata.size == 0 || metadata.size > MAX_ARTIFACT_BYTES {
            return Err("Artifact size exceeds quota");
        }
        if metadata.sha256.len() != 64
            || !metadata
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("Invalid artifact digest");
        }
        crate::release::validate_app_name(&metadata.app).map_err(|_| "Invalid artifact app")?;
        crate::release::validate_release_version(&metadata.version)
            .map_err(|_| "Invalid artifact version")?;
        Ok(metadata)
    }

    pub fn auth_body(&self) -> Vec<u8> {
        tako_core::release_artifact_upload_auth_body(
            &self.app,
            &self.version,
            self.size,
            &self.sha256,
        )
    }
}

struct UploadFile(PathBuf);
impl Drop for UploadFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(super) async fn receive<B>(
    state: Arc<ServerState>,
    permissions: Permissions,
    scope: AppEnv,
    metadata: Metadata,
    mut body: B,
) -> HttpResponse
where
    B: Body<Data = Bytes> + Unpin + Send + 'static,
    B::Error: std::fmt::Display + Send + Sync + 'static,
{
    if state.is_draining() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Server is draining");
    }
    let Ok(slot) = UPLOAD_SLOTS.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Artifact upload capacity exhausted",
        );
    };
    let data_dir = &state.runtime_config().data_dir;
    let upload_dir = data_dir.join("tmp/uploads");
    if confined_directory(&upload_dir).is_err() || std::fs::create_dir_all(&upload_dir).is_err() {
        return error(StatusCode::BAD_REQUEST, "Upload directory unavailable");
    }
    let temp_path = upload_dir.join(format!("native-{}.tar.zst", nanoid::nanoid!(24)));
    let file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp_path)
    {
        Ok(file) => file,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Upload unavailable"),
    };
    let temp = UploadFile(temp_path);
    let mut file = tokio::fs::File::from_std(file);
    let received = tokio::time::timeout(UPLOAD_TIMEOUT, async {
        let mut count = 0_u64;
        let mut digest = Sha256::new();
        while let Some(frame) = tokio::time::timeout(BODY_TIMEOUT, body.frame())
            .await
            .map_err(|_| ())?
        {
            let frame = frame.map_err(|_| ())?;
            let data = frame.into_data().map_err(|_| ())?;
            count = count.checked_add(data.len() as u64).ok_or(())?;
            if count > metadata.size {
                return Err(());
            }
            digest.update(&data);
            file.write_all(&data).await.map_err(|_| ())?;
        }
        if count != metadata.size || hex::encode(digest.finalize()) != metadata.sha256 {
            return Err(());
        }
        file.flush().await.map_err(|_| ())?;
        Ok::<_, ()>(())
    })
    .await;
    drop(file);
    if !matches!(received, Ok(Ok(()))) {
        return error(
            StatusCode::BAD_REQUEST,
            "Artifact body size, digest, or deadline mismatch",
        );
    }
    match tokio::spawn(async move {
        let guard = state.management_lock.clone().lock_owned().await;
        if state.is_draining() {
            return error(StatusCode::SERVICE_UNAVAILABLE, "Server is draining");
        }
        let command = Command::PrepareReleaseUpload {
            app: metadata.app.clone(),
            version: metadata.version.clone(),
        };
        if authorize_command(&state, &permissions, Some(&scope), &command).is_err() {
            return error(StatusCode::FORBIDDEN, "Artifact destination denied");
        }
        // Blocking extraction retains both the guard and temporary file if its waiter is dropped.
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let _guard = guard;
            if confined_directory(
                &crate::release::app_root(&state.runtime_config().data_dir, &metadata.app)
                    .join("shared/logs"),
            )
            .is_err()
                || archive::validate(&temp.0).is_err()
            {
                return error(
                    StatusCode::BAD_REQUEST,
                    "Artifact archive violates upload limits",
                );
            }
            match state.store_uploaded_release_artifact(&metadata.app, &metadata.version, &temp.0) {
                Ok(plan) => json_response(StatusCode::OK, &tako_core::Response::ok(plan)),
                Err(_) => error(StatusCode::BAD_REQUEST, "Artifact extraction failed"),
            }
        })
        .await
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Artifact worker failed"))
    })
    .await
    {
        Ok(response) => response,
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Artifact worker failed"),
    }
}

pub(super) fn confined_directory(path: &Path) -> Result<(), ()> {
    if !path.is_absolute() {
        return Err(());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if !matches!(component, Component::RootDir | Component::Normal(_)) {
            return Err(());
        }
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                return Err(());
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

pub(super) fn confined_logs(data_dir: &Path, app: &str) -> Result<(), ()> {
    let directory = crate::release::app_root(data_dir, app).join("logs");
    confined_directory(&directory)?;
    for name in ["previous.log", "current.log"] {
        match std::fs::symlink_metadata(directory.join(name)) {
            Ok(metadata)
                if !metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || metadata.nlink() != 1 =>
            {
                return Err(());
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

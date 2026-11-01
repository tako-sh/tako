//! Production inputs live beside the app's replaceable data directory, never in an
//! app-writable release. Only source text and image identities are persisted;
//! interpolated environments remain in memory.
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ComposeRuntime, digest, write_private};

#[derive(Serialize, Deserialize)]
pub(crate) struct Prepared {
    // A single directory component allocated by tempfile under the private root.
    attempt: String,
    pub images: BTreeMap<String, String>,
}

impl ComposeRuntime {
    pub(crate) fn preparation_root(&self) -> Result<PathBuf, String> {
        let data = Path::new(
            self.env
                .get(crate::TAKO_APP_DATA_DIR_ENV)
                .ok_or("Compose requires an app data directory")?,
        );
        root_for(&self.directory, &self.config.file, data)
    }

    fn ensure_preparation_root(&self) -> Result<PathBuf, String> {
        let root = self.preparation_root()?;
        if root.starts_with(&self.directory) {
            return Err("Compose private data must be outside the source directory".into());
        }
        let private = root.parent().unwrap();
        // Keep deployment inputs outside data/: restoring an app-data backup
        // replaces that entire tree. These private ancestors also protect source
        // files whose mounts must remain readable by non-root containers.
        create_private_directory(private)?;
        create_private_directory(&root)?;
        write_private(
            &root.join("release.json"),
            &serde_json::to_vec(&self.directory).map_err(|e| e.to_string())?,
        )?;
        Ok(root)
    }

    pub(crate) fn snapshot(&self) -> Result<(Self, tempfile::TempDir), String> {
        let root = self.ensure_preparation_root()?;
        let snapshot = tempfile::Builder::new()
            .prefix("prepared-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(root)
            .map_err(|e| e.to_string())?;
        let source = snapshot.path().join("source");
        create_source_directory(&source)?;
        let directory = open_directory(&self.directory)?;
        copy_tree(
            &directory,
            &source,
            &mut CopyBudget {
                entries: 100_000,
                bytes: 2 * 1024 * 1024 * 1024,
            },
            0,
        )?;
        let compose_path = source.join(&self.config.file);
        let mut bytes = Vec::new();
        File::open(&compose_path)
            .map_err(|e| format!("Compose file: {e}"))?
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err("Compose source exceeds 16 MiB".into());
        }
        let mut yaml: serde_yaml_ng::Value =
            serde_yaml_ng::from_slice(&bytes).map_err(|_| "Invalid Compose YAML")?;
        yaml.apply_merge()
            .map_err(|_| "Invalid Compose YAML merge")?;
        let raw = serde_json::to_value(yaml).map_err(|_| "Invalid Compose YAML model")?;
        crate::validation::validate_source(&raw, &source, &self.env)?;
        write_private(
            &snapshot.path().join("compose.json"),
            &serde_json::to_vec(&raw).map_err(|e| e.to_string())?,
        )?;
        let mut runtime = self.clone();
        runtime.source = Some(source);
        Ok((runtime, snapshot))
    }

    pub(crate) fn commit_preparation(
        &self,
        snapshot: tempfile::TempDir,
        images: BTreeMap<String, String>,
    ) -> Result<(), String> {
        let record = Prepared {
            attempt: snapshot
                .path()
                .file_name()
                .unwrap()
                .to_str()
                .ok_or("Invalid preparation directory")?
                .into(),
            images,
        };
        // Keep the source before publishing the pointer. A crash may leave an
        // unused private copy, but cannot publish a missing/partial preparation.
        let _ = snapshot.keep();
        write_private(
            &self.preparation_root()?.join("current.json"),
            &serde_json::to_vec(&record).map_err(|e| e.to_string())?,
        )
    }

    pub(crate) fn prepared(&self) -> Result<(Self, BTreeMap<String, String>), String> {
        let root = self.ensure_preparation_root()?;
        let record: Prepared = serde_json::from_slice(
            &std::fs::read(root.join("current.json"))
                .map_err(|e| format!("Compose release was not prepared: {e}"))?,
        )
        .map_err(|_| "Invalid private Compose preparation record")?;
        if !record.attempt.starts_with("prepared-")
            || !record
                .attempt
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err("Invalid private Compose preparation directory".into());
        }
        let mut runtime = self.clone();
        runtime.source = Some(root.join(&record.attempt).join("source"));
        Ok((runtime, record.images))
    }
}

pub(crate) fn root_for(directory: &Path, file: &str, data: &Path) -> Result<PathBuf, String> {
    let private = app_preparations(data)?;
    Ok(private.join(digest(&format!("{}\n{file}", directory.display()))))
}

fn app_preparations(app_data: &Path) -> Result<PathBuf, String> {
    // TAKO_DATA_DIR is <app root>/data/app. Preparations belong to the app
    // lifecycle, but are deployment artifacts rather than restorable app data.
    Ok(app_data
        .parent()
        .and_then(Path::parent)
        .ok_or("Invalid app data directory")?
        .join("compose"))
}

/// Remove private inputs only after the caller has verified that no containers
/// reference this release. Includes interrupted preparation attempts.
pub fn remove_release_preparations(release: &Path, app_data: &Path) -> Result<(), String> {
    let root = app_preparations(app_data)?;
    if !root.try_exists().map_err(|e| e.to_string())? {
        return Ok(());
    }
    create_private_directory(&root)?;
    for entry in std::fs::read_dir(&root).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        create_private_directory(&path)?;
        let record = path.join("release.json");
        if !record.exists() {
            continue;
        }
        let directory: PathBuf =
            serde_json::from_slice(&std::fs::read(record).map_err(|e| e.to_string())?)
                .map_err(|_| "Invalid private Compose release record")?;
        if directory.starts_with(release) {
            std::fs::remove_dir_all(path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.to_string()),
    }
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "Compose preparation directory must not be a symlink")?;
    let metadata = dir.metadata().map_err(|e| e.to_string())?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(
            "Compose preparation directory must be service-owned and private (0700)".into(),
        );
    }
    Ok(())
}

struct CopyBudget {
    entries: usize,
    bytes: u64,
}

fn copy_tree(
    directory: &File,
    destination: &Path,
    budget: &mut CopyBudget,
    depth: usize,
) -> Result<(), String> {
    if depth > 128 {
        return Err("Compose source exceeds 128 directory levels".into());
    }
    // Walk through directory descriptors. No path component can be exchanged
    // for a symlink between validation and opening a source file.
    for entry in std::fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))
        .map_err(|e| e.to_string())?
    {
        budget.entries = budget
            .entries
            .checked_sub(1)
            .ok_or("Compose source exceeds 100,000 entries")?;
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        // The server installs a shared log symlink at the release root. Logs
        // are runtime output and never part of a production build input.
        if depth == 0 && name == "logs" {
            continue;
        }
        let name_c = CString::new(name.as_encoded_bytes()).map_err(|e| e.to_string())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(
                "Compose source files must be readable and must not contain symlinks".into(),
            );
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        let target = destination.join(name);
        if metadata.is_dir() {
            create_source_directory(&target)?;
            copy_tree(&file, &target, budget, depth + 1)?;
        } else if metadata.is_file() && metadata.nlink() == 1 {
            budget.bytes = budget
                .bytes
                .checked_sub(metadata.len())
                .ok_or("Compose source exceeds 2 GiB")?;
            let mode = if metadata.mode() & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(target)
                .map_err(|e| e.to_string())?;
            output
                .set_permissions(std::fs::Permissions::from_mode(mode))
                .map_err(|e| e.to_string())?;
            let count = std::io::copy(&mut file.take(metadata.len()), &mut output)
                .map_err(|e| e.to_string())?;
            if count != metadata.len() {
                return Err("Compose source changed while being copied".into());
            }
            output.flush().map_err(|e| e.to_string())?;
            output.sync_all().map_err(|e| e.to_string())?;
        } else {
            return Err(
                "Compose source must contain only directories and regular files without hard links"
                    .into(),
            );
        }
    }
    File::open(destination)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())
}

fn create_source_directory(path: &Path) -> Result<(), String> {
    std::fs::create_dir(path).map_err(|e| e.to_string())?;
    // The enclosing preparation directory stays 0700. Files/directories below
    // source must remain readable when mounted into a non-root container.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())
}

fn open_directory(path: &Path) -> Result<File, String> {
    if !path.is_absolute() {
        return Err("Compose source directory must be absolute".into());
    }
    let mut directory = File::open("/").map_err(|e| e.to_string())?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            _ => return Err("Invalid Compose source directory".into()),
        };
        let name = CString::new(name.as_encoded_bytes()).map_err(|e| e.to_string())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err("Compose source directory must not contain symlinks".into());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::ComposeConfig;
    use std::collections::HashMap;

    fn fixture(yaml: &str) -> (tempfile::TempDir, ComposeRuntime) {
        let directory = tempfile::tempdir().unwrap();
        let release = directory.path().join("release");
        std::fs::create_dir(&release).unwrap();
        std::fs::create_dir(directory.path().join("data")).unwrap();
        std::fs::write(release.join("compose.yaml"), yaml).unwrap();
        let runtime = ComposeRuntime::new(
            release,
            ComposeConfig {
                file: "compose.yaml".into(),
                service: "web".into(),
                port: 8080,
                health_path: "/".into(),
                health_host: None,
            },
            HashMap::from([
                (
                    crate::TAKO_APP_DATA_DIR_ENV.into(),
                    directory.path().join("data/app").display().to_string(),
                ),
                ("APP_TOKEN".into(), "decrypted-test-value".into()),
            ]),
        )
        .unwrap();
        (directory, runtime)
    }

    #[test]
    fn prepared_source_is_independent_of_open_release_files_and_image_records() {
        let (_dir, runtime) = fixture(
            "services:\n  web:\n    image: busybox\n    environment:\n      TOKEN: ${APP_TOKEN}\n",
        );
        let original = runtime.directory.join("compose.yaml");
        let mut already_open = OpenOptions::new().write(true).open(&original).unwrap();
        let (captured, snapshot) = runtime.snapshot().unwrap();
        let source = captured.source.clone().unwrap();
        assert_ne!(
            std::fs::metadata(&original).unwrap().ino(),
            std::fs::metadata(source.join("compose.yaml"))
                .unwrap()
                .ino()
        );
        let canonical =
            std::fs::read_to_string(source.parent().unwrap().join("compose.json")).unwrap();
        assert!(canonical.contains("${APP_TOKEN}"));
        assert!(!canonical.contains("decrypted-test-value"));
        assert_eq!(
            std::fs::metadata(source.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        runtime
            .commit_preparation(
                snapshot,
                BTreeMap::from([("web".into(), "approved-image".into())]),
            )
            .unwrap();

        // Even a file descriptor obtained before preparation cannot mutate the
        // private copy; chmod alone on the original inode would not provide this.
        already_open.set_len(0).unwrap();
        already_open
            .write_all(b"services:\n  web:\n    privileged: true\n")
            .unwrap();
        std::fs::write(
            runtime.directory.join(".tako-compose-images.json"),
            "{\"web\":\"untrusted-image\"}",
        )
        .unwrap();
        std::fs::remove_file(original).unwrap();
        let reloaded = ComposeRuntime::new(
            runtime.directory.clone(),
            runtime.config.clone(),
            runtime.env.clone(),
        )
        .unwrap();
        let (prepared, images) = reloaded.prepared().unwrap();
        assert_eq!(images["web"], "approved-image");
        assert_eq!(
            std::fs::read_to_string(
                prepared
                    .source
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("compose.json")
            )
            .unwrap(),
            canonical
        );
        assert_eq!(
            std::fs::metadata(runtime.preparation_root().unwrap().join("current.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn private_inputs_follow_release_cleanup_and_exclude_runtime_logs() {
        let (dir, runtime) = fixture("services: {web: {image: busybox}}\n");
        let outside = dir.path().join("runtime-logs");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("private.log"), "runtime output").unwrap();
        std::os::unix::fs::symlink(&outside, runtime.directory.join("logs")).unwrap();
        let (captured, snapshot) = runtime.snapshot().unwrap();
        assert!(!captured.working_directory().join("logs").exists());
        runtime
            .commit_preparation(snapshot, BTreeMap::new())
            .unwrap();
        let data = Path::new(&runtime.env[crate::TAKO_APP_DATA_DIR_ENV]);
        remove_release_preparations(&dir.path().join("other"), data).unwrap();
        assert!(runtime.prepared().is_ok());
        remove_release_preparations(&runtime.directory, data).unwrap();
        assert!(!runtime.preparation_root().unwrap().exists());
        assert!(outside.join("private.log").is_file());
    }

    #[test]
    fn source_copy_rejects_symlinks_hard_links_and_special_files() {
        for kind in ["symlink", "hardlink", "fifo"] {
            let (dir, runtime) = fixture("services: {web: {image: busybox}}\n");
            let outside = dir.path().join("private-value");
            std::fs::write(&outside, "server-only-value").unwrap();
            let entry = runtime.directory.join("input");
            match kind {
                "symlink" => std::os::unix::fs::symlink(outside, &entry).unwrap(),
                "hardlink" => std::fs::hard_link(outside, &entry).unwrap(),
                _ => {
                    let entry = CString::new(entry.as_os_str().as_encoded_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(entry.as_ptr(), 0o600) }, 0);
                }
            }
            assert!(
                runtime.snapshot().is_err(),
                "{kind} must not enter a privileged build context"
            );
        }
    }

    #[test]
    fn unsafe_source_inputs_are_rejected_before_docker_runs() {
        let cases = [
            "include: /opt/tako/secret.key\nservices: {web: {image: busybox}}",
            "services: {web: {image: busybox, extends: {file: /opt/tako/secret.key, service: web}}}",
            "services: {web: {image: busybox, env_file: /opt/tako/secret.key}}",
            "services: {web: {build: {context: ., additional_contexts: {private: /opt/tako}}}}",
            "services: {web: {build: {context: ., dockerfile: /opt/tako/secret.key}}}",
            "services: {web: {build: {context: ., secrets: [private]}}}\nsecrets: {private: {file: /opt/tako/secret.key}}",
            "services: {web: {image: busybox, secrets: [private]}}\nsecrets: {private: {environment: HOME}}",
            "services: {web: {build: {context: ., ssh: [default]}}}",
            "services: {web: {build: {context: ., entitlements: [security.insecure]}}}",
            "services: {web: {build: {context: ., network: host}}}",
            "services: {web: {build: {context: ., cache_to: ['type=local,dest=/opt/tako']}}}",
        ];
        for yaml in cases {
            let (_dir, runtime) = fixture(yaml);
            std::fs::write(runtime.directory.join("Dockerfile"), "FROM busybox\n").unwrap();
            assert!(
                runtime.snapshot().is_err(),
                "unsafe source must fail before invoking Compose"
            );
        }
    }
}

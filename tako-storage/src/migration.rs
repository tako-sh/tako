use cap_std::{ambient_authority, fs::Dir};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::Path,
};

/// Publish an existing SDK storage tree into the private server namespace once.
/// Capability-relative reads cannot follow a raced symlink out of the app data tree.
pub(crate) fn prepare(root: &Path, legacy_root: &Path, relative: &str) -> Result<(), String> {
    crate::create_directory(root).map_err(|e| e.to_string())?;
    let state = root
        .parent()
        .ok_or("storage parent missing")?
        .join("storage-migrations");
    let marker = state.join(hex::encode(Sha256::digest(relative.as_bytes())));
    if marker.exists() {
        return Ok(());
    }
    crate::create_directory(&state).map_err(|e| e.to_string())?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state.join("migration.lock"))
        .map_err(|e| e.to_string())?;
    lock.try_lock()
        .map_err(|e| format!("storage migration busy: {e}"))?;
    let destination = root.join(relative);
    if marker.exists() {
        return Ok(());
    }
    let parent = destination.parent().ok_or("storage parent missing")?;
    crate::create_directory(parent).map_err(|e| e.to_string())?;
    let staging = tempfile::tempdir_in(parent).map_err(|e| e.to_string())?;
    let source = Dir::open_ambient_dir(legacy_root, ambient_authority());
    match source {
        Ok(source) => match source.open_dir(relative) {
            Ok(directory) => copy_tree(&directory, staging.path())
                .map_err(|e| format!("migrate local storage: {e}"))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("open previous local storage: {error}")),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("open app data for storage migration: {error}")),
    }
    File::open(staging.path())
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    if destination.exists() {
        let staged = Dir::open_ambient_dir(staging.path(), ambient_authority())
            .map_err(|e| e.to_string())?;
        copy_tree(&staged, &destination)
            .map_err(|e| format!("merge local storage migration: {e}"))?;
    } else {
        fs::rename(staging.path(), &destination).map_err(|e| e.to_string())?;
    }
    File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    let mut completed = tempfile::NamedTempFile::new_in(&state).map_err(|e| e.to_string())?;
    completed
        .write_all(relative.as_bytes())
        .map_err(|e| e.to_string())?;
    completed.as_file().sync_all().map_err(|e| e.to_string())?;
    completed
        .persist_noclobber(marker)
        .map_err(|e| e.to_string())?;
    File::open(&state)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn copy_tree(source: &Dir, destination: &Path) -> io::Result<()> {
    for entry in source.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let kind = entry.file_type()?;
        let target = destination.join(&name);
        if kind.is_dir() {
            crate::create_directory(&target)?;
            copy_tree(&source.open_dir(&name)?, &target)?;
            File::open(&target)?.sync_all()?;
        } else if kind.is_file() {
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NONBLOCK);
            }
            let input = source.open_with(&name, &options)?.into_std();
            let metadata = input.metadata()?;
            if !metadata.is_file() {
                return Err(io::Error::other("storage source changed to a special file"));
            }
            #[cfg(unix)]
            if std::os::unix::fs::MetadataExt::nlink(&metadata) != 1 {
                return Err(io::Error::other(
                    "local storage migration does not accept hard links",
                ));
            }
            if target.exists() {
                continue;
            }
            let mut output = tempfile::NamedTempFile::new_in(destination)?;
            // Bound copying even if the application keeps appending to its old file.
            let copied = io::copy(&mut input.take(metadata.len()), &mut output)?;
            if copied != metadata.len() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "storage source changed during migration",
                ));
            }
            output.as_file().sync_all()?;
            output.persist_noclobber(&target)?;
        } else {
            return Err(io::Error::other(
                "local storage migration does not accept symlinks or special files",
            ));
        }
    }
    File::open(destination)?.sync_all()?;
    Ok(())
}

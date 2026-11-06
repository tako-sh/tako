use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use super::{ConfigError, Result};

pub(super) fn edit(path: &Path, update: impl FnOnce(&mut toml::Table) -> Result<()>) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|error| ConfigError::FileWrite(parent.into(), error))?;
    let lock_path = path.with_extension("toml.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options
        .open(&lock_path)
        .map_err(|error| ConfigError::FileWrite(lock_path.clone(), error))?;
    lock.lock()
        .map_err(|error| ConfigError::FileWrite(lock_path, error))?;

    let mut document = match File::open(path) {
        Ok(mut file) => {
            let mut contents = String::new();
            file.read_to_string(&mut contents)
                .map_err(|error| ConfigError::FileRead(path.into(), error))?;
            toml::from_str::<toml::Table>(&contents)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(error) => return Err(ConfigError::FileRead(path.into(), error)),
    };
    update(&mut document)?;
    let bytes = toml::to_string_pretty(&document)?;
    let persist = || -> std::io::Result<()> {
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(bytes.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    };
    persist().map_err(|error| ConfigError::FileWrite(path.into(), error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn concurrent_section_updates_preserve_each_other() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let barrier = Arc::new(Barrier::new(8));
        std::thread::scope(|scope| {
            for index in 0..8 {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    edit(path, |document| {
                        document.insert(format!("section{index}"), toml::Value::Integer(index));
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });
        let document: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(document.len(), 8);
    }

    #[test]
    fn invalid_updates_leave_the_original_file_intact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "[unrelated]\nvalue = 42\n").unwrap();
        let before = fs::read(&path).unwrap();
        let result = edit(&path, |document| {
            document.clear();
            Err(ConfigError::Validation("rejected".into()))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }
}

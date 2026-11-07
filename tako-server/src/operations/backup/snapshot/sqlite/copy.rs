use std::fs::File;
use std::io::{self, Seek};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub(super) fn copy(source: &mut File, destination: &Path) -> io::Result<()> {
    let mut output = File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)?;

    // FICLONE shares only data. macOS fclonefileat also inherits app-controlled
    // file flags, including immutable, which can prevent staging cleanup.
    #[cfg(all(
        target_os = "linux",
        not(any(target_arch = "sparc", target_arch = "sparc64"))
    ))]
    {
        use rustix::io::Errno;
        match rustix::fs::ioctl_ficlone(&output, &*source) {
            Ok(()) => return Ok(()),
            Err(
                Errno::XDEV
                | Errno::OPNOTSUPP
                | Errno::NOTTY
                | Errno::NOSYS
                | Errno::INVAL
                | Errno::BADF,
            ) => output.set_len(0)?,
            Err(error) => return Err(error.into()),
        }
    }

    source.rewind()?;
    io::copy(source, &mut output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::SeekFrom;
    #[cfg(target_os = "linux")]
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[test]
    fn copies_the_pinned_file_into_an_exclusive_private_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("snapshot");
        fs::write(&source, b"database").unwrap();
        let mut pinned = File::open(&source).unwrap();
        pinned.seek(SeekFrom::End(0)).unwrap();
        fs::rename(&source, dir.path().join("old")).unwrap();
        fs::write(&source, b"replacement").unwrap();

        copy(&mut pinned, &destination).unwrap();
        fs::write(dir.path().join("old"), b"new data").unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"database");
        assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o077, 0);

        let link = dir.path().join("existing");
        symlink(&source, &link).unwrap();
        assert_eq!(
            copy(&mut pinned, &link).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&source).unwrap(), b"replacement");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn copies_across_filesystems_when_reflinks_are_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("snapshot");
        let mut source = File::from(
            rustix::fs::memfd_create("source", rustix::fs::MemfdFlags::CLOEXEC).unwrap(),
        );
        source.write_all(b"committed").unwrap();

        copy(&mut source, &destination).unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"committed");
    }
}

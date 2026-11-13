//! Mount boundaries in and around app data. Restores move files with
//! rename, which carries a mount point along with the directory above it,
//! and rollback directories are removed recursively, which must never
//! descend into a mounted volume.

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// The filesystem and the mount a file is on. A bind mount of the same
/// filesystem has the device of its source, so the mount id tells them
/// apart where the kernel reports it (Linux 5.8 and later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MountIdentity {
    device: u64,
    mount_id: Option<u64>,
}

pub(super) fn identity_of(path: &Path) -> std::io::Result<MountIdentity> {
    let stat = fs::lstat(path)?;
    Ok(MountIdentity {
        device: stat.st_dev as u64,
        mount_id: mount_id(fs::CWD, path, AtFlags::SYMLINK_NOFOLLOW),
    })
}

fn identity_of_dir(dir: &File) -> std::io::Result<MountIdentity> {
    let stat = fs::fstat(dir)?;
    Ok(MountIdentity {
        device: stat.st_dev as u64,
        #[cfg(target_os = "linux")]
        mount_id: mount_id(dir, "", AtFlags::EMPTY_PATH),
        #[cfg(not(target_os = "linux"))]
        mount_id: None,
    })
}

#[cfg(target_os = "linux")]
fn mount_id<Fd: std::os::fd::AsFd, P: rustix::path::Arg>(
    dir: Fd,
    path: P,
    flags: AtFlags,
) -> Option<u64> {
    let statx = fs::statx(dir, path, flags, fs::StatxFlags::MNT_ID).ok()?;
    (statx.stx_mask & fs::StatxFlags::MNT_ID.bits() != 0).then_some(statx.stx_mnt_id)
}

#[cfg(not(target_os = "linux"))]
fn mount_id<Fd: std::os::fd::AsFd, P: rustix::path::Arg>(
    _dir: Fd,
    _path: P,
    _flags: AtFlags,
) -> Option<u64> {
    None
}

/// Mount points listed in `/proc/self/mountinfo` text (the fifth field,
/// with the kernel's octal escapes for space, tab, newline and backslash).
pub(super) fn parse_mountinfo(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|field| PathBuf::from(OsStr::from_bytes(&unescape(field.as_bytes()))))
        .collect()
}

fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        let octal = field.get(index + 1..index + 4).filter(|digits| {
            field[index] == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
        });
        match octal {
            Some(digits) => {
                let value = digits
                    .iter()
                    .fold(0_u32, |value, digit| value * 8 + u32::from(digit - b'0'));
                out.push(value as u8);
                index += 4;
            }
            None => {
                out.push(field[index]);
                index += 1;
            }
        }
    }
    out
}

/// The mount points in `mounts` that are `root` or lie below it.
pub(super) fn mounts_at_or_below(root: &Path, mounts: &[PathBuf]) -> Vec<PathBuf> {
    mounts
        .iter()
        .filter(|mount| mount.starts_with(root))
        .cloned()
        .collect()
}

/// Mount points at or below the existing directory `root`.
#[cfg(target_os = "linux")]
pub(super) fn mount_points_at_or_below(root: &Path) -> Result<Vec<PathBuf>, String> {
    // Mount points are listed by their real path.
    let root =
        std::fs::canonicalize(root).map_err(|e| format!("resolve {}: {e}", root.display()))?;
    let text = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("read /proc/self/mountinfo: {e}"))?;
    Ok(mounts_at_or_below(&root, &parse_mountinfo(&text)))
}

/// Elsewhere only the per-path filesystem checks apply.
#[cfg(not(target_os = "linux"))]
pub(super) fn mount_points_at_or_below(_root: &Path) -> Result<Vec<PathBuf>, String> {
    Ok(Vec::new())
}

/// Remove the directory tree at `path` without crossing into another
/// filesystem or mount. A directory on another mount stops the removal
/// with an error and is left with everything in it; nothing is followed
/// through a symlink.
pub(super) fn remove_tree(path: &Path) -> Result<(), String> {
    remove_tree_with(path, &|_| false)
}

/// [`remove_tree`], also treating each directory `foreign` accepts as a
/// mount boundary.
pub(super) fn remove_tree_with(path: &Path, foreign: &dyn Fn(&Path) -> bool) -> Result<(), String> {
    let root = match fs::open(path, DIR_FLAGS, Mode::empty()) {
        Ok(root) => File::from(root),
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let identity =
        identity_of_dir(&root).map_err(|e| format!("inspect {}: {e}", path.display()))?;
    empty_dir(&root, path, identity, foreign)?;
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove {}: {error}", path.display())),
    }
}

fn empty_dir(
    dir: &File,
    path: &Path,
    root: MountIdentity,
    foreign: &dyn Fn(&Path) -> bool,
) -> Result<(), String> {
    let failed =
        |what: &Path, error: rustix::io::Errno| format!("remove {}: {error}", what.display());
    // Names first: entries removed while reading may hide others.
    let mut names = Vec::new();
    for entry in fs::Dir::read_from(dir).map_err(|e| failed(path, e))? {
        let entry = entry.map_err(|e| failed(path, e))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_os_string());
        }
    }
    for name in names {
        let child_path = path.join(&name);
        let stat = match fs::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(error) => return Err(failed(&child_path, error)),
        };
        let flags = if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            let child = match fs::openat(dir, &name, DIR_FLAGS, Mode::empty()) {
                Ok(child) => File::from(child),
                Err(rustix::io::Errno::NOENT) => continue,
                Err(error) => return Err(failed(&child_path, error)),
            };
            let identity = identity_of_dir(&child)
                .map_err(|e| format!("inspect {}: {e}", child_path.display()))?;
            if identity != root || foreign(&child_path) {
                return Err(format!(
                    "{} is on another filesystem or mount; it and everything in it were left in place",
                    child_path.display()
                ));
            }
            empty_dir(&child, &child_path, root, foreign)?;
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        match fs::unlinkat(dir, &name, flags) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(failed(&child_path, error)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_lists_mount_points_with_escapes() {
        let text = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
23 22 0:5 / /proc rw,nosuid shared:2 - proc proc rw
40 22 8:1 /srv/uploads /opt/tako/apps/demo/production/data/app/uploads rw - ext4 /dev/sda1 rw
41 22 8:2 / /opt/tako/apps/demo/production/data/app/my\\040files rw - ext4 /dev/sdb1 rw
42 22 8:2 / /opt/tako/apps/demo/production/data2 rw - ext4 /dev/sdb1 rw
";
        let mounts = parse_mountinfo(text);
        assert_eq!(mounts.len(), 5);
        assert_eq!(
            mounts[3],
            Path::new("/opt/tako/apps/demo/production/data/app/my files")
        );
        let data = Path::new("/opt/tako/apps/demo/production/data");
        assert_eq!(
            mounts_at_or_below(data, &mounts),
            [data.join("app/uploads"), data.join("app/my files"),]
        );
        assert_eq!(
            mounts_at_or_below(&data.join("app/uploads"), &mounts),
            [data.join("app/uploads")]
        );
        assert!(mounts_at_or_below(&data.join("tako"), &mounts).is_empty());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn the_root_directory_has_mounts_below_it() {
        let mounts = mount_points_at_or_below(Path::new("/")).unwrap();
        assert!(
            mounts.iter().any(|mount| mount == Path::new("/proc")),
            "{mounts:?}"
        );
        let temp = tempfile::tempdir().unwrap();
        assert!(mount_points_at_or_below(temp.path()).unwrap().is_empty());
    }

    /// A rollback directory with a mounted volume inside: the removal
    /// stops at the volume and deletes nothing in it.
    #[test]
    fn removal_never_descends_into_another_mount() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("rollback");
        let volume = dir.join("data/app/uploads");
        std::fs::create_dir_all(volume.join("photos")).unwrap();
        std::fs::write(volume.join("photos/one.jpg"), b"photo").unwrap();
        std::fs::write(dir.join("data/app/db.sqlite"), b"db").unwrap();
        std::os::unix::fs::symlink(temp.path(), dir.join("data/link")).unwrap();

        let error = remove_tree_with(&dir, &|path| path == volume).unwrap_err();
        assert!(error.contains("on another filesystem or mount"), "{error}");
        assert_eq!(
            std::fs::read(volume.join("photos/one.jpg")).unwrap(),
            b"photo"
        );
        assert!(temp.path().exists());

        // Without the boundary everything goes, but never through the link.
        remove_tree(&dir).unwrap();
        assert!(!dir.exists());
        assert!(temp.path().exists());
        remove_tree(&dir).unwrap();
    }
}

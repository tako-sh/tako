use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub(crate) fn is_root() -> bool {
    effective_uid() == 0
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and returns the effective user id
    // for the current process.
    unsafe { libc::geteuid() as u32 }
}

pub(crate) fn lookup_user_ids(name: &str) -> io::Result<Option<(u32, u32)>> {
    let name = CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "user name contains interior NUL byte",
        )
    })?;
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buf = vec![0u8; passwd_buffer_size()];

    loop {
        // SAFETY: name is a valid NUL-terminated C string. entry points to
        // writable passwd storage, buf is a writable byte buffer of length
        // buf.len(), and result points to writable pointer storage. getpwnam_r
        // initializes entry before setting result to it.
        let rc = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                entry.as_mut_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };

        if rc == 0 {
            if result.is_null() {
                return Ok(None);
            }
            // SAFETY: getpwnam_r returned success and result points at entry.
            let entry = unsafe { entry.assume_init() };
            return Ok(Some((entry.pw_uid, entry.pw_gid)));
        }

        if rc == libc::ERANGE {
            buf.resize(buf.len() * 2, 0);
            continue;
        }

        return Err(io::Error::from_raw_os_error(rc));
    }
}

fn passwd_buffer_size() -> usize {
    // SAFETY: sysconf has no preconditions for _SC_GETPW_R_SIZE_MAX.
    let size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    if size > 0 { size as usize } else { 16 * 1024 }
}

pub(crate) fn chown_path(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains interior NUL byte",
        )
    })?;
    // SAFETY: path is a valid NUL-terminated C string. uid and gid are plain
    // integer identifiers from the OS user database.
    let rc = unsafe { libc::chown(path.as_ptr(), uid as libc::uid_t, gid as libc::gid_t) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_user_ids_returns_none_for_missing_user() {
        assert!(
            lookup_user_ids("this-user-definitely-does-not-exist-tako-test")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn lookup_user_ids_rejects_invalid_user_name() {
        assert_eq!(
            lookup_user_ids("bad\0name").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

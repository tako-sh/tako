//! The bootstrap pipe on fd 3 is marked close-on-exec before `main`, so
//! subprocesses an app starts cannot inherit it.

#[cfg(target_os = "linux")]
#[test]
fn the_bootstrap_pipe_is_not_inherited_by_subprocesses() {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    use std::os::unix::process::CommandExt;

    if std::env::var_os("TAKO_CLOEXEC_CHILD").is_some() {
        // This process was started with the pipe on fd 3, as Tako starts
        // apps. Nothing here touched it before this check.
        let flags = unsafe { libc::fcntl(3, libc::F_GETFD) };
        assert!(flags >= 0, "fd 3 is open");
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "fd 3 is close-on-exec");
        // The SDK still reads it.
        let _ = tako::bootstrap();
        return;
    }

    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (reader, writer) = (fds[0], fds[1]);
    let mut writer = unsafe { std::fs::File::from_raw_fd(writer) };
    writer.write_all(b"{}").unwrap();
    drop(writer);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "the_bootstrap_pipe_is_not_inherited_by_subprocesses",
            "--test-threads=1",
        ])
        .env("TAKO_CLOEXEC_CHILD", "1");
    unsafe {
        child.pre_exec(move || {
            if libc::dup2(reader, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // dup2 leaves fd 3 inheritable, as tako-server does.
            Ok(())
        });
    }
    let status = child.status().unwrap();
    unsafe { libc::close(reader) };
    assert!(status.success(), "the child saw an inheritable fd 3");
}

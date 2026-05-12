//! Low-level spawn/fd helpers shared between tako-server and tako-workflows.
//!
//! Both crates fork+exec child processes with a JSON payload streamed to
//! them on a pipe — the server does it for app instances, workflows does
//! it for workers. The pipe plumbing is identical and subtle enough
//! (FD_CLOEXEC, background writer thread) that it lives here.

#![cfg(unix)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::thread::JoinHandle;

/// Set `FD_CLOEXEC` on `fd` so it does not survive `exec` in a forked
/// child.
pub fn set_cloexec(fd: &OwnedFd) -> std::io::Result<()> {
    let raw = fd.as_raw_fd();
    // SAFETY: `raw` is owned by `fd` for the duration of this call.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Create a pipe, spawn a writer thread that streams `payload` into the
/// write end, and return the read end plus a join handle.
///
/// Intended for the "parent prepares bootstrap data, child reads it from
/// an inherited fd" pattern. The caller is responsible for arranging for
/// the child to inherit the returned read end (typically via stdio
/// redirection onto fd 3), keeping it alive through `spawn()`, and
/// joining the writer handle after spawn to surface write errors.
///
/// Properties:
///   - `FD_CLOEXEC` is set on the write end. The writer thread runs in
///     parallel with `fork`, so without CLOEXEC the child would inherit
///     a live copy of the write end and block forever waiting for EOF
///     on the read fd that it itself is holding open.
///   - The write happens off-thread, so payloads larger than the OS
///     pipe buffer (16 KiB on macOS, 64 KiB on Linux) don't deadlock
///     the caller — the child hasn't been spawned yet when this
///     returns.
pub fn create_payload_pipe(
    payload: Vec<u8>,
) -> std::io::Result<(OwnedFd, JoinHandle<std::io::Result<()>>)> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe() is a standard POSIX call; fds is a valid 2-element array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe() just returned these file descriptors.
    let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    set_cloexec(&write_end)?;

    let writer_handle = std::thread::spawn(move || -> std::io::Result<()> {
        use std::io::Write;
        let mut writer = std::fs::File::from(write_end);
        writer.write_all(&payload)
        // write_end (now `writer`) drops here, closing the fd and giving the
        // child EOF once it has drained the payload.
    });

    Ok((read_end, writer_handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn set_cloexec_flags_the_fd() {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        let baseline = unsafe { libc::fcntl(write_end.as_raw_fd(), libc::F_GETFD) };
        assert!(baseline >= 0);
        assert_eq!(
            baseline & libc::FD_CLOEXEC,
            0,
            "libc::pipe unexpectedly returned a CLOEXEC-flagged fd — test premise broken"
        );

        set_cloexec(&write_end).expect("set_cloexec");

        let flags = unsafe { libc::fcntl(write_end.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);

        drop(read_end);
        drop(write_end);
    }

    #[test]
    fn payload_pipe_write_end_has_cloexec() {
        // Regression: without CLOEXEC on the write end, the forked
        // child inherits a live writer across exec and its read of the
        // bootstrap fd blocks forever waiting for an EOF it is itself
        // preventing. We can't easily introspect the write-end flags
        // after construction (the writer thread owns it), so we verify
        // the primitive behavior in `set_cloexec_flags_the_fd` above
        // and here assert that the payload round-trips end-to-end.
        let (read_end, writer) = create_payload_pipe(b"hello".to_vec()).expect("create pipe");

        let mut file = std::fs::File::from(read_end);
        let mut buf = String::new();
        file.read_to_string(&mut buf).expect("read");
        assert_eq!(buf, "hello");

        writer.join().expect("writer thread").expect("write ok");
    }

    /// End-to-end regression: exec a child that reads the pipe's read
    /// end on fd 3 until EOF. If the write end leaked across exec
    /// (missing FD_CLOEXEC), the child would never see EOF because it
    /// would be holding the write end open itself. Use a payload
    /// larger than the OS pipe buffer so the writer is guaranteed to
    /// still be holding the write end at fork time — otherwise the
    /// writer might finish and drop the fd before fork, which is the
    /// safe path and wouldn't exercise CLOEXEC.
    #[test]
    fn payload_pipe_child_sees_eof_when_write_end_leaks_past_fork() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let big = vec![b'x'; 256 * 1024];
        let (read_end, writer) = create_payload_pipe(big).expect("create pipe");
        let raw_read_fd = read_end.as_raw_fd();

        let mut cmd = Command::new("sh");
        cmd.args(["-c", "cat <&3 > /dev/null"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: dup2 is async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(raw_read_fd, 3) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd.spawn().expect("spawn child");
        drop(read_end);

        let start = Instant::now();
        let status = loop {
            match child.try_wait().expect("wait") {
                Some(status) => break status,
                None if start.elapsed() > Duration::from_secs(5) => {
                    let _ = child.kill();
                    panic!("child hung reading fd 3 — write end leaked past exec");
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        assert!(status.success(), "child exited with {status:?}");
        writer.join().expect("writer thread").expect("write ok");
    }

    #[test]
    fn payload_pipe_does_not_deadlock_on_large_payload() {
        // Regression: a payload larger than the OS pipe buffer
        // (16 KiB on macOS, 64 KiB on Linux) used to deadlock the
        // parent because the write happened synchronously. The writer
        // thread now owns the write end so construction returns
        // immediately regardless of size.
        let big = vec![b'x'; 128 * 1024];

        let start = Instant::now();
        let (read_end, writer) =
            create_payload_pipe(big.clone()).expect("create pipe must not block");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "create_payload_pipe blocked on pipe write"
        );

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut file = std::fs::File::from(read_end);
            let mut buf = Vec::new();
            let result = file.read_to_end(&mut buf).map(|_| buf);
            let _ = tx.send(result);
        });

        let buf = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reader did not complete — pipe write deadlocked")
            .expect("read pipe");
        writer.join().expect("writer thread").expect("write ok");

        assert_eq!(buf.len(), big.len());
        assert_eq!(buf, big);
    }
}

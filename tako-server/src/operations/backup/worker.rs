//! Backup work that touches live SQLite files runs in a `tako-server` child
//! process. SQLite POSIX locks belong to the process: closing any descriptor
//! to a live workflow database here would release the server's own locks.

use std::ffi::OsStr;
use std::process::{Command, Stdio};

/// A worker that did not finish successfully.
#[derive(Debug)]
pub(super) struct WorkerError {
    pub(super) message: String,
    /// The process was started; when false it never ran any of its work.
    pub(super) started: bool,
}

/// Run `tako-server <args>` and return its stdout.
pub(super) fn run_worker(args: &[&OsStr]) -> Result<Vec<u8>, String> {
    run_worker_checked(args).map_err(|error| error.message)
}

/// [`run_worker`], telling a worker that never started from one that failed.
pub(super) fn run_worker_checked(args: &[&OsStr]) -> Result<Vec<u8>, WorkerError> {
    let not_started = |error: std::io::Error| WorkerError {
        message: format!("start snapshot worker: {error}"),
        started: false,
    };
    let failed = |message: String| WorkerError {
        message,
        started: true,
    };
    let (mut command, stdout_file) = command(args).map_err(not_started)?;
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(not_started)?;
    let output = child
        .wait_with_output()
        .map_err(|e| failed(format!("wait for snapshot worker: {e}")))?;
    if !output.status.success() {
        return Err(failed(format!(
            "snapshot worker failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    match stdout_file {
        Some(file) => std::fs::read(file.path())
            .map_err(|e| failed(format!("read snapshot worker output: {e}"))),
        None => Ok(output.stdout),
    }
}

/// The worker command, and the file a test worker writes its stdout to.
#[cfg(not(test))]
fn command(args: &[&OsStr]) -> std::io::Result<(Command, Option<tempfile::NamedTempFile>)> {
    let mut command = Command::new(server_executable()?);
    command.args(args);
    Ok((command, None))
}

#[cfg(test)]
fn command(args: &[&OsStr]) -> std::io::Result<(Command, Option<tempfile::NamedTempFile>)> {
    tests::test_worker_command(args).map(|(command, out)| (command, Some(out)))
}

/// The running binary, even after an upgrade replaced or deleted its file,
/// so the worker is always the same version as the server.
#[cfg(not(test))]
fn server_executable() -> std::io::Result<std::path::PathBuf> {
    if cfg!(target_os = "linux") {
        Ok("/proc/self/exe".into())
    } else {
        std::env::current_exe()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    const ARGS: &str = "TAKO_TEST_BACKUP_WORKER_ARGS";
    const OUT: &str = "TAKO_TEST_BACKUP_WORKER_OUT";
    const FAULT: &str = "TAKO_TEST_BACKUP_WORKER_FAULT";

    /// What goes wrong with a worker this thread starts next.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(in crate::operations::backup) enum Fault {
        /// The process cannot be started, as when fork fails.
        SpawnFails,
        /// The worker does its work, then dies before it replies.
        CrashAfterRun,
    }

    thread_local! {
        static FAULTS: std::cell::RefCell<std::collections::VecDeque<Option<Fault>>> =
            const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    }

    /// Apply `faults` to the next workers this thread starts, in order;
    /// `None` starts one normally.
    pub(in crate::operations::backup) fn inject_faults(faults: &[Option<Fault>]) {
        FAULTS.with(|queue| queue.borrow_mut().extend(faults.iter().copied()));
    }

    fn next_fault() -> Option<Fault> {
        FAULTS.with(|queue| queue.borrow_mut().pop_front().flatten())
    }

    /// The test binary has no `tako-server` flags: run this module's
    /// [`worker`] test in a child process instead, which handles the
    /// worker flags the way `main` does and writes its stdout to a file.
    pub(super) fn test_worker_command(
        args: &[&OsStr],
    ) -> std::io::Result<(std::process::Command, tempfile::NamedTempFile)> {
        let fault = next_fault();
        if fault == Some(Fault::SpawnFails) {
            return Err(std::io::Error::from_raw_os_error(libc::EAGAIN));
        }
        let out = tempfile::NamedTempFile::new()?;
        let encoded: Vec<String> = args.iter().map(|arg| hex::encode(arg.as_bytes())).collect();
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .args([
                "--exact",
                "operations::backup::worker::tests::worker",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ARGS, encoded.join(","))
            .env(OUT, out.path());
        if fault == Some(Fault::CrashAfterRun) {
            command.env(FAULT, "crash");
        }
        Ok((command, out))
    }

    fn handle(args: &[OsString]) -> Result<Vec<u8>, String> {
        let paths = || args[1..].iter().map(PathBuf::from).collect::<Vec<_>>();
        match args[0].to_str() {
            Some("--snapshot-data") => {
                let paths = paths();
                crate::operations::snapshot_data_tree(&paths[0], &paths[1], &paths[2])
                    .map(|()| Vec::new())
            }
            Some("--snapshot-databases") => {
                let paths = paths();
                crate::operations::snapshot_database_tree(&paths[0], &paths[1], &paths[2])
                    .map(|()| Vec::new())
            }
            Some("--measure-databases") => {
                let paths = paths();
                crate::operations::measure_database_tree(&paths[0], &paths[1])
                    .map(|size| format!("{size}\n").into_bytes())
            }
            Some("--restore-worker") => {
                crate::operations::run_restore_worker(&args[1].to_string_lossy())
                    .map(String::into_bytes)
            }
            _ => Err(format!("unknown worker flag {:?}", args[0])),
        }
    }

    #[test]
    fn worker() {
        let Some(encoded) = std::env::var_os(ARGS) else {
            return;
        };
        let args: Vec<OsString> = encoded
            .to_string_lossy()
            .split(',')
            .map(|arg| OsStr::from_bytes(&hex::decode(arg).unwrap()).to_os_string())
            .collect();
        let result = handle(&args);
        if std::env::var_os(FAULT).is_some_and(|fault| fault == "crash") {
            std::process::exit(101);
        }
        match result {
            Ok(stdout) => std::fs::write(std::env::var_os(OUT).unwrap(), stdout).unwrap(),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
}

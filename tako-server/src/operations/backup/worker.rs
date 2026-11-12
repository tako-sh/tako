//! Backup work that touches live SQLite files runs in a `tako-server` child
//! process. SQLite POSIX locks belong to the process: closing any descriptor
//! to a live workflow database here would release the server's own locks.

use std::ffi::OsStr;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// A worker that did not finish successfully.
#[derive(Debug)]
pub(super) struct WorkerError {
    pub(super) message: String,
    /// The process was started; when false it never ran any of its work.
    pub(super) started: bool,
}

/// Run `tako-server <args>` and return its stdout.
pub(super) fn run_worker(args: &[&OsStr]) -> Result<Vec<u8>, String> {
    run_worker_checked(args, None).map_err(|error| error.message)
}

/// [`run_worker`], telling a worker that never started from one that failed.
/// A worker still running after `timeout` is killed and counts as failed.
/// Blocks the calling thread until the worker is gone.
pub(super) fn run_worker_checked(
    args: &[&OsStr],
    timeout: Option<Duration>,
) -> Result<Vec<u8>, WorkerError> {
    let not_started = |error: std::io::Error| WorkerError {
        message: format!("start snapshot worker: {error}"),
        started: false,
    };
    let failed = |message: String| WorkerError {
        message,
        started: true,
    };
    let (mut command, stdout_file) = command(args).map_err(not_started)?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(not_started)?;
    let readers = [
        child.stdout.take().map(drain),
        child.stderr.take().map(drain),
    ];
    let status = wait(&mut child, timeout);
    let [stdout, stderr] = readers.map(|reader| {
        reader
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default()
    });
    let status = match status {
        Ok(Some(status)) => status,
        Ok(None) => {
            return Err(failed(format!(
                "snapshot worker did not finish within {} seconds and was stopped",
                timeout.unwrap_or_default().as_secs()
            )));
        }
        Err(error) => return Err(failed(format!("wait for snapshot worker: {error}"))),
    };
    if !status.success() {
        return Err(failed(format!(
            "snapshot worker failed ({status}): {}",
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    match stdout_file {
        Some(file) => std::fs::read(file.path())
            .map_err(|e| failed(format!("read snapshot worker output: {e}"))),
        None => Ok(stdout),
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

/// The worker's exit status, or `None` once it ran past `timeout` and was
/// killed. Returns only after the process is gone.
fn wait(child: &mut Child, timeout: Option<Duration>) -> std::io::Result<Option<ExitStatus>> {
    let Some(timeout) = timeout else {
        return child.wait().map(Some);
    };
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            // Even if the kill fails the wait below returns once it exits.
            let _ = child.kill();
            child.wait()?;
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
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

    /// What goes wrong with a worker.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(in crate::operations::backup) enum Fault {
        /// The process cannot be started, as when fork fails.
        SpawnFails,
        /// The worker does its work, then dies before it replies.
        CrashAfterRun,
        /// The worker does its work, then never exits.
        HangAfterRun,
    }

    type Faults = Vec<(String, std::collections::VecDeque<Fault>)>;
    static FAULTS: std::sync::Mutex<Faults> = std::sync::Mutex::new(Vec::new());

    /// Apply `faults`, in order, to the next workers whose arguments
    /// mention `key` (a test's own temporary directory).
    pub(in crate::operations::backup) fn inject_faults(key: &std::path::Path, faults: &[Fault]) {
        FAULTS.lock().unwrap().push((
            key.to_string_lossy().into_owned(),
            faults.iter().copied().collect(),
        ));
    }

    fn next_fault(args: &[&OsStr]) -> Option<Fault> {
        let args: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        let mut faults = FAULTS.lock().unwrap();
        let (_, queue) = faults
            .iter_mut()
            .find(|(key, _)| args.iter().any(|arg| arg.contains(key.as_str())))?;
        queue.pop_front()
    }

    /// The test binary has no `tako-server` flags: run this module's
    /// [`worker`] test in a child process instead, which handles the
    /// worker flags the way `main` does and writes its stdout to a file.
    pub(super) fn test_worker_command(
        args: &[&OsStr],
    ) -> std::io::Result<(std::process::Command, tempfile::NamedTempFile)> {
        let fault = next_fault(args);
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
        match fault {
            Some(Fault::CrashAfterRun) => {
                command.env(FAULT, "crash");
            }
            Some(Fault::HangAfterRun) => {
                command.env(FAULT, "hang");
            }
            _ => {}
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
        match std::env::var_os(FAULT).as_deref().and_then(OsStr::to_str) {
            Some("crash") => std::process::exit(101),
            Some("hang") => std::thread::sleep(std::time::Duration::from_secs(3600)),
            _ => {}
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

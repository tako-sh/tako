//! Backup work that touches live SQLite files runs in a `tako-server` child
//! process. SQLite POSIX locks belong to the process: closing any descriptor
//! to a live workflow database here would release the server's own locks.

use std::ffi::OsStr;

/// Run `tako-server <args>` and return its stdout.
pub(super) fn run_worker(args: &[&OsStr]) -> Result<Vec<u8>, String> {
    #[cfg(not(test))]
    let output = std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .args(args)
        .output();
    #[cfg(test)]
    let output = tests::run_test_worker(args);
    let output = output.map_err(|e| format!("start snapshot worker: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "snapshot worker failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

#[cfg(test)]
pub(super) mod tests {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    const ARGS: &str = "TAKO_TEST_BACKUP_WORKER_ARGS";
    const OUT: &str = "TAKO_TEST_BACKUP_WORKER_OUT";

    /// The test binary has no `tako-server` flags: run this module's
    /// [`worker`] test in a child process instead, which handles the
    /// worker flags the way `main` does and writes its stdout to a file.
    pub(in crate::operations::backup) fn run_test_worker(
        args: &[&OsStr],
    ) -> std::io::Result<std::process::Output> {
        let out = tempfile::NamedTempFile::new()?;
        let encoded: Vec<String> = args.iter().map(|arg| hex::encode(arg.as_bytes())).collect();
        let mut output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "operations::backup::worker::tests::worker",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ARGS, encoded.join(","))
            .env(OUT, out.path())
            .output()?;
        output.stdout = std::fs::read(out.path())?;
        Ok(output)
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
        match handle(&args) {
            Ok(stdout) => std::fs::write(std::env::var_os(OUT).unwrap(), stdout).unwrap(),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
}

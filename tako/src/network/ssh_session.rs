//! Share one SSH transport per server so WireGuard's peer endpoint never roams
//! between simultaneous CLI sessions using the same device identity.
use std::fs::{self, File};
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::Result;
use crate::config::LocalNetwork;

pub(super) fn lock(alias: &LocalNetwork, name: &str) -> Result<File> {
    let directory = alias.profile_path()?.parent().unwrap().join("wireguard");
    super::private_directory(&directory)?;
    super::private_file(&directory.join(name), true)
}

fn control_directory() -> Result<PathBuf> {
    let directory =
        fs::canonicalize("/tmp")?.join(format!("tako-ssh-{}", unsafe { libc::geteuid() }));
    super::private_directory(&directory)?;
    Ok(directory)
}

fn prefix(alias: &LocalNetwork) -> String {
    hex::encode(Sha256::digest(format!("{}:{}", alias.id, alias.device)))[..24].to_string()
}

fn control(path: &std::path::Path, operation: &str) -> Result<std::process::Output> {
    Ok(Command::new("ssh")
        // These operations only use the local socket. OpenSSH still expands
        // default known-hosts paths, which can fail for a home containing '%'.
        .args([
            "-F",
            "/dev/null",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-S",
        ])
        .arg(path)
        .args(["-O", operation, "tako"])
        .stdin(Stdio::null())
        .output()?)
}

/// Called with connection.lock held. Existing foreground sessions must finish
/// before switching from their userspace transport to a system interface.
pub(super) fn prepare_connect(alias: &LocalNetwork) -> Result<File> {
    let sessions = lock(alias, "ssh-sessions.lock")?;
    sessions
        .try_lock()
        .map_err(|_| "Finish active `tako ssh` sessions before changing the network connection")?;
    stop_masters(alias)?;
    let proxies = lock(alias, "ssh-proxies.lock")?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match proxies.try_lock() {
            Ok(()) => return Ok(proxies),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(format!("Could not stop the private SSH transport: {error}").into());
            }
        }
    }
}

pub(super) fn stop_masters(alias: &LocalNetwork) -> Result<()> {
    let prefix = format!("{}-", prefix(alias));
    for entry in fs::read_dir(control_directory()?)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&prefix)
            && entry.file_type()?.is_socket()
        {
            let _ = control(&entry.path(), "exit")?;
        }
    }
    Ok(())
}

pub(crate) fn run(
    alias: &LocalNetwork,
    server: &str,
    base: &Command,
    destination: &str,
    settings: &str,
    remote: &[String],
) -> Result<ExitStatus> {
    let gate = lock(alias, "connection.lock")?;
    gate.lock()?;
    let sessions = lock(alias, "ssh-sessions.lock")?;
    sessions.lock_shared()?;
    let path = control_directory()?.join(format!(
        "{}-{}",
        prefix(alias),
        &hex::encode(Sha256::digest(server))[..24]
    ));
    let command = |multiplex_only: bool| {
        let mut command = Command::new(base.get_program());
        if multiplex_only {
            // OpenSSH uses the first value of an option.
            command.args(["-o", "ProxyCommand=false"]);
        }
        command.args(base.get_args()).arg("-S").arg(&path);
        command
    };
    let fingerprint = hex::encode(Sha256::digest(settings));
    let metadata_path = path.with_extension("settings");
    if control(&path, "check")?.status.success() {
        let mut previous = String::new();
        use std::io::Read;
        super::private_file(&metadata_path, false)?
            .take(65)
            .read_to_string(&mut previous)?;
        if previous != fingerprint {
            return Err("SSH approval changed. Finish existing SSH sessions, wait five seconds, then retry.".into());
        }
    } else {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&path)?,
            Ok(_) => return Err("The SSH control path is not a socket".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        super::write_bytes(&metadata_path, fingerprint.as_bytes(), true)?;
        let status = command(false)
            .args([
                "-M",
                "-N",
                "-f",
                "-o",
                "ControlPersist=5",
                "-o",
                "BatchMode=yes",
            ])
            .arg(destination)
            .stdin(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(format!("Could not establish private SSH: {status}").into());
        }
    }
    // Start the channel while the connection gate still prevents connect from
    // stopping the master. A missing master must not start a second WG tunnel.
    let mut child = command(true)
        .args(["-o", "ControlMaster=no"])
        .arg(destination)
        .args(remote)
        .spawn()?;
    drop(gate);
    let result = child.wait();
    drop(sessions);
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    #[ignore = "requires the isolated OpenSSH server in e2e/cli/rootless-ssh.py"]
    fn multiplex_and_connect_exclusion() {
        assert_ne!(unsafe { libc::geteuid() }, 0);
        let alias = LocalNetwork {
            name: "test".into(),
            id: "11".repeat(16),
            device: "22".repeat(32),
        };
        let output = tempfile::tempdir().unwrap();
        let first = output.path().join("first");
        let second = output.path().join("second");
        let base = || {
            let mut command = Command::new("ssh");
            command.args([
                "-F",
                "/dev/null",
                "-o",
                &format!(
                    "IdentityFile={}",
                    super::super::local_access::quote_path(Path::new(
                        &std::env::var("TAKO_TEST_SSH_KEY").unwrap()
                    ))
                    .unwrap()
                ),
                "-p",
                "2223",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "IdentityAgent=none",
                "-o",
                &format!(
                    "UserKnownHostsFile={}",
                    super::super::local_access::quote_path(Path::new(
                        &std::env::var("TAKO_TEST_SSH_HOSTS").unwrap()
                    ))
                    .unwrap()
                ),
            ]);
            command
        };
        let run_alias = alias.clone();
        let first_path = first.clone();
        let worker = std::thread::spawn(move || {
            run(
                &run_alias,
                "server",
                &base(),
                "root@127.0.0.1",
                "approved",
                &[format!(
                    "printf '%s' \"$SSH_CONNECTION\" > {}; sleep 3",
                    first_path.display()
                )],
            )
            .map_err(|error| error.to_string())
            .unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        while !first.exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        let gate = lock(&alias, "connection.lock").unwrap();
        gate.lock().unwrap();
        assert!(
            prepare_connect(&alias)
                .unwrap_err()
                .to_string()
                .contains("Finish active")
        );
        drop(gate);
        assert!(
            run(
                &alias,
                "server",
                &base(),
                "root@127.0.0.1",
                "changed-approval",
                &["true".into()]
            )
            .is_err()
        );
        assert!(
            run(
                &alias,
                "server",
                &base(),
                "root@127.0.0.1",
                "approved",
                &[format!(
                    "printf '%s' \"$SSH_CONNECTION\" > {}",
                    second.display()
                )]
            )
            .unwrap()
            .success()
        );
        assert!(worker.join().unwrap().success());
        assert_eq!(
            fs::read(&first).unwrap(),
            fs::read(&second).unwrap(),
            "both channels must use the same authenticated TCP connection"
        );
        let gate = lock(&alias, "connection.lock").unwrap();
        gate.lock().unwrap();
        prepare_connect(&alias).unwrap();
    }
}

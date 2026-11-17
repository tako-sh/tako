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

/// Holds this device's only userspace WireGuard transport to `server`.
///
/// Two transports with the same device key make the server roam its peer
/// endpoint between them. OpenSSH clients that start before a shared control
/// socket exists each run a proxy, so the proxy itself must refuse to open a
/// second transport. The lock lives as long as the returned file.
pub(crate) fn claim_transport(alias: &LocalNetwork, server: &str) -> Result<File> {
    let name = format!(
        "ssh-transport-{}.lock",
        &hex::encode(Sha256::digest(server))[..12]
    );
    let transport = lock(alias, &name)?;
    match transport.try_lock() {
        Ok(()) => Ok(transport),
        Err(std::fs::TryLockError::WouldBlock) => Err(
            "Another SSH connection to this server is starting or open without a shared connection. Retry in a moment; `tako ssh` sessions reuse one connection per server."
                .into(),
        ),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn control_directory_path() -> Result<PathBuf> {
    Ok(fs::canonicalize("/tmp")?.join(format!("tako-ssh-{}", unsafe { libc::geteuid() })))
}

pub(super) fn control_directory() -> Result<PathBuf> {
    let directory = control_directory_path()?;
    super::private_directory(&directory)?;
    Ok(directory)
}

fn prefix(alias: &LocalNetwork) -> String {
    hex::encode(Sha256::digest(format!("{}:{}", alias.id, alias.device)))[..24].to_string()
}

fn server_prefix(alias: &LocalNetwork, server: &str) -> String {
    format!(
        "{}-{}-",
        prefix(alias),
        &hex::encode(Sha256::digest(server))[..12]
    )
}

/// The OpenSSH control socket for one server and one approved SSH setting.
/// Concurrent `tako ssh` sessions derive the same path, so they share a single
/// WireGuard transport. Approval changes yield a new path
/// instead of reusing a connection authenticated under the old approval.
/// Computes the path only; callers that bind or scan create the directory.
pub(crate) fn control_path(alias: &LocalNetwork, server: &str, settings: &str) -> Result<PathBuf> {
    Ok(control_directory_path()?.join(format!(
        "{}{}",
        server_prefix(alias, server),
        &hex::encode(Sha256::digest(settings))[..12]
    )))
}

/// The settings string that identifies an approved SSH destination.
pub(crate) fn settings(user: &str, port: u16, host_key: &str, address: &str) -> String {
    format!("{user}:{port}:{host_key}:{address}")
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

/// Fails while a live control master for this server exists under another
/// path: one from an earlier approval, or from a release before
/// settings-derived paths. A second WireGuard transport would roam the
/// server's peer endpoint away from it. `own` is the caller's control path.
pub(super) fn refuse_other_transport(
    alias: &LocalNetwork,
    server: &str,
    own: Option<&std::path::Path>,
) -> Result<()> {
    let siblings = server_prefix(alias, server);
    let legacy = format!(
        "{}-{}",
        prefix(alias),
        &hex::encode(Sha256::digest(server))[..24]
    );
    let own = own.map(|path| path.file_name().unwrap().to_string_lossy().into_owned());
    for entry in fs::read_dir(control_directory()?)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // OpenSSH binds `<path>.<random>` before renaming it into place.
        if !own.as_deref().is_some_and(|own| name.starts_with(own))
            && (name.starts_with(&siblings) || name == legacy)
            && entry.file_type()?.is_socket()
            && control(&entry.path(), "check")?.status.success()
        {
            return Err("SSH approval changed. Finish existing SSH sessions to this server, wait five seconds, then retry.".into());
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
    let path = control_path(alias, server, settings)?;
    let command = |multiplex_only: bool| {
        let mut command = Command::new(base.get_program());
        if multiplex_only {
            // OpenSSH uses the first value of an option.
            command.args(["-o", "ProxyCommand=false"]);
        }
        command.args(base.get_args()).arg("-S").arg(&path);
        command
    };
    refuse_other_transport(alias, server, Some(&path))?;
    if !control(&path, "check")?.status.success() {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&path)?,
            Ok(_) => return Err("The SSH control path is not a socket".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
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

    #[test]
    fn only_one_transport_per_server_and_device() {
        let _home = crate::network::tests::TestHome::new();
        let alias = LocalNetwork {
            name: "test".into(),
            id: "11".repeat(16),
            device: "22".repeat(32),
        };
        let first = claim_transport(&alias, "server-a").unwrap();
        assert!(claim_transport(&alias, "server-a").is_err());
        // Other servers have their own transports.
        let other = claim_transport(&alias, "server-b").unwrap();
        drop(first);
        claim_transport(&alias, "server-a").unwrap();
        drop(other);
    }
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

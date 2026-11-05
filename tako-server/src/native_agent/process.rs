//! Own only children returned by our Command::spawn. Never discover/kill PIDs.
use super::{AgentError, MAX_HELPER_BYTES, Result};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tako_network::NodeConfig;

const CORROSION: &str = "/usr/local/libexec/tako/corrosion";
const HELPER: &str = "/usr/local/libexec/tako/tako-network-helper";
const POLL: Duration = Duration::from_millis(25);
pub(super) struct OwnedChild {
    child: Option<Child>,
}

impl OwnedChild {
    pub(super) fn corrosion(config: &NodeConfig, data_dir: &std::path::Path) -> Result<Self> {
        // Installer controls the executable and all its ancestors. Never execute
        // a user-writable substitute, even if its bytes have the expected digest.
        for directory in [
            "/usr",
            "/usr/local",
            "/usr/local/libexec",
            "/usr/local/libexec/tako",
        ] {
            let meta = std::fs::symlink_metadata(directory)?;
            if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
                return Err(AgentError::Invalid("unsafe Corrosion installation"));
            }
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(CORROSION)?;
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.nlink() != 1
            || meta.uid() != 0
            || meta.mode() & 0o022 != 0
            || meta.len() > 256 * 1024 * 1024
        {
            return Err(AgentError::Invalid("unsafe Corrosion executable"));
        }
        let mut digest = openssl::sha::Sha256::new();
        let mut chunk = [0u8; 65536];
        loop {
            let n = file.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            digest.update(&chunk[..n]);
        }
        if hex::encode(digest.finish()) != config.corrosion_binary_sha256 {
            return Err(AgentError::Invalid(
                "Corrosion executable checksum mismatch",
            ));
        }
        let child = Command::new(CORROSION)
            .arg("--config")
            .arg(config.network_dir(data_dir).join("corrosion.toml"))
            .arg("agent")
            // Corrosion permits environment config overrides. Clear them all.
            .env_clear()
            .env("RUST_LOG", "warn")
            .current_dir(config.network_dir(data_dir))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self { child: Some(child) })
    }
    pub(super) fn running(&mut self) -> Result<bool> {
        let Some(child) = &mut self.child else {
            return Ok(false);
        };
        if child.try_wait()?.is_some() {
            self.child = None;
            return Ok(false);
        }
        Ok(true)
    }
    /// Forced kill is an error even after successful reaping. Callers may retry
    /// only after inspecting `running`; they must retain/report the timeout.
    pub(super) fn stop(&mut self) -> Result<()> {
        let Some(child) = &mut self.child else {
            return Ok(());
        };
        if child.try_wait()?.is_some() {
            self.child = None;
            return Err(AgentError::Invalid("owned child exited unexpectedly"));
        }
        // Child has not been reaped, so its PID cannot be reused here.
        let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait()? {
                self.child = None;
                return if status.success() {
                    Ok(())
                } else {
                    Err(AgentError::Invalid("owned child shutdown was unsuccessful"))
                };
            }
            thread::sleep(POLL);
        }
        child.kill()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                self.child = None;
                return Err(AgentError::ForcedShutdown);
            }
            thread::sleep(POLL);
        }
        Err(AgentError::UnreapedChild)
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::error!(error = %error, "native agent child cleanup failed");
        }
    }
}

pub(super) fn apply(config: &NodeConfig, desired: &[u8]) -> Result<()> {
    let mut command = Command::new(HELPER);
    command
        .arg(config.network.id.to_string())
        .arg("apply")
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin");
    let bytes = bounded_output(command, Duration::from_secs(45), MAX_HELPER_BYTES)?;
    validate_helper_response(config, desired, &bytes)
}

fn validate_helper_response(config: &NodeConfig, desired: &[u8], bytes: &[u8]) -> Result<()> {
    let response: serde_json::Value = serde_json::from_slice(bytes)?;
    let state: serde_json::Value = serde_json::from_slice(desired)?;
    let ids: std::collections::BTreeSet<&str> = state["peers"]
        .as_array()
        .ok_or(AgentError::Invalid("desired peers missing"))?
        .iter()
        .map(|p| {
            p["device_id"]
                .as_str()
                .ok_or(AgentError::Invalid("desired peer ID missing"))
        })
        .collect::<Result<_>>()?;
    let ack = response["applied_device_ids"]
        .as_array()
        .ok_or(AgentError::Invalid("helper omitted applied IDs"))?;
    let applied: std::collections::BTreeSet<&str> = ack
        .iter()
        .map(|id| {
            id.as_str()
                .ok_or(AgentError::Invalid("invalid helper peer ID"))
        })
        .collect::<Result<_>>()?;
    let network = config.network.id.to_string();
    let address = state["address"]
        .as_str()
        .and_then(|s| s.strip_suffix("/128"));
    if response["ok"] != true
        || response["action"] != "apply"
        || response["network_id"] != network
        || response["interface"] != format!("tako{}", &network[..10])
        || response["address"].as_str() != address
        || ids != applied
        || ack.len() != applied.len()
    {
        return Err(AgentError::Invalid(
            "helper did not acknowledge exact desired state",
        ));
    }
    Ok(())
}

/// Pipes are nonblocking and drained with one byte cap; no unbounded output
/// reader threads or `wait_with_output`. A timeout kills/reaps only our client.
fn bounded_output(mut command: Command, timeout: Duration, limit: usize) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut pipe = child
        .stdout
        .take()
        .ok_or(AgentError::Invalid("missing child stdout"))?;
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::last_os_error().into());
    }
    let mut owned = OwnedChild { child: Some(child) };
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut eof = false;
    loop {
        let mut chunk = [0u8; 4096];
        match pipe.read(&mut chunk) {
            Ok(0) => eof = true,
            Ok(n) => {
                if bytes.len() + n > limit {
                    return Err(AgentError::Invalid("child output exceeds limit"));
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => (),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
        if eof {
            if let Some(status) = owned.child.as_mut().unwrap().try_wait()? {
                owned.child = None;
                return if status.success() {
                    Ok(bytes)
                } else {
                    Err(AgentError::Invalid("helper process failed"))
                };
            }
        }
        if Instant::now() >= deadline {
            return Err(AgentError::Timeout);
        }
        thread::sleep(POLL);
    }
}

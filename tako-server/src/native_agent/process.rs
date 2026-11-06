//! Own only children returned by our Command::spawn. Never discover/kill PIDs.
use super::{AgentError, MAX_HELPER_BYTES, Result};
use std::{
    fs::OpenOptions,
    io::Read,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
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
        // Execute the exact verified inode even during an installer rename.
        let executable = format!("/proc/self/fd/{}", file.as_raw_fd());
        let mut command = Command::new(executable);
        kill_with_agent(&mut command);
        let child = command
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
        self.stop_with_deadlines(Duration::from_secs(15), Duration::from_secs(5))
    }
    fn stop_with_deadlines(&mut self, graceful: Duration, kill: Duration) -> Result<()> {
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
        let deadline = Instant::now() + graceful;
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
        let deadline = Instant::now() + kill;
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

/// A fail-stop deadline must not orphan owned processes. Install this before
/// exec (only async-signal-safe syscalls); also close the parent-death race.
fn kill_with_agent(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
            }
            Ok(())
        });
    }
}

/// DNS is a separate helper outcome: degraded split DNS does not negate the
/// exact peer/SSH enforcement ACK, but must remain retryable and observable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DnsStatus {
    pub dns_supported: bool,
    pub dns_error: Option<String>,
}
impl DnsStatus {
    fn report(&self) {
        if self.dns_supported {
            tracing::info!(dns_supported = true, "native host DNS configured");
        } else {
            tracing::warn!(dns_supported = false, dns_error = ?self.dns_error,
                "native host DNS degraded; membership enforcement acknowledged");
        }
    }
}

pub(super) fn apply(config: &NodeConfig, desired: &[u8]) -> Result<DnsStatus> {
    let mut command = Command::new(HELPER);
    command
        .arg(config.network.id.to_string())
        .arg("apply")
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin");
    let bytes = bounded_output(command, Duration::from_secs(45), MAX_HELPER_BYTES)?;
    let dns = validate_helper_response(config, desired, &bytes)?;
    dns.report();
    Ok(dns)
}

pub(super) fn validate_helper_response(
    config: &NodeConfig,
    desired: &[u8],
    bytes: &[u8],
) -> Result<DnsStatus> {
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
    let dns_supported = match response.get("dns_supported") {
        Some(value) => value
            .as_bool()
            .ok_or(AgentError::Invalid("invalid helper DNS status"))?,
        // Older helpers must not silently become permanently cached DNS success.
        None => false,
    };
    let dns_error = match response.get("dns_error") {
        Some(serde_json::Value::Null) | None => None,
        Some(serde_json::Value::String(error)) if error.len() <= 1024 => Some(error.clone()),
        _ => return Err(AgentError::Invalid("invalid helper DNS diagnostic")),
    };
    if dns_supported && dns_error.is_some() {
        return Err(AgentError::Invalid("contradictory helper DNS status"));
    }
    Ok(DnsStatus {
        dns_supported,
        dns_error: dns_error.or_else(|| {
            (!dns_supported).then(|| "helper did not acknowledge host DNS configuration".to_owned())
        }),
    })
}

/// Pipes are nonblocking and drained with one byte cap; no unbounded output
/// reader threads or `wait_with_output`. A timeout kills/reaps only our client.
fn bounded_output(mut command: Command, timeout: Duration, limit: usize) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    kill_with_agent(&mut command);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut pipe = child
        .stdout
        .take()
        .ok_or(AgentError::Invalid("missing child stdout"))?;
    let mut owned = OwnedChild { child: Some(child) };
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helper_success_requires_exact_applied_state() {
        let fixture = super::super::tests::Fixture::new();
        let bytes = super::super::projection::desired(
            &fixture.config,
            &fixture.keys,
            &fixture.config.bootstrap,
            2,
            false,
        )
        .unwrap();
        let desired: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let id = fixture.config.network.id.to_string();
        let ack = serde_json::json!({"ok": true, "action": "apply", "network_id": id,
            "interface": format!("tako{}", &id[..10]),
            "address": desired["address"].as_str().unwrap().trim_end_matches("/128"),
            "applied_device_ids": [desired["peers"][0]["device_id"].clone()]});
        let parse = |ack: &serde_json::Value| {
            validate_helper_response(&fixture.config, &bytes, &serde_json::to_vec(ack).unwrap())
        };
        // Missing DNS metadata is unknown/degraded, never permanent success.
        let unknown = parse(&ack).unwrap();
        assert!(!unknown.dns_supported);
        assert!(unknown.dns_error.is_some());
        let mut dns_ack = ack.clone();
        dns_ack["dns_supported"] = serde_json::json!(false);
        dns_ack["dns_error"] =
            serde_json::json!("native service DNS requires active systemd-resolved");
        let degraded = parse(&dns_ack).unwrap();
        assert!(!degraded.dns_supported);
        assert_eq!(
            degraded.dns_error.as_deref(),
            Some("native service DNS requires active systemd-resolved")
        );
        dns_ack["dns_supported"] = serde_json::json!(true);
        assert!(parse(&dns_ack).is_err()); // contradictory success/error
        dns_ack.as_object_mut().unwrap().remove("dns_error");
        let recovered = parse(&dns_ack).unwrap();
        assert!(recovered.dns_supported);
        assert_eq!(recovered.dns_error, None);
        dns_ack["dns_supported"] = serde_json::json!("true");
        assert!(parse(&dns_ack).is_err());
        dns_ack["dns_supported"] = serde_json::json!(false);
        for diagnostic in [serde_json::json!(7), serde_json::json!("x".repeat(1025))] {
            dns_ack["dns_error"] = diagnostic;
            assert!(parse(&dns_ack).is_err());
        }
        for (field, value) in [
            ("ok", serde_json::json!(false)),
            ("action", serde_json::json!("remove")),
            ("address", serde_json::json!("fd00::1")),
            ("applied_device_ids", serde_json::json!([])),
        ] {
            let mut invalid = ack.clone();
            invalid[field] = value;
            assert!(
                validate_helper_response(
                    &fixture.config,
                    &bytes,
                    &serde_json::to_vec(&invalid).unwrap()
                )
                .is_err()
            );
        }
    }
    #[test]
    fn fake_command_output_and_deadline_are_bounded() {
        let mut small = Command::new("/bin/sh");
        small.args(["-c", "printf ok"]);
        assert_eq!(
            bounded_output(small, Duration::from_secs(1), 2).unwrap(),
            b"ok"
        );
        let mut oversized = Command::new("/bin/sh");
        oversized.args(["-c", "printf oversized"]);
        assert!(bounded_output(oversized, Duration::from_secs(1), 2).is_err());
        let mut slow = Command::new("/bin/sleep");
        slow.arg("5");
        assert!(matches!(
            bounded_output(slow, Duration::from_millis(30), 2),
            Err(AgentError::Timeout)
        ));
    }
    #[test]
    fn owned_commands_install_parent_death_kill_before_exec() {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("/bin/true");
        kill_with_agent(&mut command);
        unsafe {
            command.pre_exec(|| {
                let mut signal = 0i32;
                if libc::prctl(libc::PR_GET_PDEATHSIG, &mut signal) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if signal != libc::SIGKILL {
                    return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                }
                Ok(())
            });
        }
        assert!(command.status().unwrap().success());
    }
    #[test]
    fn forced_child_shutdown_is_reported_and_reaped() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; printf ready; exec sleep 5"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = [0; 5];
        child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
        let mut owned = OwnedChild { child: Some(child) };
        assert!(matches!(
            owned.stop_with_deadlines(Duration::from_millis(30), Duration::from_secs(1)),
            Err(AgentError::ForcedShutdown)
        ));
        assert!(owned.child.is_none());
    }
}

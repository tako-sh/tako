use super::super::{AppConfig, Instance};
use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use tokio::process::Command;

#[cfg(unix)]
pub(super) fn resolve_app_user() -> std::io::Result<Option<(u32, u32)>> {
    match crate::unix::lookup_user_ids("tako-app")? {
        Some((uid, gid)) => {
            tracing::info!(uid, gid, "Resolved tako-app user for app process isolation");
            Ok(Some((uid, gid)))
        }
        None => {
            tracing::warn!("tako-app user not found");
            Ok(None)
        }
    }
}

pub(super) fn build_instance_env(
    config: &AppConfig,
    _instance: &Instance,
    internal_socket: Option<&Path>,
) -> HashMap<String, String> {
    let mut env = config.env_vars.clone();

    // The Tako runtime contract (PORT=0, HOST loopback, TAKO_APP_NAME, and
    // TAKO_INTERNAL_SOCKET when available) is defined in tako-core so dev and
    // prod spawners can't drift. The internal auth token is NOT in env — it
    // travels on fd 3 with secrets so it doesn't inherit into subprocesses.
    let app_name = config.deployment_id();
    tako_core::instance_env::TakoRuntimeEnv {
        app_name: &app_name,
        internal_socket,
    }
    .apply(&mut env);

    env.entry("NODE_ENV".to_string())
        .or_insert_with(|| "production".to_string());

    env
}

/// Build the extra CLI args for the entrypoint (internal protocol, not env vars).
pub(super) fn build_instance_args(instance: &Instance) -> Vec<String> {
    vec!["--instance".to_string(), instance.id.clone()]
}

pub(super) fn app_child_parent_death_signal() -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        Some(libc::SIGTERM)
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Resolve a binary name against the app's PATH env, falling back to the bare name.
fn resolve_binary_from_env(binary: &str, env: &HashMap<String, String>) -> String {
    // Already absolute — use as-is
    if binary.starts_with('/') {
        return binary.to_string();
    }
    // Search the app's PATH
    if let Some(path_var) = env.get("PATH") {
        for dir in path_var.split(':') {
            let candidate = Path::new(dir).join(binary);
            if candidate.is_file() {
                return candidate.to_string_lossy().to_string();
            }
        }
    }
    // Fallback to bare name (Command::new will search process PATH)
    binary.to_string()
}

/// Create the bootstrap pipe on fd 3: the child reads a JSON envelope
/// `{"token": ..., "secrets": {...}}` and closes the fd.
///
/// The envelope travels on a pipe (not env/args) so the internal auth
/// token doesn't inherit into subprocesses the app spawns. The pipe is
/// always created, even with empty secrets — every Tako-managed process
/// has a bootstrap fd. The envelope shape itself lives in
/// `tako_core::bootstrap` so the workflows supervisor produces byte-for-byte
/// the same payload. See `tako_spawn::create_payload_pipe` for the
/// CLOEXEC/writer-thread semantics.
#[cfg(unix)]
pub(super) fn create_bootstrap_pipe(
    token: &str,
    secrets: &HashMap<String, String>,
    image_secret: Option<&str>,
) -> std::io::Result<(OwnedFd, std::thread::JoinHandle<std::io::Result<()>>)> {
    let bytes = tako_core::bootstrap::envelope_bytes(token, secrets, image_secret);
    tako_spawn::create_payload_pipe(bytes)
}

#[cfg(unix)]
fn create_fd_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn build_child_command(
    config: &AppConfig,
    env: &HashMap<String, String>,
    extra_args: &[String],
    app_user: Option<(u32, u32)>,
    secrets_fd: Option<RawFd>,
    readiness_fd: Option<RawFd>,
) -> std::io::Result<Command> {
    // Resolve the binary using the app's env PATH (not the server's PATH).
    let binary = resolve_binary_from_env(&config.command[0], env);
    let mut child_cmd = Command::new(&binary);
    child_cmd.args(&config.command[1..]).args(extra_args);

    #[cfg(unix)]
    if let Some((uid, gid)) = app_user {
        child_cmd.uid(uid);
        child_cmd.gid(gid);
    }

    child_cmd.current_dir(&config.path).env_clear();
    for key in ["PATH", "HOME"] {
        if let Ok(value) = std::env::var(key) {
            child_cmd.env(key, value);
        }
    }
    child_cmd
        .envs(env)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    // Pass internal runtime ABI pipes to the child.
    #[cfg(unix)]
    unsafe {
        child_cmd.pre_exec(move || {
            install_parent_death_signal(app_child_parent_death_signal())?;
            clear_ambient_capabilities()?;
            if let Some(fd) = secrets_fd {
                if fd != 3 {
                    if libc::dup2(fd, 3) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(fd);
                }
            } else {
                libc::close(3);
            }
            if let Some(fd) = readiness_fd {
                if fd != 4 {
                    if libc::dup2(fd, 4) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(fd);
                }
            } else {
                libc::close(4);
            }
            Ok(())
        });
    }

    Ok(child_cmd)
}

#[cfg(target_os = "linux")]
fn clear_ambient_capabilities() -> std::io::Result<()> {
    let result = unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn clear_ambient_capabilities() -> std::io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_parent_death_signal(signal: Option<i32>) -> std::io::Result<()> {
    let Some(signal) = signal else {
        return Ok(());
    };

    // SAFETY: This runs in the child after fork and before exec. `prctl`,
    // `getppid`, and `_exit` are used only with plain integer arguments.
    let result = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, signal) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }

    // If the parent died between fork and PR_SET_PDEATHSIG, avoid execing an
    // immediately orphaned app process.
    if unsafe { libc::getppid() } == 1 {
        unsafe { libc::_exit(1) };
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_parent_death_signal(_signal: Option<i32>) -> std::io::Result<()> {
    Ok(())
}

pub(super) fn spawn_child_process(
    config: &AppConfig,
    env: &HashMap<String, String>,
    extra_args: &[String],
    app_user: Option<(u32, u32)>,
    token: &str,
    secrets: &HashMap<String, String>,
    image_secret: &str,
) -> std::io::Result<(tokio::process::Child, Option<OwnedFd>)> {
    // Bootstrap pipe on fd 3: always present, carries `{token, secrets}`.
    // The OwnedFd must stay alive until after spawn (fork copies the fd table).
    // The writer thread owns the write end; it drains in parallel with the child
    // so large envelopes don't deadlock the parent on pipe-buffer backpressure.
    #[cfg(unix)]
    let image_secret = (!image_secret.is_empty()).then_some(image_secret);
    let (bootstrap_pipe, bootstrap_writer) = create_bootstrap_pipe(token, secrets, image_secret)?;
    #[cfg(unix)]
    let raw_fd = Some(bootstrap_pipe.as_raw_fd());
    #[cfg(not(unix))]
    let raw_fd = None;

    #[cfg(unix)]
    let (readiness_read_end, readiness_write_end) = create_fd_pipe()?;
    #[cfg(unix)]
    let readiness_raw_fd = Some(readiness_write_end.as_raw_fd());
    #[cfg(not(unix))]
    let readiness_raw_fd = None;

    let mut child_cmd =
        build_child_command(config, env, extra_args, app_user, raw_fd, readiness_raw_fd)?;
    let spawn_result = child_cmd.spawn();

    match spawn_result {
        Ok(child) => {
            #[cfg(unix)]
            {
                drop(readiness_write_end);
                // Join the bootstrap writer now that the child is draining fd 3.
                // Surface any write error; otherwise the child would see a short
                // payload and fail with a confusing parse error at startup.
                join_bootstrap_writer(bootstrap_writer)?;
                Ok((child, Some(readiness_read_end)))
            }
            #[cfg(not(unix))]
            {
                Ok((child, None))
            }
        }
        Err(error) => {
            // Spawn failed; the writer thread may still be blocked on a full
            // pipe buffer waiting for a reader that will never come. Dropping
            // the read end closes the read side, so the writer gets EPIPE and
            // exits. We then join to reap the thread.
            #[cfg(unix)]
            {
                drop(bootstrap_pipe);
                drop(readiness_write_end);
                drop(readiness_read_end);
                let _ = bootstrap_writer.join();
            }
            Err(error)
        }
    }
    // The parent-owned pipe ends drop here after spawn, leaving the child with
    // only the ABI fds it needs across exec.
}

#[cfg(unix)]
fn join_bootstrap_writer(
    handle: std::thread::JoinHandle<std::io::Result<()>>,
) -> std::io::Result<()> {
    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::other("bootstrap writer thread panicked")),
    }
}

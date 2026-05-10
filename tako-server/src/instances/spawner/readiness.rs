use super::super::{Instance, InstanceError, InstanceState};
#[cfg(unix)]
use std::os::fd::OwnedFd;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;

const STARTUP_TIMEOUT_OUTPUT_WAIT: Duration = Duration::from_secs(1);

/// Wait for the SDK to report the bound port on fd 4.
/// Sets the instance upstream once the port is learned.
pub(super) async fn wait_for_ready(
    instance: Arc<Instance>,
    readiness_fd: Option<OwnedFd>,
) -> Result<(), InstanceError> {
    let readiness_fd = readiness_fd.ok_or_else(|| {
        InstanceError::HealthCheckFailed("no readiness pipe available".to_string())
    })?;
    let readiness_file = tokio::fs::File::from_std(std::fs::File::from(readiness_fd));
    let mut lines = tokio::io::BufReader::new(readiness_file).lines();

    tokio::select! {
        line = lines.next_line() => {
            match line {
                Ok(Some(line)) => {
                    let port: u16 = line.trim().parse().map_err(|_| {
                        InstanceError::HealthCheckFailed(
                            format!("invalid port in readiness signal: {line}"),
                        )
                    })?;
                    instance.set_port(port);
                    instance.set_state(InstanceState::Ready);
                    Ok(())
                }
                Ok(None) => {
                    let detail = if instance.is_alive().await {
                        "Process closed readiness pipe before reporting a port".to_string()
                    } else {
                        startup_exit_detail(instance).await
                    };
                    Err(InstanceError::HealthCheckFailed(detail))
                }
                Err(error) => {
                    Err(InstanceError::HealthCheckFailed(
                        format!("failed to read readiness pipe: {error}"),
                    ))
                }
            }
        }
        _ = check_process_alive(&instance) => {
            let detail = startup_exit_detail(instance).await;
            Err(InstanceError::HealthCheckFailed(detail))
        }
    }
}

/// Resolves when the instance process is no longer alive.
async fn check_process_alive(instance: &Instance) {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    loop {
        interval.tick().await;
        if !instance.is_alive().await {
            return;
        }
    }
}

async fn startup_exit_detail(instance: Arc<Instance>) -> String {
    let Some(child) = instance.take_process() else {
        return "Process exited during startup".to_string();
    };

    match child.wait_with_output().await {
        Ok(output) => format_startup_exit_error(output.status, &output.stdout, &output.stderr),
        Err(error) => format!("Process exited during startup; failed to read output: {error}"),
    }
}

pub(super) async fn startup_timeout_detail(instance: Arc<Instance>, timeout: Duration) -> String {
    let Some(mut child) = instance.take_process() else {
        return format!("exceeded {timeout:?} while waiting for fd 4 readiness");
    };

    let _ = child.start_kill();

    match tokio::time::timeout(STARTUP_TIMEOUT_OUTPUT_WAIT, child.wait_with_output()).await {
        Ok(Ok(output)) => format_startup_timeout_error(timeout, &output.stdout, &output.stderr),
        Ok(Err(error)) => format!(
            "exceeded {timeout:?} while waiting for fd 4 readiness; failed to read output: {error}"
        ),
        Err(_) => format!(
            "exceeded {timeout:?} while waiting for fd 4 readiness; timed out reading startup output"
        ),
    }
}

pub(super) fn format_startup_exit_error(
    status: ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let status_text = match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated by signal".to_string(),
    };

    let stderr_text = String::from_utf8_lossy(stderr).trim().to_string();
    let stdout_text = String::from_utf8_lossy(stdout).trim().to_string();
    let detail = if !stderr_text.is_empty() {
        stderr_text
    } else {
        stdout_text
    };

    if detail.is_empty() {
        return format!("Process exited during startup ({status_text})");
    }

    let preview = truncate_chars(&detail, 400);
    format!("Process exited during startup ({status_text}): {preview}")
}

pub(super) fn format_startup_timeout_error(
    timeout: Duration,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let stderr_text = String::from_utf8_lossy(stderr).trim().to_string();
    let stdout_text = String::from_utf8_lossy(stdout).trim().to_string();
    let detail = if !stderr_text.is_empty() {
        stderr_text
    } else {
        stdout_text
    };

    if detail.is_empty() {
        return format!("exceeded {timeout:?} while waiting for fd 4 readiness");
    }

    let preview = truncate_chars(&detail, 400);
    format!("exceeded {timeout:?} while waiting for fd 4 readiness: {preview}")
}

pub(super) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

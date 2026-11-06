//! Preserve structured stdout and bounded diagnostic tails without blocking pipes.

use std::io;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;

tokio::task_local! {
    static DRAIN_REQUESTED: std::sync::Arc<std::sync::atomic::AtomicBool>;
}

pub(crate) fn check_cancellation() -> io::Result<()> {
    if DRAIN_REQUESTED
        .try_with(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
        .unwrap_or(false)
    {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Server drain cancelled preparation",
        ))
    } else {
        Ok(())
    }
}

async fn cancelled() {
    let Ok(flag) = DRAIN_REQUESTED.try_with(Clone::clone) else {
        return std::future::pending().await;
    };
    loop {
        if flag.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn owned_process_group(child: &Child) -> Option<i32> {
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|id| i32::try_from(id).ok()) {
        // Only signal a group created for this child, never the server's group.
        if unsafe { libc::getpgid(pid) } == pid {
            return Some(pid);
        }
    }
    None
}

async fn stop_and_reap(child: &mut Child, group: Option<i32>) -> io::Result<()> {
    #[cfg(unix)]
    let group_error = group.and_then(|group| {
        if unsafe { libc::kill(-group, libc::SIGKILL) } == 0 {
            return None;
        }
        let error = io::Error::last_os_error();
        (error.raw_os_error() != Some(libc::ESRCH)).then_some(error)
    });
    if child.try_wait()?.is_none() {
        child.kill().await?;
    }
    child.wait().await?;
    #[cfg(unix)]
    if let Some(error) = group_error {
        return Err(error);
    }
    Ok(())
}

/// Keep ownership until stdin is closed or the failed command has been reaped.
pub(crate) async fn write_stdin(
    mut child: Child,
    bytes: &[u8],
    deadline: Duration,
) -> io::Result<Child> {
    let group = owned_process_group(&child);
    let write = async {
        let mut input = child.stdin.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Missing Compose input pipe")
        })?;
        tokio::time::timeout(deadline, input.write_all(bytes))
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Timed out sending the Compose model",
                )
            })?
    };
    let result = tokio::select! {
        biased;
        _ = cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "Server drain cancelled preparation")),
        result = write => result,
    };
    if let Err(error) = result {
        stop_and_reap(&mut child, group).await?;
        return Err(error);
    }
    Ok(child)
}

/// Run preparation under a shared drain gate. Command capture kills and reaps
/// its child before returning cancellation; the enclosing future keeps ownership.
pub async fn with_cancellation<F: std::future::Future>(
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    operation: F,
) -> F::Output {
    DRAIN_REQUESTED.scope(flag, operation).await
}

const TAIL_BYTES: usize = 64 * 1024;
const MAX_STDOUT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) enum StdoutMode {
    Complete,
    Tail,
}

pub(crate) struct CapturedOutput {
    pub(crate) status: Option<ExitStatus>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

async fn drain(
    mut pipe: impl AsyncRead + Unpin,
    output: &mut Vec<u8>,
    mode: StdoutMode,
) -> io::Result<()> {
    // Keep nested deployment futures small enough for Tokio worker stacks.
    let mut buffer = vec![0; 8192];
    loop {
        let count = pipe.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        match mode {
            StdoutMode::Complete if output.len() + count > MAX_STDOUT_BYTES => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Compose command stdout exceeded {MAX_STDOUT_BYTES} bytes"),
                ));
            }
            StdoutMode::Complete => (),
            StdoutMode::Tail => {
                let excess = (output.len() + count).saturating_sub(TAIL_BYTES);
                output.drain(..excess);
            }
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

/// A missing status means the deadline elapsed. Kill and reap the child on any
/// timeout or pipe/wait error; cancellation is covered by the caller's kill_on_drop.
pub(crate) async fn capture(
    mut child: Child,
    deadline: Option<Duration>,
    stdout_mode: StdoutMode,
) -> io::Result<CapturedOutput> {
    let group = owned_process_group(&child);
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let mut output = CapturedOutput {
        status: None,
        stdout: Vec::with_capacity(TAIL_BYTES),
        stderr: Vec::with_capacity(TAIL_BYTES),
    };
    let collect = async {
        let (_, _, status) = tokio::try_join!(
            drain(stdout, &mut output.stdout, stdout_mode),
            drain(stderr, &mut output.stderr, StdoutMode::Tail),
            child.wait(),
        )?;
        Ok::<_, io::Error>(status)
    };
    let capture = async {
        if let Some(deadline) = deadline {
            tokio::time::timeout(deadline, collect).await.ok()
        } else {
            Some(collect.await)
        }
    };
    let result = tokio::select! {
        biased;
        _ = cancelled() => Some(Err(io::Error::new(io::ErrorKind::Interrupted, "Server drain cancelled preparation"))),
        result = capture => result,
    };
    match result {
        Some(Ok(status)) => output.status = Some(status),
        result => {
            stop_and_reap(&mut child, group).await?;
            if let Some(Err(error)) = result {
                return Err(error);
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[tokio::test]
    async fn preparation_cancellation_reaps_compose_command_and_preserves_scope() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let flag = Arc::new(AtomicBool::new(false));
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "exec sleep 30"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let task_flag = flag.clone();
        let task = tokio::spawn(async move {
            with_cancellation(
                task_flag,
                capture(child, Some(Duration::from_secs(1800)), StdoutMode::Tail),
            )
            .await
        });
        flag.store(true, Ordering::Release);
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Interrupted);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        assert!(check_cancellation().is_ok());
    }

    #[tokio::test]
    async fn structured_stdout_survives_large_output_on_both_pipes() {
        let dir = tempfile::tempdir().unwrap();
        let stdout = serde_json::to_vec(&serde_json::json!({
            "services": {"web": {"environment": {"VALUE": "x".repeat(96 * 1024)}}}
        }))
        .unwrap();
        let stderr = [vec![b'e'; 96 * 1024], b"diagnostic-end\n".to_vec()].concat();
        std::fs::write(dir.path().join("stdout"), &stdout).unwrap();
        std::fs::write(dir.path().join("stderr"), &stderr).unwrap();
        for mode in [StdoutMode::Complete, StdoutMode::Tail] {
            let child = tokio::process::Command::new("sh")
                .args(["-c", "cat stdout & cat stderr >&2 & wait"])
                .current_dir(dir.path())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let output = capture(child, Some(Duration::from_secs(5)), mode)
                .await
                .unwrap();
            assert!(output.status.unwrap().success());
            assert_eq!(output.stderr, stderr[stderr.len() - TAIL_BYTES..]);
            match mode {
                StdoutMode::Complete => {
                    assert_eq!(output.stdout, stdout);
                    assert!(serde_json::from_slice::<serde_json::Value>(&output.stdout).is_ok());
                }
                StdoutMode::Tail => {
                    assert_eq!(output.stdout, stdout[stdout.len() - TAIL_BYTES..]);
                }
            }
        }
    }

    #[tokio::test]
    async fn oversized_structured_stdout_fails_explicitly() {
        let input = vec![b'x'; MAX_STDOUT_BYTES + 1];
        let mut output = Vec::new();
        let error = drain(input.as_slice(), &mut output, StdoutMode::Complete)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("stdout exceeded"));
        assert!(output.len() <= MAX_STDOUT_BYTES);
    }
}

#[cfg(test)]
mod stdin_tests;

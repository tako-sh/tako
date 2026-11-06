//! Drain subprocess pipes concurrently while retaining only bounded diagnostic tails.

use std::io;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
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

pub(crate) async fn during_preparation<F: std::future::Future>(
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    operation: F,
) -> F::Output {
    tako_compose::with_command_cancellation(flag.clone(), DRAIN_REQUESTED.scope(flag, operation))
        .await
}

pub(crate) const PREPARATION_TIMEOUT: Duration = Duration::from_secs(600);

const TAIL_BYTES: usize = 64 * 1024;

pub(crate) struct CapturedOutput {
    pub(crate) status: Option<ExitStatus>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

async fn drain(mut pipe: impl AsyncRead + Unpin, tail: &mut Vec<u8>) -> io::Result<()> {
    let mut buffer = [0; 8192];
    loop {
        let count = pipe.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        let excess = (tail.len() + count).saturating_sub(TAIL_BYTES);
        tail.drain(..excess);
        tail.extend_from_slice(&buffer[..count]);
    }
}

/// A missing status means the deadline elapsed. Kill and reap the child on any
/// timeout or pipe/wait error; cancellation is covered by the caller's kill_on_drop.
pub(crate) async fn capture(
    mut child: Child,
    deadline: Option<Duration>,
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
            drain(stdout, &mut output.stdout),
            drain(stderr, &mut output.stderr),
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
mod tests;

//! Drain subprocess pipes concurrently while retaining only bounded diagnostic tails.

use std::io;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;

const TAIL_BYTES: usize = 64 * 1024;

pub(crate) struct CapturedOutput {
    pub(crate) status: Option<ExitStatus>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

async fn drain(mut pipe: impl AsyncRead + Unpin, tail: &mut Vec<u8>) -> io::Result<()> {
    // Keep nested deployment futures small enough for Tokio worker stacks.
    let mut buffer = vec![0; 8192];
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
    let result = if let Some(deadline) = deadline {
        tokio::time::timeout(deadline, collect).await.ok()
    } else {
        Some(collect.await)
    };
    match result {
        Some(Ok(status)) => output.status = Some(status),
        result => {
            let _ = child.kill().await;
            if let Some(Err(error)) = result {
                return Err(error);
            }
        }
    }
    Ok(output)
}

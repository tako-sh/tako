//! Preserve structured stdout and bounded diagnostic tails without blocking pipes.

use std::io;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

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

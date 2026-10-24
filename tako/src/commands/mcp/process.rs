use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

pub(super) const OUTPUT_LIMIT: usize = 1024 * 1024;

struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if self.0 != 0 {
            // CLI subprocesses and their build children share a separate group.
            // This also runs if the tool future is dropped on MCP cancellation.
            unsafe {
                libc::kill(-(self.0 as i32), libc::SIGKILL);
            }
        }
    }
}

async fn read_output(mut reader: impl AsyncRead + Unpin) -> std::io::Result<(String, bool)> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let remaining = OUTPUT_LIMIT.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..count.min(remaining)]);
        truncated |= count > remaining;
    }
    Ok((String::from_utf8_lossy(&output).into_owned(), truncated))
}

pub(super) async fn execute(
    executable: &Path,
    project: &Path,
    config: &Path,
    args: &[String],
    stdin: Option<&str>,
    timeout: Duration,
) -> Result<Value, String> {
    let mut command = Command::new(executable);
    command
        .args(["--ci", "--json", "--config"])
        .arg(config)
        .args(args)
        .current_dir(project)
        .env("TAKO_TELEMETRY", "0")
        .env("NO_COLOR", "1")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let mut group = ProcessGroup(child.id().unwrap());
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let input = child.stdin.take();
    let write_input = async {
        if let Some(mut input) = input {
            input
                .write_all(stdin.unwrap_or_default().as_bytes())
                .await?;
            input.shutdown().await?;
        }
        Ok::<_, std::io::Error>(())
    };
    let work = async {
        let (status, stdout, stderr, written) = tokio::join!(
            child.wait(),
            read_output(stdout),
            read_output(stderr),
            write_input
        );
        written.map_err(|error| error.to_string())?;
        Ok::<_, String>((
            status.map_err(|error| error.to_string())?,
            stdout.map_err(|error| error.to_string())?,
            stderr.map_err(|error| error.to_string())?,
        ))
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(Ok((status, (stdout, out_truncated), (stderr, err_truncated)))) => {
            group.0 = 0;
            let stdout = serde_json::from_str::<Value>(&stdout).unwrap_or(Value::String(stdout));
            Ok(
                json!({"ok":status.success(),"exit_code":status.code(),"stdout":stdout,"stderr":stderr,"truncated":out_truncated || err_truncated}),
            )
        }
        Ok(Err(error)) => Err(error),
        Err(_) => {
            drop(group);
            let _ = child.kill().await;
            Err(format!(
                "Tako command timed out after {} seconds. Local command terminated; remote work may continue. Inspect status before retrying",
                timeout.as_secs()
            ))
        }
    }
}

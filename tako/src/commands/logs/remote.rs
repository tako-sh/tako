use std::process::Stdio;
use std::sync::{Arc, Mutex};

use crate::shell::shell_single_quote;
use crate::ssh::SshClient;

pub(super) type SharedLogSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

pub(super) async fn stream_remote_logs(
    host: &str,
    port: u16,
    log_cmd: &str,
    sink: SharedLogSink,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match stream_remote_logs_russh(host, port, log_cmd, sink.clone()).await {
        Ok(()) => Ok(()),
        Err(primary) => match stream_remote_logs_openssh(host, port, log_cmd, sink).await {
            Ok(()) => Ok(()),
            Err(fallback) => Err(format!(
                "SSH log stream failed for {host}:{port}: {primary}; OpenSSH fallback failed: {fallback}"
            )
            .into()),
        },
    }
}

async fn stream_remote_logs_russh(
    host: &str,
    port: u16,
    log_cmd: &str,
    sink: SharedLogSink,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut ssh = SshClient::connect_to(host, port).await?;
    let out = sink.clone();
    let err = sink;
    let exit = ssh
        .exec_streaming(
            log_cmd,
            move |data| {
                out(data);
            },
            move |data| {
                err(data);
            },
        )
        .await?;
    ssh.disconnect().await?;

    if exit != 0 {
        return Err(format!("remote log command exited {exit}").into());
    }

    Ok(())
}

async fn stream_remote_logs_openssh(
    host: &str,
    port: u16,
    log_cmd: &str,
    sink: SharedLogSink,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut child = tokio::process::Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-p")
        .arg(port.to_string())
        .arg(format!("tako@{host}"))
        .arg(log_cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_task = stdout.map(|stream| {
        let sink = sink.clone();
        tokio::spawn(async move { stream_to_sink(stream, sink).await })
    });
    let stderr_task = stderr.map(|stream| {
        let sink = sink.clone();
        tokio::spawn(async move { stream_to_sink(stream, sink).await })
    });

    let status = child.wait().await?;

    if let Some(task) = stdout_task {
        task.await??;
    }
    if let Some(task) = stderr_task {
        task.await??;
    }

    if !status.success() {
        return Err(format!("exit {status}").into());
    }

    Ok(())
}

async fn stream_to_sink<R>(mut stream: R, sink: SharedLogSink) -> Result<(), std::io::Error>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut buf = [0_u8; 8192];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        sink(&buf[..n]);
    }
    Ok(())
}

pub(super) async fn collect_remote_log_bytes(
    host: &str,
    port: u16,
    log_cmd: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    match collect_remote_log_bytes_russh(host, port, log_cmd).await {
        Ok(bytes) => Ok(bytes),
        Err(primary) => match collect_remote_log_bytes_openssh(host, port, log_cmd).await {
            Ok(bytes) => Ok(bytes),
            Err(fallback) => Err(format!(
                "SSH log fetch failed for {host}:{port}: {primary}; OpenSSH fallback failed: {fallback}"
            )
            .into()),
        },
    }
}

async fn collect_remote_log_bytes_russh(
    host: &str,
    port: u16,
    log_cmd: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut ssh = SshClient::connect_to(host, port).await?;
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let out = bytes.clone();
    let err = Arc::new(Mutex::new(Vec::new()));
    let err_out = err.clone();

    let exit = ssh
        .exec_streaming(
            log_cmd,
            move |data| {
                if let Ok(mut buf) = out.lock() {
                    buf.extend_from_slice(data);
                }
            },
            move |data| {
                if let Ok(mut buf) = err_out.lock() {
                    buf.extend_from_slice(data);
                }
            },
        )
        .await?;
    ssh.disconnect().await?;

    if exit != 0 {
        let stderr = err.lock().unwrap_or_else(|e| e.into_inner());
        let detail = String::from_utf8_lossy(&stderr).trim().to_string();
        let message = if detail.is_empty() {
            format!("remote log command exited {exit}")
        } else {
            format!("remote log command exited {exit}: {detail}")
        };
        return Err(message.into());
    }

    let bytes = Arc::try_unwrap(bytes)
        .map_err(|_| "log byte collector still has references")?
        .into_inner()
        .map_err(|_| "log byte collector lock poisoned")?;
    Ok(bytes)
}

async fn collect_remote_log_bytes_openssh(
    host: &str,
    port: u16,
    log_cmd: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let output = tokio::process::Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-p")
        .arg(port.to_string())
        .arg(format!("tako@{host}"))
        .arg(log_cmd)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("exit {}", output.status)
        } else {
            stderr
        };
        return Err(detail.into());
    }

    Ok(output.stdout)
}

pub(super) fn build_fetch_log_command(
    app_name: &str,
    route_filters: &[String],
    days: u32,
) -> String {
    let log_dir = shell_single_quote(&format!("/opt/tako/apps/{app_name}/logs"));
    let app_logs = build_app_log_command(&log_dir, days);
    let journal = build_journal_command(
        &format!(
            "--since {}",
            shell_single_quote(&format!("{days} days ago"))
        ),
        app_name,
        route_filters,
        false,
    );
    format!(
        "{{ {app_logs}; {journal}; }} | if command -v zstd >/dev/null 2>&1; then zstd -c; else cat; fi"
    )
}

fn build_app_log_command(log_dir: &str, days: u32) -> String {
    let since = shell_single_quote(&format!("{days} days ago"));
    let awk = shell_single_quote(
        r#"substr($0,5,1)!="-" || substr($0,11,1)!="T" || substr($0,1,19) >= cutoff"#,
    );
    format!(
        "if cutoff=$(date -u -d {since} '+%Y-%m-%dT%H:%M:%S' 2>/dev/null); then for log_file in {log_dir}/previous.log {log_dir}/current.log; do test -f \"$log_file\" && awk -v cutoff=\"$cutoff\" {awk} \"$log_file\"; done; else for log_file in {log_dir}/previous.log {log_dir}/current.log; do test -f \"$log_file\" && cat \"$log_file\"; done; fi"
    )
}

pub(super) fn build_tail_log_command(app_name: &str, route_filters: &[String]) -> String {
    let log_file = shell_single_quote(&format!("/opt/tako/apps/{app_name}/logs/current.log"));
    let journal = build_journal_command("-f", app_name, route_filters, true);
    format!("{{ tail -F {log_file} 2>/dev/null & {journal} & wait; }} || echo 'No logs available'")
}

fn build_journal_command(
    time_args: &str,
    app_name: &str,
    route_filters: &[String],
    line_buffered: bool,
) -> String {
    let grep = build_journal_filter(app_name, route_filters, line_buffered);
    format!(
        "(sudo -n journalctl -u tako-server {time_args} --no-pager -o cat 2>/dev/null || journalctl -u tako-server {time_args} --no-pager -o cat 2>/dev/null) | {grep}"
    )
}

fn build_journal_filter(app_name: &str, route_filters: &[String], line_buffered: bool) -> String {
    let mut patterns = vec![format!("\"app\":\"{app_name}\"")];
    patterns.extend(route_host_log_patterns(route_filters));
    patterns.sort();
    patterns.dedup();

    let mut parts = vec!["grep".to_string()];
    if line_buffered {
        parts.push("--line-buffered".to_string());
    }
    parts.push("-F".to_string());
    for pattern in patterns {
        parts.push("-e".to_string());
        parts.push(shell_single_quote(&pattern));
    }
    parts.join(" ")
}

fn route_host_log_patterns(routes: &[String]) -> Vec<String> {
    routes
        .iter()
        .filter_map(|route| route.split('/').next())
        .filter(|host| !host.is_empty())
        .map(|host| {
            if let Some(suffix) = host.strip_prefix("*.") {
                format!(".{suffix}:")
            } else {
                format!("Host: {host}:")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn journal_filter_matches_app_and_route_hosts() {
        let filter = build_journal_filter(
            "demo/production",
            &[
                "demo.tako.sh".to_string(),
                "*.demo.tako.sh".to_string(),
                "demo.tako.sh/api/*".to_string(),
            ],
            false,
        );

        assert!(filter.contains("grep -F"));
        assert!(filter.contains("-e '\"app\":\"demo/production\"'"));
        assert!(filter.contains("-e 'Host: demo.tako.sh:'"));
        assert!(filter.contains("-e '.demo.tako.sh:'"));
    }

    #[test]
    fn fetch_log_command_includes_server_journal_route_diagnostics() {
        let command = build_fetch_log_command(
            "demo/production",
            &["demo.tako.sh".to_string(), "*.demo.tako.sh".to_string()],
            2,
        );

        assert!(command.contains("date -u -d '2 days ago' '+%Y-%m-%dT%H:%M:%S'"));
        assert!(command.contains("awk -v cutoff=\"$cutoff\""));
        assert!(command.contains("substr($0,1,19) >= cutoff"));
        assert!(command.contains("'/opt/tako/apps/demo/production/logs'/previous.log"));
        assert!(command.contains("journalctl -u tako-server --since '2 days ago'"));
        assert!(command.contains("-e '\"app\":\"demo/production\"'"));
        assert!(command.contains("-e 'Host: demo.tako.sh:'"));
        assert!(command.contains("-e '.demo.tako.sh:'"));
        assert!(command.contains("zstd -c"));
    }

    #[test]
    fn app_log_command_reads_current_when_previous_log_is_missing() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let log_dir =
            std::env::temp_dir().join(format!("tako-log-command-{}-{unique}", std::process::id()));
        fs::create_dir_all(&log_dir).unwrap();
        fs::write(log_dir.join("current.log"), "[out] [test] current only\n").unwrap();

        let command = build_app_log_command(&shell_single_quote(&log_dir.to_string_lossy()), 1);
        let output = Command::new("sh").arg("-c").arg(command).output().unwrap();
        let _ = fs::remove_dir_all(&log_dir);

        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("current only"),
            "stdout should include current.log: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn tail_log_command_streams_app_and_journal_logs() {
        let command = build_tail_log_command(
            "demo/production",
            &["demo.tako.sh".to_string(), "*.demo.tako.sh".to_string()],
        );

        assert!(command.contains("tail -F '/opt/tako/apps/demo/production/logs/current.log'"));
        assert!(command.contains("journalctl -u tako-server -f --no-pager -o cat"));
        assert!(command.contains("grep --line-buffered -F"));
        assert!(command.contains("-e 'Host: demo.tako.sh:'"));
        assert!(command.contains("& wait"));
    }
}

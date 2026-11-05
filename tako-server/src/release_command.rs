//! One-shot release command runner.
//!
//! Used by the deploy flow to run migrations / cache prep / etc. against
//! the new release directory before any rolling update starts. Mirrors
//! `tako-server::release::prepare_release_runtime` style: spawn
//! `sh -c "<command>"` with merged env in `cwd = release_dir`.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
use tako_spawn::ProcessIsolation;
use tokio::process::Command as TokioCommand;

/// Hard cap on a single release-command invocation. The deploy flow
/// fails when this fires.
pub const RELEASE_COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug)]
pub struct ReleaseCommandOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl ReleaseCommandOutcome {
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

pub async fn run(
    command_line: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    #[cfg(unix)] isolation: Option<ProcessIsolation>,
) -> Result<ReleaseCommandOutcome, String> {
    run_with_timeout(
        command_line,
        cwd,
        env,
        RELEASE_COMMAND_TIMEOUT,
        #[cfg(unix)]
        isolation,
    )
    .await
}

async fn run_with_timeout(
    command_line: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    timeout_duration: Duration,
    #[cfg(unix)] isolation: Option<ProcessIsolation>,
) -> Result<ReleaseCommandOutcome, String> {
    crate::process_output::check_cancellation().map_err(|e| e.to_string())?;
    let mut cmd = TokioCommand::new("sh");
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.args(["-c", command_line])
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    if let Some(isolation) = isolation {
        unsafe {
            cmd.pre_exec(move || tako_spawn::install_process_isolation(&isolation));
        }
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn release command: {e}"))?;

    let output = crate::process_output::capture(child, Some(timeout_duration))
        .await
        .map_err(|e| format!("Failed to collect release command output: {e}"))?;
    Ok(ReleaseCommandOutcome {
        exit_code: output.status.and_then(|status| status.code()),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        timed_out: output.status.is_none(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[tokio::test]
    async fn noisy_binary_output_keeps_bounded_tails_and_status() {
        let dir = TempDir::new().unwrap();
        let outcome = run("i=0; while [ $i -lt 20000 ]; do printf 'abcdefghij'; printf '0123456789' >&2; i=$((i+1)); done; printf '\\377stdout-tail'; printf '\\377stderr-tail' >&2; exit 7", dir.path(), &empty_env(), None).await.unwrap();
        assert_eq!(outcome.exit_code, Some(7));
        assert!(outcome.stdout.len() <= 65538);
        assert!(outcome.stderr.len() <= 65538);
        assert!(outcome.stdout.ends_with("stdout-tail"));
        assert!(outcome.stderr.ends_with("stderr-tail"));
    }

    fn empty_env() -> HashMap<String, String> {
        let mut env = HashMap::new();
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }
        env
    }

    #[tokio::test]
    async fn runs_successful_command() {
        let dir = TempDir::new().unwrap();
        let outcome = run("echo hello", dir.path(), &empty_env(), None)
            .await
            .unwrap();
        assert!(outcome.succeeded());
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.stdout.contains("hello"));
        assert!(!outcome.timed_out);
    }

    #[tokio::test]
    async fn captures_nonzero_exit() {
        let dir = TempDir::new().unwrap();
        let outcome = run("exit 7", dir.path(), &empty_env(), None).await.unwrap();
        assert!(!outcome.succeeded());
        assert_eq!(outcome.exit_code, Some(7));
    }

    #[tokio::test]
    async fn forwards_env_vars() {
        let dir = TempDir::new().unwrap();
        let mut env = empty_env();
        env.insert("FOO".to_string(), "bar-value".to_string());
        let outcome = run("printf %s \"$FOO\"", dir.path(), &env, None)
            .await
            .unwrap();
        assert!(outcome.succeeded());
        assert_eq!(outcome.stdout, "bar-value");
    }

    #[tokio::test]
    async fn runs_in_provided_cwd() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("marker.txt");
        std::fs::write(&marker, "hi").unwrap();
        let outcome = run("ls marker.txt", dir.path(), &empty_env(), None)
            .await
            .unwrap();
        assert!(outcome.succeeded());
        assert!(outcome.stdout.contains("marker.txt"));
    }

    #[tokio::test]
    async fn captures_stderr() {
        let dir = TempDir::new().unwrap();
        let outcome = run("echo oops 1>&2; exit 1", dir.path(), &empty_env(), None)
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, Some(1));
        assert!(outcome.stderr.contains("oops"));
    }

    #[tokio::test]
    async fn timeout_stops_release_command_process() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("marker.txt");
        let command = format!("sleep 0.2; touch {}", marker.display());
        let outcome = run_with_timeout(
            &command,
            dir.path(),
            &empty_env(),
            Duration::from_millis(25),
            None,
        )
        .await
        .unwrap();

        assert!(outcome.timed_out);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn does_not_inherit_parent_env() {
        unsafe { std::env::set_var("RELEASE_TEST_LEAK", "should-not-appear") };
        let dir = TempDir::new().unwrap();
        let outcome = run(
            "printf %s \"${RELEASE_TEST_LEAK:-EMPTY}\"",
            dir.path(),
            &empty_env(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.stdout, "EMPTY");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn applies_release_command_umask() {
        let dir = TempDir::new().unwrap();
        let isolation = ProcessIsolation {
            resource_limits: tako_spawn::ResourceLimits {
                open_files: None,
                processes: None,
                address_space_bytes: None,
            },
            umask: Some(0o027),
            ..Default::default()
        };
        let outcome = run("umask", dir.path(), &empty_env(), Some(isolation))
            .await
            .unwrap();
        assert!(outcome.succeeded());
        assert_eq!(outcome.stdout.trim(), "0027");
    }
}

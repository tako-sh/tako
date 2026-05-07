//! One-shot release command runner.
//!
//! Used by the deploy flow to run migrations / cache prep / etc. against
//! the new release directory before any rolling update starts. Mirrors
//! `tako-server::release::prepare_release_runtime` style: spawn
//! `sh -c "<command>"` with merged env in `cwd = release_dir`.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;
use tokio::time::timeout;

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
) -> Result<ReleaseCommandOutcome, String> {
    run_with_timeout(command_line, cwd, env, RELEASE_COMMAND_TIMEOUT).await
}

async fn run_with_timeout(
    command_line: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    timeout_duration: Duration,
) -> Result<ReleaseCommandOutcome, String> {
    let mut cmd = TokioCommand::new("sh");
    cmd.args(["-c", command_line])
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn release command: {e}"))?;

    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let mut stdout = String::new();
    let mut stderr = String::new();

    let read_stdout = stdout_pipe.read_to_string(&mut stdout);
    let read_stderr = stderr_pipe.read_to_string(&mut stderr);
    let wait = child.wait();

    let combined = async move {
        let (_, _, status) = tokio::join!(read_stdout, read_stderr, wait);
        status.map_err(|e| format!("Failed to wait on release command: {e}"))
    };

    match timeout(timeout_duration, combined).await {
        Ok(Ok(status)) => Ok(ReleaseCommandOutcome {
            exit_code: status.code(),
            stdout,
            stderr,
            timed_out: false,
        }),
        Ok(Err(msg)) => Err(msg),
        Err(_elapsed) => Ok(ReleaseCommandOutcome {
            exit_code: None,
            stdout,
            stderr,
            timed_out: true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

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
        let outcome = run("echo hello", dir.path(), &empty_env()).await.unwrap();
        assert!(outcome.succeeded());
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.stdout.contains("hello"));
        assert!(!outcome.timed_out);
    }

    #[tokio::test]
    async fn captures_nonzero_exit() {
        let dir = TempDir::new().unwrap();
        let outcome = run("exit 7", dir.path(), &empty_env()).await.unwrap();
        assert!(!outcome.succeeded());
        assert_eq!(outcome.exit_code, Some(7));
    }

    #[tokio::test]
    async fn forwards_env_vars() {
        let dir = TempDir::new().unwrap();
        let mut env = empty_env();
        env.insert("FOO".to_string(), "bar-value".to_string());
        let outcome = run("printf %s \"$FOO\"", dir.path(), &env).await.unwrap();
        assert!(outcome.succeeded());
        assert_eq!(outcome.stdout, "bar-value");
    }

    #[tokio::test]
    async fn runs_in_provided_cwd() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("marker.txt");
        std::fs::write(&marker, "hi").unwrap();
        let outcome = run("ls marker.txt", dir.path(), &empty_env())
            .await
            .unwrap();
        assert!(outcome.succeeded());
        assert!(outcome.stdout.contains("marker.txt"));
    }

    #[tokio::test]
    async fn captures_stderr() {
        let dir = TempDir::new().unwrap();
        let outcome = run("echo oops 1>&2; exit 1", dir.path(), &empty_env())
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
        )
        .await
        .unwrap();
        assert_eq!(outcome.stdout, "EMPTY");
    }
}

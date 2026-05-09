use super::*;
use std::time::Duration;
use tempfile::tempdir;

fn sleep_spec(cwd: PathBuf, workers: u32, sleep_secs: &str) -> WorkerSpec {
    WorkerSpec {
        app: "test".into(),
        workers,
        concurrency: 1,
        idle_timeout_ms: 0,
        command: vec!["sleep".into(), sleep_secs.into()],
        cwd,
        env: HashMap::new(),
        secrets: HashMap::new(),
        log_sink: None,
    }
}

#[tokio::test]
async fn start_noop_when_workers_zero() {
    let dir = tempdir().unwrap();
    let sup = WorkerSupervisor::new(sleep_spec(dir.path().into(), 0, "10"));
    sup.start().await.unwrap();
    assert!(!sup.is_running());
}

#[tokio::test]
async fn start_spawns_n_workers_when_workers_positive() {
    let dir = tempdir().unwrap();
    let sup = WorkerSupervisor::new(sleep_spec(dir.path().into(), 2, "10"));
    sup.start().await.unwrap();
    assert!(sup.is_running());
    assert_eq!(sup.state.lock().children.len(), 2);
    sup.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn wake_spawns_one_on_scale_to_zero_when_none_running() {
    let dir = tempdir().unwrap();
    let sup = WorkerSupervisor::new(sleep_spec(dir.path().into(), 0, "10"));
    sup.wake().unwrap();
    assert!(sup.is_running());
    sup.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn wake_does_not_oversubscribe_when_already_running() {
    let dir = tempdir().unwrap();
    let sup = WorkerSupervisor::new(sleep_spec(dir.path().into(), 0, "10"));
    sup.wake().unwrap();
    sup.wake().unwrap();
    sup.wake().unwrap();
    assert_eq!(sup.state.lock().children.len(), 1);
    sup.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn shutdown_sigterms_children_and_waits() {
    let dir = tempdir().unwrap();
    // Use a short sleep so the child exits promptly on SIGTERM (default
    // disposition for `sleep` is to exit on SIGTERM).
    let sup = WorkerSupervisor::new(sleep_spec(dir.path().into(), 1, "60"));
    sup.start().await.unwrap();
    assert!(sup.is_running());
    sup.shutdown(Duration::from_secs(2)).await;
    assert!(!sup.is_running());
}

#[tokio::test]
async fn shutdown_reaps_children_that_ignore_sigterm() {
    let dir = tempdir().unwrap();
    let spec = WorkerSpec {
        app: "test".into(),
        workers: 1,
        concurrency: 1,
        idle_timeout_ms: 0,
        command: vec!["sh".into(), "-c".into(), "trap '' TERM; sleep 60".into()],
        cwd: dir.path().into(),
        env: HashMap::new(),
        secrets: HashMap::new(),
        log_sink: None,
    };
    let sup = WorkerSupervisor::new(spec);
    sup.start().await.unwrap();
    assert!(sup.is_running());
    sup.shutdown(Duration::from_millis(50)).await;
    assert_eq!(sup.state.lock().children.len(), 0);
}

#[tokio::test]
async fn wake_respawns_missing_always_on_worker() {
    let dir = tempdir().unwrap();
    // Start with 1 always-on worker that sleeps briefly then exits.
    let sup = WorkerSupervisor::new(sleep_spec(dir.path().into(), 1, "0.05"));
    sup.start().await.unwrap();
    // Give it time to exit on its own.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!sup.is_running());
    sup.wake().unwrap();
    assert!(sup.is_running());
    sup.shutdown(Duration::from_secs(1)).await;
}

fn failing_spec(cwd: PathBuf) -> WorkerSpec {
    // `false` exits immediately with status 1. Simulates a worker whose
    // bootstrap throws (bad code, missing entrypoint, etc.) — exits
    // non-zero without claiming any runs.
    WorkerSpec {
        app: "test".into(),
        workers: 0,
        concurrency: 1,
        idle_timeout_ms: 0,
        command: vec!["false".into()],
        cwd,
        env: HashMap::new(),
        secrets: HashMap::new(),
        log_sink: None,
    }
}

#[tokio::test]
async fn health_check_ok_before_any_spawn() {
    let dir = tempdir().unwrap();
    let sup = WorkerSupervisor::new(sleep_spec(dir.path().into(), 0, "10"));
    assert!(sup.check_startup_health().is_ok());
}

#[tokio::test]
async fn health_check_fails_after_worker_exits_without_claiming() {
    let dir = tempdir().unwrap();
    let sup = WorkerSupervisor::new(failing_spec(dir.path().into()));
    sup.wake().unwrap();
    // Let the child exit.
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Re-poll: this call processes exits and flips the health flag.
    let err = sup.check_startup_health().expect_err("should be unhealthy");
    assert!(
        err.contains("worker exited"),
        "error should describe cold exit, got: {err}"
    );
}

#[tokio::test]
async fn notify_claimed_clears_unhealthy_state() {
    let dir = tempdir().unwrap();
    let sup = WorkerSupervisor::new(failing_spec(dir.path().into()));
    sup.wake().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    sup.check_startup_health().unwrap_err();
    sup.notify_claimed();
    assert!(sup.check_startup_health().is_ok());
}

#[tokio::test]
async fn wake_returns_error_while_in_unhealthy_cooldown() {
    let dir = tempdir().unwrap();
    let sup = WorkerSupervisor::new(failing_spec(dir.path().into()));
    sup.wake().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // First wake after cold-exit observation must refuse to respawn.
    sup.check_startup_health().unwrap_err();
    let err = sup.wake().expect_err("wake during cooldown should error");
    assert!(matches!(err, SupervisorError::Unhealthy(_)));
}

#[tokio::test]
async fn clean_idle_exit_does_not_mark_unhealthy() {
    // `true` exits 0 immediately — simulates a clean idle-out.
    let dir = tempdir().unwrap();
    let spec = WorkerSpec {
        app: "test".into(),
        workers: 0,
        concurrency: 1,
        idle_timeout_ms: 0,
        command: vec!["true".into()],
        cwd: dir.path().into(),
        env: HashMap::new(),
        secrets: HashMap::new(),
        log_sink: None,
    };
    let sup = WorkerSupervisor::new(spec);
    sup.wake().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(sup.check_startup_health().is_ok());
}

#[tokio::test]
async fn background_reaper_collects_clean_idle_exit_without_poll() {
    let dir = tempdir().unwrap();
    let spec = WorkerSpec {
        app: "test".into(),
        workers: 0,
        concurrency: 1,
        idle_timeout_ms: 0,
        command: vec!["true".into()],
        cwd: dir.path().into(),
        env: HashMap::new(),
        secrets: HashMap::new(),
        log_sink: None,
    };
    let sup = WorkerSupervisor::new(spec);
    sup.wake().unwrap();
    for _ in 0..20 {
        if sup.state.lock().children.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("background reaper did not collect exited worker");
}

#[cfg(unix)]
#[test]
fn bootstrap_pipe_envelope_has_token_and_secrets() {
    use std::io::Read;
    use std::os::fd::{FromRawFd, IntoRawFd};

    let secrets = HashMap::from([
        ("DATABASE_URL".to_string(), "postgres://x".to_string()),
        ("API_KEY".to_string(), "sk-123".to_string()),
    ]);
    let token = "worker-token-abc";

    let (read_end, writer) = create_bootstrap_pipe(token, &secrets).expect("create pipe");

    let mut buf = String::new();
    let fd = read_end.into_raw_fd();
    // SAFETY: fd was just handed over by into_raw_fd; File::from_raw_fd owns it.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.read_to_string(&mut buf).expect("read pipe");
    writer.join().expect("writer thread").expect("write ok");

    let parsed: serde_json::Value = serde_json::from_str(&buf).expect("valid JSON");
    assert_eq!(parsed["token"].as_str(), Some(token));
    assert_eq!(
        parsed["secrets"]["DATABASE_URL"].as_str(),
        Some("postgres://x")
    );
    assert_eq!(parsed["secrets"]["API_KEY"].as_str(), Some("sk-123"));
}

#[cfg(unix)]
#[test]
fn bootstrap_pipe_is_always_created_even_with_empty_secrets() {
    use std::io::Read;
    use std::os::fd::{FromRawFd, IntoRawFd};

    let secrets: HashMap<String, String> = HashMap::new();
    let token = "still-has-a-token";

    let (read_end, writer) = create_bootstrap_pipe(token, &secrets).expect("create pipe");

    let mut buf = String::new();
    let fd = read_end.into_raw_fd();
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.read_to_string(&mut buf).expect("read pipe");
    writer.join().expect("writer thread").expect("write ok");

    let parsed: serde_json::Value = serde_json::from_str(&buf).expect("valid JSON");
    assert_eq!(parsed["token"].as_str(), Some(token));
    assert!(parsed["secrets"].is_object());
    assert_eq!(parsed["secrets"].as_object().unwrap().len(), 0);
}

#[tokio::test]
async fn effective_env_sets_concurrency_and_idle_timeout() {
    let spec = WorkerSpec {
        app: "a".into(),
        workers: 1,
        concurrency: 7,
        idle_timeout_ms: 12_000,
        command: vec!["sleep".into(), "0".into()],
        cwd: ".".into(),
        env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
        secrets: HashMap::new(),
        log_sink: None,
    };
    let env = spec.effective_env();
    assert_eq!(
        env.get("TAKO_WORKER_CONCURRENCY").map(String::as_str),
        Some("7")
    );
    assert_eq!(
        env.get("TAKO_WORKER_IDLE_TIMEOUT_MS").map(String::as_str),
        Some("12000")
    );
    assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
}

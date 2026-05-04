//! Worker process supervisor.
//!
//! One `WorkerSupervisor` per deployed app. Lifecycle:
//!
//! - `workers >= 1` (always-on): spawn N workers on `start`, respawn any that
//!   exit unexpectedly.
//! - `workers == 0` (scale-to-zero): no workers until `wake()` is called
//!   (from enqueue or cron tick). `wake()` spawns one worker if none is
//!   running. When the worker idles out and exits, we don't respawn —
//!   the next `wake()` starts a fresh one.
//!
//! `shutdown(timeout)` SIGTERMs all workers, waits, and SIGKILLs anything
//! still alive after the timeout. Used by the drain path.

use std::collections::HashMap;
use std::ffi::OsString;
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// After a worker crashes (non-zero exit before claiming any runs), refuse
/// to respawn or accept enqueues until this window elapses. Gives the user
/// a clear error at the next enqueue instead of a silent crash loop.
const UNHEALTHY_COOLDOWN: Duration = Duration::from_secs(5);

/// Callback invoked once per line of worker stdout/stderr when
/// [`WorkerSpec::log_sink`] is set. `is_stderr` is `true` for stderr.
pub type WorkerLogSink = Arc<dyn Fn(&str, bool) + Send + Sync>;

/// Static configuration for a single app's workers.
#[derive(Clone)]
pub struct WorkerSpec {
    /// Human-readable app identifier (for logs).
    pub app: String,
    /// Number of always-on workers. `0` = scale-to-zero.
    pub workers: u32,
    /// Per-worker concurrency (passed as env var).
    pub concurrency: u32,
    /// Idle-exit timeout for scale-to-zero workers (ms). `0` = never exit.
    pub idle_timeout_ms: u64,
    /// Program + args. E.g. `["bun", "/path/to/tako-worker.mjs"]`.
    pub command: Vec<OsString>,
    /// Working directory for the worker process.
    pub cwd: PathBuf,
    /// Extra env vars (merged on top of `build_base_env`).
    pub env: HashMap<String, String>,
    /// Secrets to hand the worker via fd 3. Mirror of the HTTP
    /// instance's runtime ABI — the SDK reads JSON from fd 3 at startup
    /// and populates the `secrets` export on `tako.gen.ts`.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub secrets: HashMap<String, String>,
    /// Optional per-line log sink. When `Some`, the supervisor pipes
    /// stdout/stderr and forwards each line. When `None`, inherits the
    /// parent's stdio (production default — lets journald/systemd capture
    /// it).
    pub log_sink: Option<WorkerLogSink>,
}

impl WorkerSpec {
    /// Env vars this supervisor always sets for workers, independent of
    /// the caller-supplied `env`. Caller's `env` is layered on top.
    fn effective_env(&self) -> HashMap<String, String> {
        let mut env: HashMap<String, String> = self.env.clone();
        env.insert(
            "TAKO_WORKER_CONCURRENCY".into(),
            self.concurrency.to_string(),
        );
        env.insert(
            "TAKO_WORKER_IDLE_TIMEOUT_MS".into(),
            self.idle_timeout_ms.to_string(),
        );
        env
    }
}

#[derive(thiserror::Error, Debug)]
pub enum SupervisorError {
    #[error("worker spec has empty command")]
    EmptyCommand,
    #[error("spawn failed: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("worker unhealthy: {0}")]
    Unhealthy(String),
}

pub struct WorkerSupervisor {
    spec: WorkerSpec,
    state: Arc<Mutex<State>>,
}

struct ChildEntry {
    child: Child,
    spawned_at: Instant,
    /// Value of `health.runs_claimed_total` at spawn time. If the child
    /// exits and this counter hasn't advanced, the worker never managed
    /// to claim a single run — a strong signal its bootstrap is broken.
    claimed_snapshot: u64,
}

#[derive(Default)]
struct WorkerHealth {
    /// Monotonically-increasing count of `notify_claimed()` calls — bumped
    /// by the enqueue-socket handler whenever a worker successfully claims
    /// a run.
    runs_claimed_total: u64,
    /// When `Some(t)` and `now < t`, the supervisor refuses to spawn new
    /// workers and the enqueue RPC returns an error. Cleared on the next
    /// successful claim.
    unhealthy_until: Option<Instant>,
    last_error: Option<String>,
}

struct State {
    children: Vec<ChildEntry>,
    shutting_down: bool,
    health: WorkerHealth,
}

impl WorkerSupervisor {
    pub fn new(spec: WorkerSpec) -> Self {
        let state = Arc::new(Mutex::new(State {
            children: Vec::new(),
            shutting_down: false,
            health: WorkerHealth::default(),
        }));
        Self::spawn_reaper(Arc::downgrade(&state), spec.log_sink.clone());
        Self { spec, state }
    }

    /// Launch all always-on workers. No-op when `workers == 0`
    /// (scale-to-zero: `wake()` spawns on demand).
    pub async fn start(&self) -> Result<(), SupervisorError> {
        if self.spec.workers == 0 {
            return Ok(());
        }
        let mut state = self.state.lock();
        for _ in 0..self.spec.workers {
            self.spawn_one_locked(&mut state)?;
        }
        Ok(())
    }

    /// Called on enqueue/cron tick. For scale-to-zero (`workers == 0`),
    /// spawns a worker if none is running. For always-on, respawns any
    /// that died. Holds the state lock across the spawn calls so two
    /// concurrent wakes can't both see an empty slot and over-spawn.
    ///
    /// Returns `Unhealthy` during the cooldown window after a crash-loop
    /// detection — caller should surface this to the user instead of
    /// silently respawning.
    pub fn wake(&self) -> Result<(), SupervisorError> {
        let mut state = self.state.lock();
        if state.shutting_down {
            return Ok(());
        }
        Self::process_exits(&mut state, self.spec.log_sink.as_ref());
        if let Some(reason) = Self::unhealthy_reason(&state) {
            return Err(SupervisorError::Unhealthy(reason));
        }
        let target = if self.spec.workers == 0 {
            if state.children.is_empty() { 1 } else { 0 }
        } else {
            (self.spec.workers as usize).saturating_sub(state.children.len())
        };
        for _ in 0..target {
            if let Err(e) = self.spawn_one_locked(&mut state) {
                // Spawn itself failed (program-not-found, fork error, etc.).
                // Mark unhealthy so the next enqueue surfaces a clear error
                // instead of retrying the same broken command endlessly.
                let msg = format!("worker spawn failed: {e}");
                state.health.unhealthy_until = Some(Instant::now() + UNHEALTHY_COOLDOWN);
                state.health.last_error = Some(msg.clone());
                Self::emit_health_error(self.spec.log_sink.as_ref(), &msg);
                return Err(e);
            }
        }
        Ok(())
    }

    /// Returns true while at least one child is running.
    pub fn is_running(&self) -> bool {
        let mut state = self.state.lock();
        Self::process_exits(&mut state, self.spec.log_sink.as_ref());
        !state.children.is_empty()
    }

    /// Pre-enqueue probe. Returns `Err` with a user-facing message if the
    /// worker is in the post-crash cooldown window. Called by the internal
    /// socket's `EnqueueRun` handler before writing to the DB — lets the
    /// SDK workflow `.enqueue()` call reject loudly when the worker can't
    /// possibly process the job.
    pub fn check_startup_health(&self) -> Result<(), String> {
        let mut state = self.state.lock();
        Self::process_exits(&mut state, self.spec.log_sink.as_ref());
        match Self::unhealthy_reason(&state) {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// Record that a worker successfully claimed a run. Resets any
    /// crash-loop cooldown — a worker that claims work is by definition
    /// healthy enough to process the queue.
    pub fn notify_claimed(&self) {
        let mut state = self.state.lock();
        state.health.runs_claimed_total = state.health.runs_claimed_total.saturating_add(1);
        state.health.unhealthy_until = None;
        state.health.last_error = None;
    }

    /// Drain exited children and update health accordingly. Must be called
    /// with the state lock held. A child that exits non-zero without
    /// claiming any runs flips the supervisor into the unhealthy cooldown
    /// state; a clean exit (code 0) or an exit after at least one claim
    /// is treated as normal idle-out.
    fn process_exits(state: &mut State, log_sink: Option<&WorkerLogSink>) {
        let entries: Vec<ChildEntry> = state.children.drain(..).collect();
        let mut still_live = Vec::with_capacity(entries.len());
        let mut cold_crashes: Vec<(Option<i32>, Duration)> = Vec::new();
        for mut entry in entries {
            match entry.child.try_wait() {
                Ok(None) => still_live.push(entry),
                Ok(Some(status)) => {
                    let code = status.code();
                    let crashed = code != Some(0);
                    let claimed = state
                        .health
                        .runs_claimed_total
                        .saturating_sub(entry.claimed_snapshot)
                        > 0;
                    if crashed && !claimed && !state.shutting_down {
                        cold_crashes.push((code, entry.spawned_at.elapsed()));
                    }
                }
                Err(_) => {}
            }
        }
        state.children = still_live;
        for (code, ran_for) in cold_crashes {
            let code_str = code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let msg = format!(
                "worker exited with status {code_str} after {}ms without claiming any runs",
                ran_for.as_millis()
            );
            state.health.unhealthy_until = Some(Instant::now() + UNHEALTHY_COOLDOWN);
            state.health.last_error = Some(msg.clone());
            Self::emit_health_error(log_sink, &msg);
        }
    }

    fn unhealthy_reason(state: &State) -> Option<String> {
        let until = state.health.unhealthy_until?;
        if Instant::now() < until {
            Some(
                state
                    .health
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "worker unhealthy".to_string()),
            )
        } else {
            None
        }
    }

    fn emit_health_error(log_sink: Option<&WorkerLogSink>, msg: &str) {
        if let Some(sink) = log_sink {
            let payload = serde_json::json!({
                "ts": unix_millis_now(),
                "level": "error",
                "scope": "tako",
                "msg": msg,
            });
            (sink)(&payload.to_string(), true);
        }
        tracing::warn!("{msg}");
    }

    /// SIGTERM all children, wait for exit, SIGKILL after `drain_timeout`.
    pub async fn shutdown(&self, drain_timeout: Duration) {
        let mut children: Vec<ChildEntry> = {
            let mut state = self.state.lock();
            state.shutting_down = true;
            state.children.drain(..).collect()
        };

        for entry in &children {
            #[cfg(unix)]
            unsafe {
                if let Some(pid) = entry.child.id() {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
            #[cfg(not(unix))]
            let _ = entry;
        }

        let deadline = tokio::time::Instant::now() + drain_timeout;
        loop {
            children.retain_mut(|entry| matches!(entry.child.try_wait(), Ok(None)));
            if children.is_empty() {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        if !children.is_empty() {
            for entry in &mut children {
                let _ = entry.child.start_kill();
            }
            for entry in &mut children {
                let _ = entry.child.wait().await;
            }
        }
    }

    fn spawn_reaper(state: Weak<Mutex<State>>, log_sink: Option<WorkerLogSink>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(state) = state.upgrade() else {
                    break;
                };
                let mut state = state.lock();
                Self::process_exits(&mut state, log_sink.as_ref());
                if state.shutting_down && state.children.is_empty() {
                    break;
                }
            }
        });
    }

    /// Caller must hold `self.state` so the spawn + push is atomic with
    /// the slot-availability check.
    fn spawn_one_locked(&self, state: &mut State) -> Result<(), SupervisorError> {
        let mut iter = self.spec.command.iter();
        let program = iter.next().ok_or(SupervisorError::EmptyCommand)?;
        let args: Vec<&OsString> = iter.collect();

        let mut cmd = Command::new(program);
        let piped = self.spec.log_sink.is_some();
        cmd.args(args)
            .current_dir(&self.spec.cwd)
            .stdout(if piped {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .stderr(if piped {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .stdin(Stdio::null())
            .env_clear();
        // Preserve PATH (needed to find `bun`/`node`/etc.) + inherit HOME.
        for key in ["PATH", "HOME"] {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
        }
        for (k, v) in self.spec.effective_env() {
            cmd.env(k, v);
        }

        // Bootstrap ABI: the SDK reads a JSON `{token, secrets}` envelope
        // from fd 3 at startup. The pipe is always created — workers don't
        // currently serve inbound HTTP, but the envelope shape is pinned by
        // `tako_core::bootstrap` and the SDK's fd-3 parser rejects anything
        // else. A unique per-spawn token is cheap and keeps the contract
        // identical to the HTTP instance spawner. The read end must stay
        // alive through `spawn()` so the fork copies a valid fd; the writer
        // thread drains on its own so the parent doesn't deadlock on the
        // pipe buffer.
        #[cfg(unix)]
        let bootstrap_token = nanoid::nanoid!(32);
        #[cfg(unix)]
        let (bootstrap_read_end, bootstrap_writer) =
            create_bootstrap_pipe(&bootstrap_token, &self.spec.secrets)
                .map_err(SupervisorError::Spawn)?;
        #[cfg(unix)]
        let bootstrap_fd: RawFd = bootstrap_read_end.as_raw_fd();

        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(move || {
                if bootstrap_fd != 3 {
                    if libc::dup2(bootstrap_fd, 3) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(bootstrap_fd);
                }
                Ok(())
            });
        }

        tracing::info!(
            app = %self.spec.app,
            workers = self.spec.workers,
            "Spawning worker process"
        );

        let spawn_result = cmd.spawn();
        // Parent-owned read end drops here after spawn, keeping the child's
        // fd 3 alive but releasing our end. The writer thread owns the write
        // end; we join it to surface write errors (or reap it on spawn
        // failure once the read end is dropped and the writer sees EPIPE).
        #[cfg(unix)]
        let mut child = match spawn_result {
            Ok(child) => {
                drop(bootstrap_read_end);
                join_secrets_writer(bootstrap_writer)?;
                child
            }
            Err(error) => {
                // Dropping the read end gives the writer thread EPIPE so it
                // exits instead of wedging on a full pipe buffer. Detaching
                // the JoinHandle is fine — the thread will exit on its own.
                drop(bootstrap_read_end);
                let _ = bootstrap_writer.join();
                return Err(SupervisorError::Spawn(error));
            }
        };
        #[cfg(not(unix))]
        let mut child = spawn_result?;

        if let Some(sink) = &self.spec.log_sink {
            if let Some(stdout) = child.stdout.take() {
                let sink = sink.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        (sink)(&line, false);
                    }
                });
            }
            if let Some(stderr) = child.stderr.take() {
                let sink = sink.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        (sink)(&line, true);
                    }
                });
            }
        }

        state.children.push(ChildEntry {
            child,
            spawned_at: Instant::now(),
            claimed_snapshot: state.health.runs_claimed_total,
        });
        Ok(())
    }
}

fn unix_millis_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn join_secrets_writer(
    handle: std::thread::JoinHandle<std::io::Result<()>>,
) -> Result<(), SupervisorError> {
    match handle.join() {
        Ok(result) => result.map_err(SupervisorError::Spawn),
        Err(_) => Err(SupervisorError::Spawn(std::io::Error::other(
            "secrets writer thread panicked",
        ))),
    }
}

/// Create the fd-3 bootstrap pipe for a worker process: the child reads a
/// JSON `{"token": ..., "secrets": {...}}` envelope and closes the fd. The
/// envelope shape is owned by `tako_core::bootstrap` — sharing it with the
/// app spawner prevents drift between the two spawner paths.
#[cfg(unix)]
fn create_bootstrap_pipe(
    token: &str,
    secrets: &HashMap<String, String>,
) -> std::io::Result<(
    std::os::fd::OwnedFd,
    std::thread::JoinHandle<std::io::Result<()>>,
)> {
    let bytes = tako_core::bootstrap::envelope_bytes(token, secrets);
    tako_spawn::create_payload_pipe(bytes)
}

#[cfg(test)]
mod tests {
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
}

//! Worker process supervisor.
//!
//! One `WorkerSupervisor` per deployed app. Lifecycle:
//!
//! - `workers >= 1` (always-on): spawn N workers on `start`, respawn any that
//!   exit unexpectedly.
//! - `workers == 0` (scale-to-zero): the dispatcher starts one worker to register
//!   the release's schedules, then wakes worker groups for runnable work.
//!   Workers exit after their idle window.
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

/// One named worker group. Empty `WorkerSpec.lanes` means a single
/// `"default"` lane that uses `workers` / `concurrency`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerLane {
    pub name: String,
    pub workers: u32,
    pub concurrency: u32,
}

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
    /// and populates `tako.secrets` from `tako.sh`.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub secrets: HashMap<String, String>,
    /// Storage bindings to hand the worker via fd 3.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub storages: HashMap<String, tako_core::StorageBinding>,
    /// Optional per-line log sink. When `Some`, the supervisor pipes
    /// stdout/stderr and forwards each line. When `None`, inherits the
    /// parent's stdio (production default — lets journald/systemd capture
    /// it).
    pub log_sink: Option<WorkerLogSink>,
    /// Optional production process isolation for server-managed workers.
    pub isolation: Option<tako_spawn::ProcessIsolation>,
    /// Named worker groups. Empty uses a single default lane.
    pub lanes: Vec<WorkerLane>,
}

impl WorkerSpec {
    /// Env vars this supervisor always sets for workers, independent of
    /// the caller-supplied `env`. Caller's `env` is layered on top.
    fn resolved_lanes(&self) -> Vec<WorkerLane> {
        if self.lanes.is_empty() {
            vec![WorkerLane {
                name: "default".into(),
                workers: self.workers,
                concurrency: self.concurrency,
            }]
        } else {
            self.lanes.clone()
        }
    }

    fn effective_env(&self, lane: &WorkerLane) -> HashMap<String, String> {
        let mut env: HashMap<String, String> = self.env.clone();
        env.insert(
            "TAKO_WORKER_CONCURRENCY".into(),
            lane.concurrency.to_string(),
        );
        env.insert(
            "TAKO_WORKER_IDLE_TIMEOUT_MS".into(),
            self.idle_timeout_ms.to_string(),
        );
        env.insert("TAKO_WORKFLOW_WORKER".into(), lane.name.clone());
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
    lane: String,
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
    pub(crate) fn begin_drain(&self) {
        let mut state = self.state.lock();
        if state.shutting_down {
            return;
        }
        state.shutting_down = true;
        for entry in &state.children {
            #[cfg(unix)]
            unsafe {
                if let Some(pid) = entry.child.id() {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
        }
    }

    pub(crate) fn remaining_children(&self) -> usize {
        let mut state = self.state.lock();
        state
            .children
            .retain_mut(|entry| !matches!(entry.child.try_wait(), Ok(Some(_))));
        state.children.len()
    }

    pub(crate) fn force_stop(&self) -> (usize, usize) {
        let mut state = self.state.lock();
        state.shutting_down = true;
        let mut signalled = 0;
        let mut errors = 0;
        for entry in &mut state.children {
            match entry.child.start_kill() {
                Ok(()) => signalled += 1,
                Err(_) => errors += 1,
            }
        }
        (signalled, errors)
    }

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
        let mut state = self.state.lock();
        for lane in self.spec.resolved_lanes() {
            for _ in 0..lane.workers {
                self.spawn_one_locked(&mut state, &lane)?;
            }
        }
        Ok(())
    }

    /// Called by the dispatcher after enqueue/signal/cron/reclaim make
    /// runnable work visible. For scale-to-zero (`workers == 0`), spawns a
    /// worker if none is running. For always-on, respawns any that died. Holds
    /// the state lock across the spawn calls so concurrent wakes can't both
    /// see an empty slot and over-spawn.
    ///
    /// Returns `Unhealthy` during the cooldown window after a crash-loop
    /// detection — caller should surface this to the user instead of
    /// silently respawning.
    pub fn wake(&self) -> Result<(), SupervisorError> {
        self.wake_lanes(&self.spec.resolved_lanes())
    }

    /// One worker can register the complete schedule set before any job exists.
    pub(crate) fn bootstrap_schedules(&self) -> Result<(), SupervisorError> {
        let lanes = self.spec.resolved_lanes();
        self.wake_lanes(&lanes[..1])
    }

    fn wake_lanes(&self, lanes: &[WorkerLane]) -> Result<(), SupervisorError> {
        let mut state = self.state.lock();
        if state.shutting_down {
            return Ok(());
        }
        Self::process_exits(&mut state, self.spec.log_sink.as_ref());
        if let Some(reason) = Self::unhealthy_reason(&state) {
            return Err(SupervisorError::Unhealthy(reason));
        }
        for lane in lanes {
            let live = state
                .children
                .iter()
                .filter(|child| child.lane == lane.name)
                .count();
            let target = if lane.workers == 0 {
                usize::from(live == 0)
            } else {
                (lane.workers as usize).saturating_sub(live)
            };
            for _ in 0..target {
                if let Err(e) = self.spawn_one_locked(&mut state, lane) {
                    let msg = format!("worker spawn failed: {e}");
                    state.health.unhealthy_until = Some(Instant::now() + UNHEALTHY_COOLDOWN);
                    state.health.last_error = Some(msg.clone());
                    Self::emit_health_error(self.spec.log_sink.as_ref(), &msg);
                    return Err(e);
                }
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
        let entries: Vec<ChildEntry> = std::mem::take(&mut state.children);
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
                Err(_) => still_live.push(entry),
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
            std::mem::take(&mut state.children)
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

    /// Ask children to drain and wait for natural exit without SIGKILL.
    ///
    /// Worker entrypoints handle SIGTERM by stopping claims and awaiting
    /// in-flight runs. Used when a deploy replaces/removes workflow code:
    /// running work should finish where it started, while new claims move to
    /// the replacement runtime or stop entirely.
    pub async fn shutdown_gracefully(&self) {
        self.begin_drain();
        while self.remaining_children() > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
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
    fn spawn_one_locked(
        &self,
        state: &mut State,
        lane: &WorkerLane,
    ) -> Result<(), SupervisorError> {
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
        for (k, v) in self.spec.effective_env(lane) {
            cmd.env(k, v);
        }

        // Bootstrap ABI: the SDK reads a JSON `{token, secrets, storages}` envelope
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
            create_bootstrap_pipe(&bootstrap_token, &self.spec.secrets, &self.spec.storages)
                .map_err(SupervisorError::Spawn)?;
        #[cfg(unix)]
        let bootstrap_fd: RawFd = bootstrap_read_end.as_raw_fd();
        #[cfg(unix)]
        let isolation = self.spec.isolation.clone();

        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(move || {
                if bootstrap_fd != 3 {
                    if libc::dup2(bootstrap_fd, 3) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(bootstrap_fd);
                }
                if let Some(isolation) = &isolation {
                    tako_spawn::install_process_isolation(isolation)?;
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
                join_secrets_writer_after_spawn(bootstrap_writer)?;
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
            lane: lane.name.clone(),
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
fn join_secrets_writer_after_spawn(
    handle: std::thread::JoinHandle<std::io::Result<()>>,
) -> Result<(), SupervisorError> {
    match handle.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Ok(Err(error)) => Err(SupervisorError::Spawn(error)),
        Err(_) => Err(SupervisorError::Spawn(std::io::Error::other(
            "secrets writer thread panicked",
        ))),
    }
}

/// Create the fd-3 bootstrap pipe for a worker process: the child reads a
/// JSON `{"token": ..., "secrets": {...}, "storages": {...}}` envelope and closes the fd. The
/// envelope shape is owned by `tako_core::bootstrap` — sharing it with the
/// app spawner prevents drift between the two spawner paths.
#[cfg(unix)]
fn create_bootstrap_pipe(
    token: &str,
    secrets: &HashMap<String, String>,
    storages: &HashMap<String, tako_core::StorageBinding>,
) -> std::io::Result<(
    std::os::fd::OwnedFd,
    std::thread::JoinHandle<std::io::Result<()>>,
)> {
    let bytes = tako_core::bootstrap::envelope_bytes(token, secrets, storages);
    tako_spawn::create_payload_pipe(bytes)
}

/// Scale-to-zero lanes for every worker group declared under `workflows_dir`,
/// plus the default group. Used by tako-server and `tako dev`.
pub fn workflow_lanes_from_dir(
    workflows_dir: &std::path::Path,
    workers: u32,
    concurrency: u32,
) -> Vec<WorkerLane> {
    let mut names = std::collections::BTreeSet::from(["default".to_string()]);
    collect_worker_group_names(workflows_dir, &mut names);
    names
        .into_iter()
        .map(|name| WorkerLane {
            name,
            workers,
            concurrency,
        })
        .collect()
}

fn collect_worker_group_names(
    workflows_dir: &std::path::Path,
    names: &mut std::collections::BTreeSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(workflows_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_workflow_source(&path) {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        collect_worker_names_from_source(&source, names);
    }
}

fn is_workflow_source(path: &std::path::Path) -> bool {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.ends_with(".d.ts") || name.ends_with(".d.mts") || name.ends_with(".d.cts")
        })
    {
        return false;
    }
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("ts" | "tsx" | "js" | "mjs" | "mts")
    )
}

fn collect_worker_names_from_source(source: &str, names: &mut std::collections::BTreeSet<String>) {
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => i = skip_js_string(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = i.saturating_add(2);
            }
            b'w' if bytes[i..].starts_with(b"worker") => {
                let after = i + "worker".len();
                if let Some(name) = worker_name_after_key(bytes, after) {
                    names.insert(name);
                }
                i = after;
            }
            _ => i += 1,
        }
    }
}

fn worker_name_after_key(bytes: &[u8], mut i: usize) -> Option<String> {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if bytes.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let quote = *bytes.get(i)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    i += 1;
    let start = i;
    while i < bytes.len() && bytes[i] != quote {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let name = std::str::from_utf8(&bytes[start..i]).ok()?.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some(name.to_string())
}

fn skip_js_string(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = i.saturating_add(2);
            continue;
        }
        if bytes[i] == quote {
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

const PID_FILE_DIR: &str = ".tako/pids";

/// Persistent app registration (survives server restarts).
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredApp {
    pub config_file: String,
    pub config_path: String,
    pub project_dir: String,
    pub name: String,
    pub variant: Option<String>,
    pub is_enabled: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

// ---------------------------------------------------------------------------
// In-memory log ring buffer — replaces the JSONL file-based log store.
// ---------------------------------------------------------------------------

const LOG_BUFFER_CAPACITY: usize = 500;

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub id: u64,
    pub line: String,
}

struct LogBufferInner {
    entries: VecDeque<LogEntry>,
    next_id: u64,
    capacity: usize,
    sender: broadcast::Sender<LogEntry>,
}

/// Thread-safe, clonable log ring buffer.
///
/// Stores up to `capacity` entries per app. When the buffer is full, the oldest
/// entry is dropped. Subscribers receive new entries in real time.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<LogBufferInner>>,
}

impl std::fmt::Debug for LogBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogBuffer").finish_non_exhaustive()
    }
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LogBufferInner {
                entries: VecDeque::with_capacity(LOG_BUFFER_CAPACITY),
                next_id: 0,
                capacity: LOG_BUFFER_CAPACITY,
                sender: broadcast::channel(512).0,
            })),
        }
    }

    /// Push a line into the buffer. Assigns a sequential ID, trims the oldest
    /// entry if over capacity, and broadcasts to all live subscribers.
    pub fn push(&self, line: String) {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        let entry = LogEntry {
            id,
            line: line.clone(),
        };
        if inner.entries.len() >= inner.capacity {
            inner.entries.pop_front();
        }
        inner.entries.push_back(entry.clone());
        let _ = inner.sender.send(entry);
    }

    /// Subscribe to the log stream. Returns:
    /// - backlog entries after the given `after` ID (or all buffered if None)
    /// - a receiver for new entries
    /// - whether the requested `after` point was truncated (oldest entries dropped)
    pub fn subscribe(
        &self,
        after: Option<u64>,
    ) -> (Vec<LogEntry>, broadcast::Receiver<LogEntry>, bool) {
        let inner = self.inner.lock().unwrap();
        let rx = inner.sender.subscribe();

        let oldest_id = inner.entries.front().map(|e| e.id);
        let truncated = match (after, oldest_id) {
            (Some(req), Some(oldest)) => req < oldest,
            (Some(_), None) => false, // buffer empty, nothing truncated
            (None, _) => false,
        };

        let backlog: Vec<LogEntry> = match after {
            Some(after_id) => inner
                .entries
                .iter()
                .filter(|e| e.id > after_id)
                .cloned()
                .collect(),
            None => inner.entries.iter().cloned().collect(),
        };

        (backlog, rx, truncated)
    }

    /// Clear all entries. Preserves the ID counter so cursor-based resumption
    /// still works across clears. Existing subscribers remain connected.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.clear();
    }
}

/// Runtime app state (in-memory only, lost on server restart).
#[derive(Debug, Clone)]
pub struct RuntimeApp {
    pub app_namespace: String,
    pub config_file: String,
    pub environment: String,
    pub compose: Option<tako_compose::ComposeRuntime>,
    pub project_dir: String,
    pub name: String,
    pub variant: Option<String>,
    pub hosts: Vec<String>,
    pub upstream_port: u16,
    pub is_idle: bool,
    pub command: Vec<String>,
    pub worker_command: Option<Vec<String>>,
    pub workflow_retention_ms: Option<i64>,
    pub env: HashMap<String, String>,
    pub log_buffer: LogBuffer,
    pub pid: Option<u32>,
    pub client_pid: Option<u32>,
    pub tunnel: Option<crate::tunnel::TunnelRegistration>,
    pub readiness_failure_hint: Option<String>,
    pub bootstrap_token: String,
    pub secrets: HashMap<String, String>,
    pub storages: HashMap<String, tako_core::StorageBinding>,
    pub databases: HashMap<String, tako_core::DatabaseConnection>,
    pub kv: HashMap<String, tako_core::KvConnection>,
}

// ---------------------------------------------------------------------------
// PID file management — {project_dir}/.tako/pids/<config-hash>.pid
// ---------------------------------------------------------------------------

fn pid_file_key(config_path: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config_path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn pid_file_path(project_dir: &str, config_path: &str) -> PathBuf {
    Path::new(project_dir)
        .join(PID_FILE_DIR)
        .join(format!("{}.pid", pid_file_key(config_path)))
}

/// Write the app's PID to a config-scoped pid file.
pub fn write_pid_file(project_dir: &str, config_path: &str, pid: u32) {
    let path = pid_file_path(project_dir, config_path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, pid.to_string());
}

/// Remove the PID file for an app.
pub fn remove_pid_file(project_dir: &str, config_path: &str) {
    let _ = std::fs::remove_file(pid_file_path(project_dir, config_path));
}

/// Read the PID for an app's config-scoped pid file, if it exists.
pub fn read_pid_file(project_dir: &str, config_path: &str) -> Option<u32> {
    std::fs::read_to_string(pid_file_path(project_dir, config_path))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Kill any orphaned app process from a previous server run and clean up
/// the PID file. Called on startup for each registered project.
pub fn kill_orphaned_process(project_dir: &str, config_path: &str) {
    let Some(pid) = read_pid_file(project_dir, config_path) else {
        return;
    };
    if pid == 0 {
        remove_pid_file(project_dir, config_path);
        return;
    }
    // Check if the process is still alive.
    let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
    if alive {
        tracing::info!(
            project_dir = %project_dir,
            config_path = %config_path,
            pid = pid,
            "killing orphaned app process from previous run"
        );
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    remove_pid_file(project_dir, config_path);
}

// ---------------------------------------------------------------------------
// redb store — persists registrations across restarts
// ---------------------------------------------------------------------------

/// Config path → JSON-encoded [`RegisteredApp`].
const APPS: TableDefinition<&str, &str> = TableDefinition::new("apps");

pub struct DevStateStore {
    db: Database,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl DevStateStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create db parent: {e}"))?;
        }
        let db = Database::create(&path).map_err(|e| format!("open db: {e}"))?;
        let store = Self { db };
        // Create the table so reads never meet a missing one.
        store.write(|_| Ok(()))?;
        Ok(store)
    }

    fn write<T>(
        &self,
        change: impl FnOnce(&mut redb::Table<&str, &str>) -> Result<T, String>,
    ) -> Result<T, String> {
        let txn = self.db.begin_write().map_err(|e| format!("write: {e}"))?;
        let value = {
            let mut apps = txn.open_table(APPS).map_err(|e| format!("write: {e}"))?;
            change(&mut apps)?
        };
        txn.commit().map_err(|e| format!("commit: {e}"))?;
        Ok(value)
    }

    pub fn register(
        &self,
        config_path: &str,
        config_file: &str,
        project_dir: &str,
        name: &str,
        variant: Option<&str>,
    ) -> Result<(), String> {
        let now = unix_now();
        self.write(|apps| {
            let existing = apps
                .get(config_path)
                .map_err(|e| format!("register: {e}"))?
                .map(|row| decode(row.value()))
                .transpose()?;
            let app = RegisteredApp {
                config_file: config_file.to_string(),
                config_path: config_path.to_string(),
                project_dir: project_dir.to_string(),
                name: name.to_string(),
                variant: variant.map(str::to_string),
                is_enabled: existing.as_ref().is_none_or(|app| app.is_enabled),
                created_at: existing.as_ref().map_or(now, |app| app.created_at),
                updated_at: now,
            };
            let json = serde_json::to_string(&app).map_err(|e| format!("register: {e}"))?;
            apps.insert(config_path, json.as_str())
                .map_err(|e| format!("register: {e}"))?;
            Ok(())
        })
    }

    pub fn unregister(&self, config_path: &str) -> Result<bool, String> {
        self.write(|apps| {
            Ok(apps
                .remove(config_path)
                .map_err(|e| format!("unregister: {e}"))?
                .is_some())
        })
    }

    #[cfg(test)]
    pub fn get(&self, config_path: &str) -> Result<Option<RegisteredApp>, String> {
        let txn = self.db.begin_read().map_err(|e| format!("get: {e}"))?;
        let apps = txn.open_table(APPS).map_err(|e| format!("get: {e}"))?;
        apps.get(config_path)
            .map_err(|e| format!("get: {e}"))?
            .map(|row| decode(row.value()))
            .transpose()
    }

    pub fn list(&self) -> Result<Vec<RegisteredApp>, String> {
        let txn = self.db.begin_read().map_err(|e| format!("list: {e}"))?;
        let apps = txn.open_table(APPS).map_err(|e| format!("list: {e}"))?;
        let mut out = Vec::new();
        for row in apps.iter().map_err(|e| format!("list: {e}"))? {
            let (_, value) = row.map_err(|e| format!("list: {e}"))?;
            out.push(decode(value.value())?);
        }
        out.sort_by(|a, b| (&a.name, &a.config_path).cmp(&(&b.name, &b.config_path)));
        Ok(out)
    }

    #[cfg(test)]
    pub fn set_enabled(&self, config_path: &str, enabled: bool) -> Result<bool, String> {
        let now = unix_now();
        self.write(|apps| {
            let Some(mut app) = apps
                .get(config_path)
                .map_err(|e| format!("set_enabled: {e}"))?
                .map(|row| decode(row.value()))
                .transpose()?
            else {
                return Ok(false);
            };
            app.is_enabled = enabled;
            app.updated_at = now;
            let json = serde_json::to_string(&app).map_err(|e| format!("set_enabled: {e}"))?;
            apps.insert(config_path, json.as_str())
                .map_err(|e| format!("set_enabled: {e}"))?;
            Ok(true)
        })
    }

    pub fn cleanup_stale(&self) -> Result<Vec<String>, String> {
        let apps = self.list()?;
        let mut removed = Vec::new();
        for app in apps {
            if !Path::new(&app.config_file).exists() {
                self.unregister(&app.config_path)?;
                removed.push(app.config_path);
            }
        }
        Ok(removed)
    }
}

fn decode(json: &str) -> Result<RegisteredApp, String> {
    serde_json::from_str(json).map_err(|e| format!("read app registration: {e}"))
}

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
use rusqlite::OptionalExtension;
use rusqlite::{Connection, params};
use tokio::sync::mpsc;

const PID_FILE_DIR: &str = ".tako/pids";

/// Persistent app registration (survives server restarts).
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct RegisteredApp {
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
    subscribers: Vec<mpsc::UnboundedSender<LogEntry>>,
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
                subscribers: Vec::new(),
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
        inner
            .subscribers
            .retain(|tx| tx.send(entry.clone()).is_ok());
    }

    /// Subscribe to the log stream. Returns:
    /// - backlog entries after the given `after` ID (or all buffered if None)
    /// - a receiver for new entries
    /// - whether the requested `after` point was truncated (oldest entries dropped)
    pub fn subscribe(
        &self,
        after: Option<u64>,
    ) -> (Vec<LogEntry>, mpsc::UnboundedReceiver<LogEntry>, bool) {
        let mut inner = self.inner.lock().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

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

        inner.subscribers.push(tx);
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
    pub project_dir: String,
    pub name: String,
    pub variant: Option<String>,
    pub hosts: Vec<String>,
    pub upstream_port: u16,
    pub is_idle: bool,
    pub command: Vec<String>,
    pub env: HashMap<String, String>,
    pub log_buffer: LogBuffer,
    pub pid: Option<u32>,
    pub client_pid: Option<u32>,
    pub tunnel: Option<crate::tunnel::TunnelRegistration>,
    pub readiness_failure_hint: Option<String>,
    pub bootstrap_token: String,
    pub secrets: HashMap<String, String>,
    pub storages: HashMap<String, tako_core::StorageBinding>,
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
// SQLite store — persists registration across restarts
// ---------------------------------------------------------------------------

pub struct DevStateStore {
    conn: Connection,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn set_pragmas(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;",
    )
    .map_err(|e| format!("set pragmas: {e}"))
}

impl DevStateStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create db parent: {e}"))?;
        }
        let conn = Connection::open(&path).map_err(|e| format!("open db: {e}"))?;
        set_pragmas(&conn)?;
        ensure_schema(&conn)?;
        Ok(Self { conn })
    }

    pub fn register(
        &self,
        config_path: &str,
        project_dir: &str,
        name: &str,
        variant: Option<&str>,
    ) -> Result<(), String> {
        let now = unix_now() as i64;
        self.conn
            .execute(
                "INSERT INTO apps (config_path, project_dir, name, variant, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(config_path) DO UPDATE SET
                    project_dir = excluded.project_dir,
                    name = excluded.name,
                    variant = excluded.variant,
                    updated_at = excluded.updated_at;",
                params![config_path, project_dir, name, variant, now],
            )
            .map_err(|e| format!("register: {e}"))?;
        Ok(())
    }

    pub fn unregister(&self, config_path: &str) -> Result<bool, String> {
        let rows = self
            .conn
            .execute(
                "DELETE FROM apps WHERE config_path = ?1;",
                params![config_path],
            )
            .map_err(|e| format!("unregister: {e}"))?;
        Ok(rows > 0)
    }

    #[cfg(test)]
    pub fn get(&self, config_path: &str) -> Result<Option<RegisteredApp>, String> {
        self.conn
            .query_row(
                "SELECT config_path, project_dir, name, variant, is_enabled, created_at, updated_at
                 FROM apps WHERE config_path = ?1;",
                params![config_path],
                row_to_registered_app,
            )
            .optional()
            .map_err(|e| format!("get: {e}"))
    }

    pub fn list(&self) -> Result<Vec<RegisteredApp>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT config_path, project_dir, name, variant, is_enabled, created_at, updated_at
                 FROM apps ORDER BY name, config_path;",
            )
            .map_err(|e| format!("prepare list: {e}"))?;
        stmt.query_map([], row_to_registered_app)
            .map_err(|e| format!("list: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("list collect: {e}"))
    }

    #[cfg(test)]
    pub fn set_enabled(&self, config_path: &str, enabled: bool) -> Result<bool, String> {
        let now = unix_now() as i64;
        let rows = self
            .conn
            .execute(
                "UPDATE apps SET is_enabled = ?1, updated_at = ?2 WHERE config_path = ?3;",
                params![enabled, now, config_path],
            )
            .map_err(|e| format!("set_enabled: {e}"))?;
        Ok(rows > 0)
    }

    pub fn cleanup_stale(&self) -> Result<Vec<String>, String> {
        let apps = self.list()?;
        let mut removed = Vec::new();
        for app in apps {
            if !Path::new(&app.config_path).exists() {
                self.unregister(&app.config_path)?;
                removed.push(app.config_path);
            }
        }
        Ok(removed)
    }
}

fn row_to_registered_app(row: &rusqlite::Row) -> rusqlite::Result<RegisteredApp> {
    Ok(RegisteredApp {
        config_path: row.get(0)?,
        project_dir: row.get(1)?,
        name: row.get(2)?,
        variant: row.get(3)?,
        is_enabled: row.get(4)?,
        created_at: row.get::<_, i64>(5)? as u64,
        updated_at: row.get::<_, i64>(6)? as u64,
    })
}

fn ensure_schema(conn: &Connection) -> Result<(), String> {
    let columns = table_columns(conn, "apps")?;
    if columns.is_empty() {
        return create_apps_table(conn);
    }

    // v0: no migrations — drop and recreate if schema doesn't match.
    let expected = [
        "config_path",
        "project_dir",
        "name",
        "variant",
        "is_enabled",
        "created_at",
        "updated_at",
    ];
    if !expected.iter().all(|col| columns.iter().any(|c| c == col)) {
        conn.execute_batch("DROP TABLE apps;")
            .map_err(|e| format!("drop outdated apps table: {e}"))?;
        return create_apps_table(conn);
    }

    Ok(())
}

fn create_apps_table(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS apps (
            config_path TEXT PRIMARY KEY,
            project_dir TEXT NOT NULL,
            name TEXT NOT NULL,
            variant TEXT,
            is_enabled INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL DEFAULT 0
        );",
    )
    .map_err(|e| format!("create apps schema: {e}"))
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    conn.prepare(&format!("PRAGMA table_info({table});"))
        .map_err(|e| format!("prepare table info: {e}"))?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("query table info: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("collect table info: {e}"))
}

#[cfg(test)]
mod tests;

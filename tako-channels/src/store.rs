use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{ChannelAuthResponse, ChannelError, ChannelMessage, ChannelPublishPayload};

const CHANNELS_DB_FILENAME: &str = "channels.sqlite";
const INCREMENTAL_VACUUM_PAGES: i64 = 128;
const WAL_TRUNCATE_DELETED_ROWS_THRESHOLD: usize = 1024;

/// Build the SQLite DB path from a data directory and app name.
/// Callers provide their own app/env path resolution: production uses
/// env-scoped `app_runtime_data_paths`. Local dev uses in-memory stores
/// and does not call this helper.
pub fn channels_db_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join(CHANNELS_DB_FILENAME)
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Per app/environment SQLite-backed channel store.
///
/// The connection is opened once and reused; every operation locks a
/// mutex and uses the cached connection. Callers should hold a single
/// `ChannelStore` for each DB path and share it across requests (e.g.
/// behind an `Arc`): constructing a new `ChannelStore` reruns pragmas
/// and schema init on every call.
pub struct ChannelStore {
    pub(crate) conn: Mutex<rusqlite::Connection>,
}

impl ChannelStore {
    /// Open (or create) the channel DB at `path` and run the idempotent
    /// schema init. Safe to call repeatedly against the same path because
    /// SQLite supports multiple connections per file, but callers are
    /// expected to hold the returned store for the process's lifetime.
    pub fn open(path: &Path) -> Result<Self, ChannelError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ChannelError::Storage(format!("create channel dir: {e}")))?;
        }
        let conn =
            rusqlite::Connection::open(path).map_err(|e| ChannelError::Storage(e.to_string()))?;
        init_connection(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory channel DB. Used by local dev where replay only
    /// needs to survive reconnects within the current daemon process.
    pub fn open_in_memory() -> Result<Self, ChannelError> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        init_connection(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn append(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
    ) -> Result<ChannelMessage, ChannelError> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction()
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        tx.execute(
            "UPDATE channel_metadata SET last_activity_unix_ms = ?2 WHERE channel = ?1",
            rusqlite::params![channel, now_unix_ms()],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
        tx.execute(
            "INSERT INTO channel_messages (channel, type, data_json) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                channel,
                payload.r#type,
                serde_json::to_string(&payload.data)
                    .map_err(|e| ChannelError::BadRequest(format!("serialize payload: {e}")))?,
            ],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

        let id = tx.last_insert_rowid();
        tx.commit()
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        Ok(ChannelMessage {
            id: id.to_string(),
            channel: channel.to_string(),
            r#type: payload.r#type.clone(),
            data: payload.data.clone(),
        })
    }

    pub fn read_after(
        &self,
        channel: &str,
        after: Option<i64>,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, channel, type, data_json
                 FROM channel_messages
                 WHERE channel = ?1 AND (?2 IS NULL OR id > ?2)
                 ORDER BY id ASC
                 LIMIT ?3",
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![channel, after, i64::from(limit)], |row| {
                let data_json: String = row.get(3)?;
                let data = serde_json::from_str(&data_json).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })?;
                Ok(ChannelMessage {
                    id: row.get::<_, i64>(0)?.to_string(),
                    channel: row.get(1)?,
                    r#type: row.get(2)?,
                    data,
                })
            })
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| ChannelError::Storage(e.to_string()))
    }

    pub fn replay_cursor(
        &self,
        channel: &str,
        requested: Option<i64>,
    ) -> Result<Option<i64>, ChannelError> {
        let conn = self.conn.lock();
        let latest = message_id(&conn, channel, "MAX")?;
        let Some(requested) = requested else {
            return Ok(latest);
        };

        let Some(oldest) = message_id(&conn, channel, "MIN")? else {
            return Ok(Some(requested));
        };

        if requested < oldest.saturating_sub(1) {
            return Err(ChannelError::StaleCursor);
        }

        Ok(Some(requested))
    }

    pub fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        let conn = self.conn.lock();
        let now = now_unix_ms();
        conn.execute(
            "INSERT INTO channel_metadata (
                channel,
                replay_window_ms,
                inactivity_ttl_ms,
                keepalive_interval_ms,
                max_connection_lifetime_ms,
                last_activity_unix_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(channel) DO UPDATE SET
                replay_window_ms = excluded.replay_window_ms,
                inactivity_ttl_ms = excluded.inactivity_ttl_ms,
                keepalive_interval_ms = excluded.keepalive_interval_ms,
                max_connection_lifetime_ms = excluded.max_connection_lifetime_ms,
                last_activity_unix_ms = excluded.last_activity_unix_ms",
            rusqlite::params![
                channel,
                auth.replay_window_ms as i64,
                auth.inactivity_ttl_ms as i64,
                auth.keepalive_interval_ms as i64,
                auth.max_connection_lifetime_ms as i64,
                now,
            ],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

        let mut deleted_rows = 0usize;

        if auth.replay_window_ms > 0 {
            let cutoff = now - auth.replay_window_ms as i64;
            deleted_rows += conn
                .execute(
                    "DELETE FROM channel_messages WHERE channel = ?1 AND created_at_unix_ms < ?2",
                    rusqlite::params![channel, cutoff],
                )
                .map_err(|e| ChannelError::Storage(e.to_string()))?;
        }

        deleted_rows += conn
            .execute(
                "DELETE FROM channel_messages
             WHERE channel IN (
                SELECT channel
                FROM channel_metadata
                WHERE inactivity_ttl_ms > 0
                  AND last_activity_unix_ms < (?1 - inactivity_ttl_ms)
             )",
                rusqlite::params![now],
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        deleted_rows += conn
            .execute(
                "DELETE FROM channel_metadata
             WHERE inactivity_ttl_ms > 0
               AND last_activity_unix_ms < (?1 - inactivity_ttl_ms)",
                rusqlite::params![now],
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        if deleted_rows > 0 {
            run_cleanup_maintenance(&conn, deleted_rows);
        }

        Ok(())
    }
}

fn message_id(
    conn: &rusqlite::Connection,
    channel: &str,
    aggregate: &str,
) -> Result<Option<i64>, ChannelError> {
    let sql = format!("SELECT {aggregate}(id) FROM channel_messages WHERE channel = ?1");
    conn.query_row(&sql, rusqlite::params![channel], |row| row.get(0))
        .map_err(|e| ChannelError::Storage(e.to_string()))
}

fn init_connection(conn: &rusqlite::Connection) -> Result<(), ChannelError> {
    conn.execute_batch(
        "PRAGMA auto_vacuum = INCREMENTAL;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS channel_messages (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             channel TEXT NOT NULL,
             type TEXT NOT NULL,
             data_json TEXT NOT NULL,
             created_at_unix_ms INTEGER NOT NULL DEFAULT (unixepoch() * 1000)
         );",
    )
    .map_err(|e| ChannelError::Storage(e.to_string()))?;

    ensure_channel_metadata_schema(conn)?;

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_channel_messages_channel_id
         ON channel_messages(channel, id);",
    )
    .map_err(|e| ChannelError::Storage(e.to_string()))?;
    Ok(())
}

fn ensure_channel_metadata_schema(conn: &rusqlite::Connection) -> Result<(), ChannelError> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'channel_metadata'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

    if exists == 0 {
        conn.execute_batch(
            "CREATE TABLE channel_metadata (
                channel TEXT PRIMARY KEY,
                replay_window_ms INTEGER NOT NULL,
                inactivity_ttl_ms INTEGER NOT NULL,
                keepalive_interval_ms INTEGER NOT NULL,
                max_connection_lifetime_ms INTEGER NOT NULL,
                last_activity_unix_ms INTEGER NOT NULL
            );",
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
        return Ok(());
    }

    let mut columns = conn
        .prepare("PRAGMA table_info(channel_metadata)")
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    let columns = columns
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| ChannelError::Storage(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

    if columns.iter().any(|column| column == "retention_ms")
        && !columns.iter().any(|column| column == "replay_window_ms")
    {
        conn.execute_batch(
            "ALTER TABLE channel_metadata RENAME COLUMN retention_ms TO replay_window_ms;",
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    }

    Ok(())
}

fn run_cleanup_maintenance(conn: &rusqlite::Connection, deleted_rows: usize) {
    let vacuum_sql = format!("PRAGMA incremental_vacuum({INCREMENTAL_VACUUM_PAGES});");
    let _ = conn.execute_batch(&vacuum_sql);

    if deleted_rows >= WAL_TRUNCATE_DELETED_ROWS_THRESHOLD {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
}

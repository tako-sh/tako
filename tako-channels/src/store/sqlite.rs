use parking_lot::Mutex;
use std::path::Path;

use crate::{ChannelAuthResponse, ChannelError, ChannelMessage, ChannelPublishPayload};

use super::{channel_message_from_row, now_unix_ms, resolve_replay_cursor};

// Autoincrement message IDs start at one, so zero is the initial cursor.
const READ_AFTER_SQL: &str = "SELECT id, channel, type, data_json
    FROM channel_messages WHERE channel = ?1 AND id > ?2 ORDER BY id ASC LIMIT ?3";

const INCREMENTAL_VACUUM_PAGES: i64 = 128;
const WAL_TRUNCATE_DELETED_ROWS_THRESHOLD: usize = 1024;

fn storage_err(e: impl std::fmt::Display) -> ChannelError {
    ChannelError::Storage(e.to_string())
}

/// Per app/environment SQLite-backed channel store.
///
/// The connection is opened once and reused; every operation locks a
/// mutex and uses the cached connection. Callers should hold a single
/// `ChannelStore` for each DB path and share it across requests (e.g.
/// behind an `Arc`): constructing a new `ChannelStore` reruns pragmas
/// and schema init on every call.
pub(super) struct SqliteChannelStore {
    pub(crate) conn: Mutex<rusqlite::Connection>,
}

impl SqliteChannelStore {
    pub(super) fn open(path: &Path) -> Result<Self, ChannelError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ChannelError::Storage(format!("create channel dir: {e}")))?;
        }
        let conn = tako_sqlite::open_local(path).map_err(storage_err)?;
        init_connection(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub(super) fn open_in_memory() -> Result<Self, ChannelError> {
        let conn = tako_sqlite::open_in_memory().map_err(storage_err)?;
        init_connection(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub(super) fn append(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
    ) -> Result<ChannelMessage, ChannelError> {
        let data_json = serde_json::to_string(&payload.data)
            .map_err(|e| ChannelError::BadRequest(format!("serialize payload: {e}")))?;
        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(storage_err)?;
        tx.execute(
            "INSERT INTO channel_metadata (channel, replay_window_ms, inactivity_ttl_ms, keepalive_interval_ms, max_connection_lifetime_ms, last_activity_unix_ms)
             VALUES (?1, 600000, 0, 25000, 7200000, ?2) ON CONFLICT(channel) DO NOTHING",
            rusqlite::params![channel, now_unix_ms()],
        ).map_err(storage_err)?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "UPDATE channel_metadata SET last_activity_unix_ms = ?2 WHERE channel = ?1",
                )
                .map_err(storage_err)?;
            stmt.execute(rusqlite::params![channel, now_unix_ms()])
                .map_err(storage_err)?;
        }
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO channel_messages (channel, type, data_json) VALUES (?1, ?2, ?3)",
                )
                .map_err(storage_err)?;
            stmt.execute(rusqlite::params![channel, payload.r#type, data_json])
                .map_err(storage_err)?;
        }

        let id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE channel_metadata SET latest_message_id = ?2 WHERE channel = ?1",
            rusqlite::params![channel, id],
        )
        .map_err(storage_err)?;
        tx.commit().map_err(storage_err)?;

        Ok(ChannelMessage {
            id: id.to_string(),
            channel: channel.to_string(),
            r#type: payload.r#type.clone(),
            data: payload.data.clone(),
        })
    }

    pub(super) fn read_after(
        &self,
        channel: &str,
        after: Option<i64>,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        let rows = {
            let conn = self.conn.lock();
            let mut stmt = conn.prepare_cached(READ_AFTER_SQL).map_err(storage_err)?;

            let rows = stmt
                .query_map(
                    rusqlite::params![channel, after.unwrap_or(0), i64::from(limit)],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .map_err(storage_err)?;

            rows.collect::<Result<Vec<_>, _>>().map_err(storage_err)?
        };

        rows.into_iter().map(channel_message_from_row).collect()
    }

    pub(super) fn replay_cursor(
        &self,
        channel: &str,
        requested: Option<i64>,
    ) -> Result<Option<i64>, ChannelError> {
        let conn = self.conn.lock();
        let (oldest, latest) = conn.query_row(
            "SELECT MIN(id), COALESCE(MAX(id), (SELECT NULLIF(latest_message_id, 0) FROM channel_metadata WHERE channel = ?1))
             FROM channel_messages WHERE channel = ?1",
            rusqlite::params![channel], |row| Ok((row.get(0)?, row.get(1)?)),
        ).map_err(storage_err)?;
        resolve_replay_cursor(requested, oldest, latest)
    }

    pub(super) fn sync_channel(
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
        .map_err(storage_err)?;

        Ok(())
    }

    pub(super) fn latest_ids(
        &self,
        channels: &[String],
    ) -> Result<Vec<(String, i64)>, ChannelError> {
        let channels = serde_json::to_string(channels).map_err(storage_err)?;
        let conn = self.conn.lock();
        let mut statement = conn
            .prepare_cached(
                "SELECT channel, latest_message_id FROM channel_metadata
             WHERE channel IN (SELECT value FROM json_each(?1)) AND latest_message_id > 0",
            )
            .map_err(storage_err)?;
        statement
            .query_map([channels], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(storage_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage_err)
    }

    pub(super) fn prune(&self) -> Result<(), ChannelError> {
        let conn = self.conn.lock();
        let now = now_unix_ms();

        conn.execute("DELETE FROM presence WHERE expires_at <= ?1", [now])
            .map_err(storage_err)?;
        let mut deleted_rows = 0usize;

        deleted_rows += conn
                .execute(
                    "DELETE FROM channel_messages WHERE EXISTS (
                        SELECT 1 FROM channel_metadata m WHERE m.channel = channel_messages.channel
                        AND m.replay_window_ms > 0 AND channel_messages.created_at_unix_ms < (?1 - m.replay_window_ms)
                    )",
                    rusqlite::params![now],
                )
                .map_err(storage_err)?;

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
            .map_err(storage_err)?;

        if deleted_rows > 0 {
            run_cleanup_maintenance(&conn, deleted_rows);
        }

        Ok(())
    }
}

fn init_connection(conn: &rusqlite::Connection) -> Result<(), ChannelError> {
    conn.execute_batch(
        "PRAGMA auto_vacuum = INCREMENTAL;
         CREATE TABLE IF NOT EXISTS presence (
             channel TEXT NOT NULL, connection_id TEXT NOT NULL, member_json TEXT NOT NULL,
             expires_at INTEGER NOT NULL, PRIMARY KEY(channel, connection_id)
         );
         CREATE TABLE IF NOT EXISTS channel_messages (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             channel TEXT NOT NULL,
             type TEXT NOT NULL,
             data_json TEXT NOT NULL,
             created_at_unix_ms INTEGER NOT NULL DEFAULT (unixepoch() * 1000)
         );",
    )
    .map_err(storage_err)?;

    ensure_channel_metadata_schema(conn)?;

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_channel_messages_channel_id
         ON channel_messages(channel, id);",
    )
    .map_err(storage_err)?;
    Ok(())
}

fn ensure_channel_metadata_schema(conn: &rusqlite::Connection) -> Result<(), ChannelError> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'channel_metadata'",
            [],
            |row| row.get(0),
        )
        .map_err(storage_err)?;

    if exists == 0 {
        conn.execute_batch(
            "CREATE TABLE channel_metadata (
                channel TEXT PRIMARY KEY,
                replay_window_ms INTEGER NOT NULL,
                inactivity_ttl_ms INTEGER NOT NULL,
                keepalive_interval_ms INTEGER NOT NULL,
                max_connection_lifetime_ms INTEGER NOT NULL,
                last_activity_unix_ms INTEGER NOT NULL,
                latest_message_id INTEGER NOT NULL DEFAULT 0
            );",
        )
        .map_err(storage_err)?;
        return Ok(());
    }

    let mut columns = conn
        .prepare("PRAGMA table_info(channel_metadata)")
        .map_err(storage_err)?;
    let columns = columns
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(storage_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_err)?;

    if !columns.iter().any(|column| column == "latest_message_id") {
        conn.execute_batch(
            "ALTER TABLE channel_metadata ADD COLUMN latest_message_id INTEGER NOT NULL DEFAULT 0;",
        )
        .map_err(storage_err)?;
    }

    if columns.iter().any(|column| column == "retention_ms")
        && !columns.iter().any(|column| column == "replay_window_ms")
    {
        conn.execute_batch(
            "ALTER TABLE channel_metadata RENAME COLUMN retention_ms TO replay_window_ms;",
        )
        .map_err(storage_err)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_query_seeks_to_cursor_instead_of_scanning_channel_history() {
        let store = SqliteChannelStore::open_in_memory().unwrap();
        let conn = store.conn.lock();
        let detail: String = conn
            .query_row(
                &format!("EXPLAIN QUERY PLAN {READ_AFTER_SQL}"),
                rusqlite::params!["feed", 100_000, 100],
                |row| row.get(3),
            )
            .unwrap();
        assert!(detail.contains("channel=? AND id>?"), "{detail}");
    }
}

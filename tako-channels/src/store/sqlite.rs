use crate::{ChannelEvent, ChannelEventId};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use parking_lot::Mutex;
use std::path::Path;

use crate::{
    ChannelAuthResponse, ChannelError, ChannelMessage, ChannelPublishPayload, ReplayCursor,
};

use super::{channel_message_from_row, now_unix_ms};
use rusqlite::OptionalExtension;

// Autoincrement message IDs start at one, so zero is the initial cursor.
const READ_AFTER_SQL: &str = "SELECT id, channel, type, data_json,
    (SELECT event_json FROM channel_message_events WHERE message_id = channel_messages.id)
    FROM channel_messages WHERE channel = ?1 AND id > ?2 ORDER BY id ASC LIMIT ?3";

const INCREMENTAL_VACUUM_PAGES: i64 = 128;
const WAL_TRUNCATE_DELETED_ROWS_THRESHOLD: usize = 1024;

fn storage_err(e: impl std::fmt::Display) -> ChannelError {
    ChannelError::Storage(e.to_string())
}

/// Hard replay budgets. Byte limits count UTF-8 channel, type and serialized data bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SqliteReplayLimits {
    pub max_messages_per_stream: u64,
    pub max_bytes_per_stream: u64,
    pub max_messages: u64,
    pub max_bytes: u64,
    /// Zero disables this age cap, not the count or byte budgets.
    pub max_age_ms: u64,
}

impl Default for SqliteReplayLimits {
    fn default() -> Self {
        Self {
            max_messages_per_stream: 10_000,
            max_bytes_per_stream: 16 * 1024 * 1024,
            max_messages: 100_000,
            max_bytes: 64 * 1024 * 1024,
            max_age_ms: 24 * 60 * 60 * 1000,
        }
    }
}

impl SqliteReplayLimits {
    fn validate(self) -> Result<Self, ChannelError> {
        let bounds = [
            self.max_messages_per_stream,
            self.max_bytes_per_stream,
            self.max_messages,
            self.max_bytes,
        ];
        if bounds
            .iter()
            .any(|value| *value == 0 || *value > i64::MAX as u64)
            || self.max_age_ms > i64::MAX as u64
        {
            return Err(ChannelError::BadRequest(
                "replay count/byte limits must be positive SQLite integers".into(),
            ));
        }
        Ok(self)
    }
}

pub(super) struct SqliteChannelStore {
    pub(crate) conn: Mutex<rusqlite::Connection>,
    epoch: [u8; 16],
}

impl SqliteChannelStore {
    pub(super) fn open(
        path: &Path,
        limits: Option<SqliteReplayLimits>,
    ) -> Result<Self, ChannelError> {
        let limits = limits.map(SqliteReplayLimits::validate).transpose()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ChannelError::Storage(format!("create channel dir: {e}")))?;
        }
        let mut conn = tako_sqlite::open_local(path).map_err(storage_err)?;
        let epoch = init_connection(&mut conn, limits)?;
        Ok(Self {
            epoch,
            conn: Mutex::new(conn),
        })
    }

    pub(super) fn open_in_memory() -> Result<Self, ChannelError> {
        let mut conn = tako_sqlite::open_in_memory().map_err(storage_err)?;
        let epoch = init_connection(&mut conn, None)?;
        Ok(Self {
            epoch,
            conn: Mutex::new(conn),
        })
    }

    pub(super) fn publish(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
        ephemeral: bool,
    ) -> Result<(ChannelMessage, ChannelEvent), ChannelError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let sequence: i64 = tx.query_row(
            "UPDATE channel_event_sequence SET sequence = sequence + 1
             WHERE singleton = 1 AND sequence < 9223372036854775807 RETURNING sequence",
            [],
            |row| row.get(0),
        )?;
        let event = ChannelEvent {
            id: ChannelEventId {
                origin: URL_SAFE_NO_PAD.encode(self.epoch),
                sequence: sequence as u64,
            },
            channel: channel.into(),
            payload: payload.clone(),
            ephemeral,
        };
        let message = append_event(&tx, self.epoch, &event)?;
        tx.commit()?;
        Ok((message, event))
    }

    pub(super) fn ingest_remote(
        &self,
        event: &ChannelEvent,
    ) -> Result<Option<(ChannelMessage, bool)>, ChannelError> {
        if event.id.sequence == 0
            || event.id.sequence > i64::MAX as u64
            || event.id.origin.is_empty()
            || event.id.origin.len() > 128
            || !event
                .id
                .origin
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(ChannelError::BadRequest(
                "invalid channel event identity".into(),
            ));
        }
        if event.id.origin == URL_SAFE_NO_PAD.encode(self.epoch) {
            return Ok(None);
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let previous: i64 = tx
            .query_row(
                "SELECT sequence FROM channel_remote_origins WHERE origin = ?1",
                [&event.id.origin],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let sequence = event.id.sequence as i64;
        if sequence <= previous {
            return Ok(None);
        }
        let message = append_event(&tx, self.epoch, event)?;
        tx.execute(
            "INSERT INTO channel_remote_origins(origin, sequence) VALUES (?1, ?2)
            ON CONFLICT(origin) DO UPDATE SET sequence = excluded.sequence",
            rusqlite::params![event.id.origin, sequence],
        )?;
        tx.commit()?;
        Ok(Some((message, sequence > previous + 1)))
    }

    pub(super) fn read_after(
        &self,
        channel: &str,
        after: Option<ReplayCursor>,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        prune_age(&tx, load_limits(&tx)?, now_unix_ms())?;
        if let Some(cursor) = after {
            validate_cursor(&tx, self.epoch, channel, cursor)?;
        }
        let rows = {
            let mut stmt = tx.prepare_cached(READ_AFTER_SQL)?;
            stmt.query_map(
                rusqlite::params![
                    channel,
                    after.map_or(0, |cursor| cursor.position),
                    i64::from(limit)
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?
        };
        tx.commit()?;
        rows.into_iter()
            .map(|row| {
                let cursor = ReplayCursor::sqlite(self.epoch, channel, row.0);
                let mut message = channel_message_from_row((row.0, row.1, row.2, row.3))?;
                message.event_id = row
                    .4
                    .map(|json| serde_json::from_str(&json))
                    .transpose()
                    .map_err(storage_err)?;
                message.id = cursor.to_string();
                Ok(message)
            })
            .collect()
    }

    pub(super) fn replay_cursor(
        &self,
        channel: &str,
        requested: Option<ReplayCursor>,
    ) -> Result<Option<ReplayCursor>, ChannelError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        prune_age(&tx, load_limits(&tx)?, now_unix_ms())?;
        let cursor = if let Some(cursor) = requested {
            validate_cursor(&tx, self.epoch, channel, cursor)?;
            cursor
        } else {
            ReplayCursor::sqlite(self.epoch, channel, stream_head(&tx, channel)?)
        };
        tx.commit()?;
        Ok(Some(cursor))
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

    pub(super) fn prune(&self) -> Result<(), ChannelError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_unix_ms();
        tx.execute("DELETE FROM presence WHERE expires_at <= ?1", [now])?;
        let deleted = prune_replay(&tx, load_limits(&tx)?, now)?;
        tx.commit()?;
        if deleted > 0 {
            run_cleanup_maintenance(&conn, deleted);
        }
        Ok(())
    }
}

fn append_event(
    tx: &rusqlite::Connection,
    epoch: [u8; 16],
    event: &ChannelEvent,
) -> Result<ChannelMessage, ChannelError> {
    let mut message = ChannelMessage {
        id: String::new(),
        event_id: Some(event.id.clone()),
        channel: event.channel.clone(),
        r#type: event.payload.r#type.clone(),
        data: event.payload.data.clone(),
    };
    if event.ephemeral {
        return Ok(message);
    }
    let data_json = serde_json::to_string(&event.payload.data).map_err(storage_err)?;
    let limits = load_limits(tx)?;
    let bytes = event
        .channel
        .len()
        .saturating_add(event.payload.r#type.len())
        .saturating_add(data_json.len()) as u64;
    if bytes > limits.max_bytes || bytes > limits.max_bytes_per_stream {
        return Err(ChannelError::BadRequest(
            "message exceeds SQLite replay byte budget".into(),
        ));
    }
    let now = now_unix_ms();
    tx.execute(
        "INSERT INTO channel_metadata (channel, replay_window_ms, inactivity_ttl_ms, keepalive_interval_ms, max_connection_lifetime_ms, last_activity_unix_ms)
         VALUES (?1, 600000, 0, 25000, 7200000, ?2)
         ON CONFLICT(channel) DO UPDATE SET last_activity_unix_ms = excluded.last_activity_unix_ms", rusqlite::params![event.channel, now])?;
    tx.execute("INSERT INTO channel_messages (channel, type, data_json, created_at_unix_ms) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![event.channel, event.payload.r#type, data_json, now])?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO channel_message_events(message_id, event_json) VALUES (?1, ?2)",
        rusqlite::params![id, serde_json::to_string(&event.id).map_err(storage_err)?],
    )?;
    tx.execute(
        "UPDATE channel_metadata SET latest_message_id = ?2 WHERE channel = ?1",
        rusqlite::params![event.channel, id],
    )?;
    prune_replay(tx, limits, now)?;
    message.id = ReplayCursor::sqlite(epoch, &event.channel, id).to_string();
    Ok(message)
}

fn load_limits(conn: &rusqlite::Connection) -> Result<SqliteReplayLimits, ChannelError> {
    let json: String = conn.query_row(
        "SELECT limits_json FROM channel_store_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    serde_json::from_str::<SqliteReplayLimits>(&json)
        .map_err(storage_err)?
        .validate()
}

fn stream_head(conn: &rusqlite::Connection, channel: &str) -> Result<i64, ChannelError> {
    Ok(conn
        .query_row(
            "SELECT latest_message_id FROM channel_metadata WHERE channel = ?1",
            [channel],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

fn validate_cursor(
    conn: &rusqlite::Connection,
    epoch: [u8; 16],
    channel: &str,
    cursor: ReplayCursor,
) -> Result<(), ChannelError> {
    if cursor.scope != ReplayCursor::sqlite(epoch, channel, 0).scope {
        return Err(ChannelError::StaleCursor);
    }
    let floor: i64 = conn
        .query_row(
            "SELECT floor FROM channel_replay_floor WHERE channel = ?1",
            [channel],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if cursor.position < floor || cursor.position > stream_head(conn, channel)? {
        return Err(ChannelError::StaleCursor);
    }
    Ok(())
}

fn prune_age(
    conn: &rusqlite::Connection,
    limits: SqliteReplayLimits,
    now: i64,
) -> Result<usize, ChannelError> {
    Ok(conn.execute(
        "DELETE FROM channel_messages WHERE (?2 > 0 AND created_at_unix_ms < ?1 - ?2) OR EXISTS (
            SELECT 1 FROM channel_metadata m WHERE m.channel = channel_messages.channel AND (
                (m.replay_window_ms > 0 AND channel_messages.created_at_unix_ms < ?1 - m.replay_window_ms)
                OR (m.inactivity_ttl_ms > 0 AND m.last_activity_unix_ms < ?1 - m.inactivity_ttl_ms)
            )
        )", rusqlite::params![now, limits.max_age_ms as i64])?)
}

fn prune_replay(
    conn: &rusqlite::Connection,
    limits: SqliteReplayLimits,
    now: i64,
) -> Result<usize, ChannelError> {
    let mut deleted = prune_age(conn, limits, now)?;
    deleted += conn.execute(
        "DELETE FROM channel_messages WHERE id IN (
            SELECT id FROM (
                SELECT id, ROW_NUMBER() OVER (PARTITION BY channel ORDER BY id DESC) AS count,
                    SUM(length(CAST(channel AS BLOB)) + length(CAST(type AS BLOB)) + length(CAST(data_json AS BLOB)))
                        OVER (PARTITION BY channel ORDER BY id DESC) AS bytes
                FROM channel_messages
            ) WHERE count > ?1 OR bytes > ?2
        )", rusqlite::params![limits.max_messages_per_stream as i64, limits.max_bytes_per_stream as i64])?;
    deleted += conn.execute(
        "DELETE FROM channel_messages WHERE id IN (
            SELECT id FROM (
                SELECT id, ROW_NUMBER() OVER (ORDER BY id DESC) AS count,
                    SUM(length(CAST(channel AS BLOB)) + length(CAST(type AS BLOB)) + length(CAST(data_json AS BLOB)))
                        OVER (ORDER BY id DESC) AS bytes
                FROM channel_messages
            ) WHERE count > ?1 OR bytes > ?2
        )", rusqlite::params![limits.max_messages as i64, limits.max_bytes as i64])?;
    Ok(deleted)
}

fn init_connection(
    conn: &mut rusqlite::Connection,
    limits: Option<SqliteReplayLimits>,
) -> Result<[u8; 16], ChannelError> {
    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let conn = &tx;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS presence (
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
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS channel_store_state (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1), epoch BLOB NOT NULL, limits_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS channel_event_sequence (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), sequence INTEGER NOT NULL);
         INSERT INTO channel_event_sequence VALUES (1, 0) ON CONFLICT DO NOTHING;
         CREATE TABLE IF NOT EXISTS channel_message_events (message_id INTEGER PRIMARY KEY, event_json TEXT NOT NULL);
         CREATE TRIGGER IF NOT EXISTS channel_event_deleted AFTER DELETE ON channel_messages BEGIN
            DELETE FROM channel_message_events WHERE message_id = OLD.id;
         END;
         CREATE TABLE IF NOT EXISTS channel_remote_origins (origin TEXT PRIMARY KEY, sequence INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS channel_replay_floor (channel TEXT PRIMARY KEY, floor INTEGER NOT NULL);
         CREATE TRIGGER IF NOT EXISTS channel_replay_deleted AFTER DELETE ON channel_messages BEGIN
            INSERT INTO channel_replay_floor(channel, floor) VALUES (OLD.channel, OLD.id)
            ON CONFLICT(channel) DO UPDATE SET floor = MAX(floor, excluded.floor);
         END;
         UPDATE channel_metadata SET latest_message_id = MAX(latest_message_id, COALESCE(
            (SELECT MAX(id) FROM channel_messages WHERE channel_messages.channel = channel_metadata.channel), 0));"
    )?;
    let mut epoch = [0u8; 16];
    getrandom::fill(&mut epoch).map_err(storage_err)?;
    conn.execute("INSERT INTO channel_store_state(singleton, epoch, limits_json) VALUES(1, ?1, ?2) ON CONFLICT DO NOTHING",
        rusqlite::params![epoch.as_slice(), serde_json::to_string(&SqliteReplayLimits::default()).map_err(storage_err)?])?;
    if let Some(limits) = limits {
        conn.execute(
            "UPDATE channel_store_state SET limits_json = ?1 WHERE singleton = 1",
            [serde_json::to_string(&limits).map_err(storage_err)?],
        )?;
    }
    prune_replay(conn, load_limits(conn)?, now_unix_ms())?;
    let saved: Vec<u8> = conn.query_row(
        "SELECT epoch FROM channel_store_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let epoch = saved
        .try_into()
        .map_err(|_| storage_err("invalid channel epoch"))?;
    tx.commit()?;
    Ok(epoch)
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

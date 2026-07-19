use parking_lot::Mutex;
use std::path::Path;

pub(crate) use tako_sqlite::block_on;
use tako_sqlite::commit_or_rollback;

use crate::{ChannelAuthResponse, ChannelError, ChannelMessage, ChannelPublishPayload};

use super::{channel_message_from_row, now_unix_ms};

const WAL_TRUNCATE_DELETED_ROWS_THRESHOLD: usize = 1024;

fn storage_err(e: impl std::fmt::Display) -> ChannelError {
    ChannelError::Storage(e.to_string())
}

/// Per app/environment SQLite-backed channel store (turso engine).
///
/// The connection is opened once and reused; every operation locks a
/// mutex and uses the cached connection. Callers should hold a single
/// `ChannelStore` for each DB path and share it across requests (e.g.
/// behind an `Arc`): constructing a new `ChannelStore` reruns pragmas
/// and schema init on every call.
pub(super) struct SqliteChannelStore {
    pub(crate) conn: Mutex<turso::Connection>,
}

impl SqliteChannelStore {
    pub(super) fn open(path: &Path) -> Result<Self, ChannelError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ChannelError::Storage(format!("create channel dir: {e}")))?;
        }
        let path = path
            .to_str()
            .ok_or_else(|| ChannelError::Storage("non-UTF-8 channel db path".into()))?;
        let conn = block_on(async {
            let conn = tako_sqlite::open_local(path).await?;
            init_connection(&conn).await?;
            Ok::<_, ChannelError>(conn)
        })?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub(super) fn open_in_memory() -> Result<Self, ChannelError> {
        let conn = block_on(async {
            let conn = tako_sqlite::open_in_memory().await?;
            init_connection(&conn).await?;
            Ok::<_, ChannelError>(conn)
        })?;
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
        let id = block_on(async {
            let tx = conn.transaction().await.map_err(storage_err)?;
            let result = async {
                tx.execute(
                    "UPDATE channel_metadata SET last_activity_unix_ms = ?2 WHERE channel = ?1",
                    (channel, now_unix_ms()),
                )
                .await?;
                tx.execute(
                    "INSERT INTO channel_messages (channel, type, data_json) VALUES (?1, ?2, ?3)",
                    (channel, payload.r#type.as_str(), data_json.as_str()),
                )
                .await?;
                Ok::<_, ChannelError>(tx.last_insert_rowid())
            }
            .await;
            commit_or_rollback(tx, result).await
        })?;

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
        let conn = self.conn.lock();
        let rows = block_on(async {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT id, channel, type, data_json
                     FROM channel_messages
                     WHERE channel = ?1 AND (?2 IS NULL OR id > ?2)
                     ORDER BY id ASC
                     LIMIT ?3",
                )
                .await
                .map_err(storage_err)?;
            let mut rows = stmt
                .query((channel, after, i64::from(limit)))
                .await
                .map_err(storage_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().await.map_err(storage_err)? {
                out.push((
                    row.get::<i64>(0).map_err(storage_err)?,
                    row.get::<String>(1).map_err(storage_err)?,
                    row.get::<String>(2).map_err(storage_err)?,
                    row.get::<String>(3).map_err(storage_err)?,
                ));
            }
            Ok::<_, ChannelError>(out)
        })?;

        rows.into_iter().map(channel_message_from_row).collect()
    }

    pub(super) fn replay_cursor(
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

    pub(super) fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        let conn = self.conn.lock();
        let now = now_unix_ms();
        block_on(async {
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
                (
                    channel,
                    auth.replay_window_ms as i64,
                    auth.inactivity_ttl_ms as i64,
                    auth.keepalive_interval_ms as i64,
                    auth.max_connection_lifetime_ms as i64,
                    now,
                ),
            )
            .await
            .map_err(storage_err)?;

            let mut deleted_rows = 0usize;

            if auth.replay_window_ms > 0 {
                let cutoff = now - auth.replay_window_ms as i64;
                deleted_rows += conn
                    .execute(
                        "DELETE FROM channel_messages WHERE channel = ?1 AND created_at_unix_ms < ?2",
                        (channel, cutoff),
                    )
                    .await
                    .map_err(storage_err)? as usize;
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
                    (now,),
                )
                .await
                .map_err(storage_err)? as usize;
            deleted_rows += conn
                .execute(
                    "DELETE FROM channel_metadata
                     WHERE inactivity_ttl_ms > 0
                       AND last_activity_unix_ms < (?1 - inactivity_ttl_ms)",
                    (now,),
                )
                .await
                .map_err(storage_err)? as usize;

            if deleted_rows >= WAL_TRUNCATE_DELETED_ROWS_THRESHOLD {
                // Best-effort WAL reset. Turso has no incremental_vacuum, so
                // freed pages stay on the freelist and the main DB file keeps
                // its high-water size; only the WAL is truncated here. The
                // pragma only runs through query(), not execute().
                if let Ok(mut rows) = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await {
                    let _ = rows.next().await;
                }
            }

            Ok(())
        })
    }
}

fn message_id(
    conn: &turso::Connection,
    channel: &str,
    aggregate: &str,
) -> Result<Option<i64>, ChannelError> {
    let sql = format!("SELECT {aggregate}(id) FROM channel_messages WHERE channel = ?1");
    block_on(async {
        let mut rows = conn.query(&sql, (channel,)).await.map_err(storage_err)?;
        let row = rows
            .next()
            .await
            .map_err(storage_err)?
            .ok_or_else(|| storage_err("aggregate query returned no row"))?;
        row.get::<Option<i64>>(0).map_err(storage_err)
    })
}

async fn init_connection(conn: &turso::Connection) -> Result<(), ChannelError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS channel_messages (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             channel TEXT NOT NULL,
             type TEXT NOT NULL,
             data_json TEXT NOT NULL,
             created_at_unix_ms INTEGER NOT NULL DEFAULT (unixepoch() * 1000)
         );",
    )
    .await
    .map_err(storage_err)?;

    ensure_channel_metadata_schema(conn).await?;

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_channel_messages_channel_id
         ON channel_messages(channel, id);",
    )
    .await
    .map_err(storage_err)?;
    Ok(())
}

async fn ensure_channel_metadata_schema(conn: &turso::Connection) -> Result<(), ChannelError> {
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'channel_metadata'",
            (),
        )
        .await
        .map_err(storage_err)?;
    let exists: i64 = rows
        .next()
        .await
        .map_err(storage_err)?
        .ok_or_else(|| storage_err("sqlite_master count returned no row"))?
        .get(0)
        .map_err(storage_err)?;

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
        .await
        .map_err(storage_err)?;
        return Ok(());
    }

    let mut columns = Vec::new();
    let mut rows = conn
        .query("PRAGMA table_info(channel_metadata)", ())
        .await
        .map_err(storage_err)?;
    while let Some(row) = rows.next().await.map_err(storage_err)? {
        columns.push(row.get::<String>(1).map_err(storage_err)?);
    }

    if columns.iter().any(|column| column == "retention_ms")
        && !columns.iter().any(|column| column == "replay_window_ms")
    {
        conn.execute_batch(
            "ALTER TABLE channel_metadata RENAME COLUMN retention_ms TO replay_window_ms;",
        )
        .await
        .map_err(storage_err)?;
    }

    Ok(())
}

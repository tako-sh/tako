use parking_lot::Mutex;
use postgres::{Client, NoTls};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{ChannelAuthResponse, ChannelError, ChannelMessage, ChannelPublishPayload};

const CHANNELS_DB_FILENAME: &str = "channels.sqlite";
const INCREMENTAL_VACUUM_PAGES: i64 = 128;
const WAL_TRUNCATE_DELETED_ROWS_THRESHOLD: usize = 1024;
pub const POSTGRES_CHANNELS_SCHEMA: &str = "tako_channels";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelStoreConfig {
    Sqlite {
        path: PathBuf,
    },
    Postgres {
        url: String,
        schema: String,
        app_id: String,
    },
}

impl ChannelStoreConfig {
    pub fn sqlite(path: impl Into<PathBuf>) -> Self {
        Self::Sqlite { path: path.into() }
    }

    pub fn postgres(url: impl Into<String>, app_id: impl Into<String>) -> Self {
        Self::Postgres {
            url: url.into(),
            schema: POSTGRES_CHANNELS_SCHEMA.to_string(),
            app_id: app_id.into(),
        }
    }
}

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

pub struct ChannelStore {
    backend: ChannelStoreBackend,
}

enum ChannelStoreBackend {
    Sqlite(SqliteChannelStore),
    Postgres(PostgresChannelStore),
}

/// Per app/environment SQLite-backed channel store.
///
/// The connection is opened once and reused; every operation locks a
/// mutex and uses the cached connection. Callers should hold a single
/// `ChannelStore` for each DB path and share it across requests (e.g.
/// behind an `Arc`): constructing a new `ChannelStore` reruns pragmas
/// and schema init on every call.
struct SqliteChannelStore {
    pub(crate) conn: Mutex<rusqlite::Connection>,
}

struct PostgresChannelStore {
    client: Mutex<Client>,
    schema: String,
    app_id: String,
}

impl ChannelStore {
    pub fn open_config(config: ChannelStoreConfig) -> Result<Self, ChannelError> {
        match config {
            ChannelStoreConfig::Sqlite { path } => Self::open_sqlite(&path),
            ChannelStoreConfig::Postgres {
                url,
                schema,
                app_id,
            } => Self::open_postgres_with_schema(&url, &schema, &app_id),
        }
    }

    /// Open (or create) the channel DB at `path` and run the idempotent
    /// schema init. Safe to call repeatedly against the same path because
    /// SQLite supports multiple connections per file, but callers are
    /// expected to hold the returned store for the process's lifetime.
    pub fn open(path: &Path) -> Result<Self, ChannelError> {
        Self::open_sqlite(path)
    }

    pub fn open_sqlite(path: &Path) -> Result<Self, ChannelError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ChannelError::Storage(format!("create channel dir: {e}")))?;
        }
        let conn =
            rusqlite::Connection::open(path).map_err(|e| ChannelError::Storage(e.to_string()))?;
        init_connection(&conn)?;
        Ok(Self {
            backend: ChannelStoreBackend::Sqlite(SqliteChannelStore {
                conn: Mutex::new(conn),
            }),
        })
    }

    pub fn open_postgres(url: &str, app_id: &str) -> Result<Self, ChannelError> {
        Self::open_config(ChannelStoreConfig::postgres(url, app_id))
    }

    pub fn open_postgres_with_schema(
        url: &str,
        schema: &str,
        app_id: &str,
    ) -> Result<Self, ChannelError> {
        validate_pg_identifier(schema)?;
        let mut client =
            Client::connect(url, NoTls).map_err(|e| ChannelError::Storage(e.to_string()))?;
        init_postgres(&mut client, schema)?;
        Ok(Self {
            backend: ChannelStoreBackend::Postgres(PostgresChannelStore {
                client: Mutex::new(client),
                schema: schema.to_string(),
                app_id: app_id.to_string(),
            }),
        })
    }

    /// Open an in-memory channel DB. Used by local dev where replay only
    /// needs to survive reconnects within the current daemon process.
    pub fn open_in_memory() -> Result<Self, ChannelError> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        init_connection(&conn)?;
        Ok(Self {
            backend: ChannelStoreBackend::Sqlite(SqliteChannelStore {
                conn: Mutex::new(conn),
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn sqlite_conn(&self) -> parking_lot::MutexGuard<'_, rusqlite::Connection> {
        match &self.backend {
            ChannelStoreBackend::Sqlite(store) => store.conn.lock(),
            ChannelStoreBackend::Postgres(_) => {
                panic!("sqlite connection requested for postgres channel store")
            }
        }
    }

    pub fn append(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
    ) -> Result<ChannelMessage, ChannelError> {
        match &self.backend {
            ChannelStoreBackend::Sqlite(store) => append_sqlite(store, channel, payload),
            ChannelStoreBackend::Postgres(store) => append_postgres(store, channel, payload),
        }
    }

    pub fn read_after(
        &self,
        channel: &str,
        after: Option<i64>,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        match &self.backend {
            ChannelStoreBackend::Sqlite(store) => read_after_sqlite(store, channel, after, limit),
            ChannelStoreBackend::Postgres(store) => {
                read_after_postgres(store, channel, after, limit)
            }
        }
    }

    pub fn replay_cursor(
        &self,
        channel: &str,
        requested: Option<i64>,
    ) -> Result<Option<i64>, ChannelError> {
        match &self.backend {
            ChannelStoreBackend::Sqlite(store) => replay_cursor_sqlite(store, channel, requested),
            ChannelStoreBackend::Postgres(store) => {
                replay_cursor_postgres(store, channel, requested)
            }
        }
    }

    pub fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        match &self.backend {
            ChannelStoreBackend::Sqlite(store) => sync_channel_sqlite(store, channel, auth),
            ChannelStoreBackend::Postgres(store) => sync_channel_postgres(store, channel, auth),
        }
    }
}

fn append_sqlite(
    store: &SqliteChannelStore,
    channel: &str,
    payload: &ChannelPublishPayload,
) -> Result<ChannelMessage, ChannelError> {
    let data_json = serde_json::to_string(&payload.data)
        .map_err(|e| ChannelError::BadRequest(format!("serialize payload: {e}")))?;
    let mut conn = store.conn.lock();
    let tx = conn
        .transaction()
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    {
        let mut stmt = tx
            .prepare_cached(
                "UPDATE channel_metadata SET last_activity_unix_ms = ?2 WHERE channel = ?1",
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        stmt.execute(rusqlite::params![channel, now_unix_ms()])
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
    }
    {
        let mut stmt = tx
            .prepare_cached(
                "INSERT INTO channel_messages (channel, type, data_json) VALUES (?1, ?2, ?3)",
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        stmt.execute(rusqlite::params![channel, payload.r#type, data_json])
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
    }

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

fn read_after_sqlite(
    store: &SqliteChannelStore,
    channel: &str,
    after: Option<i64>,
    limit: u32,
) -> Result<Vec<ChannelMessage>, ChannelError> {
    let rows = {
        let conn = store.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT id, channel, type, data_json
                 FROM channel_messages
                 WHERE channel = ?1 AND (?2 IS NULL OR id > ?2)
                 ORDER BY id ASC
                 LIMIT ?3",
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![channel, after, i64::from(limit)], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| ChannelError::Storage(e.to_string()))?
    };

    rows.into_iter().map(channel_message_from_row).collect()
}

fn replay_cursor_sqlite(
    store: &SqliteChannelStore,
    channel: &str,
    requested: Option<i64>,
) -> Result<Option<i64>, ChannelError> {
    let conn = store.conn.lock();
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

fn sync_channel_sqlite(
    store: &SqliteChannelStore,
    channel: &str,
    auth: &ChannelAuthResponse,
) -> Result<(), ChannelError> {
    let conn = store.conn.lock();
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

fn append_postgres(
    store: &PostgresChannelStore,
    channel: &str,
    payload: &ChannelPublishPayload,
) -> Result<ChannelMessage, ChannelError> {
    let data_json = serde_json::to_string(&payload.data)
        .map_err(|e| ChannelError::BadRequest(format!("serialize payload: {e}")))?;
    let mut client = store.client.lock();
    let mut tx = client
        .transaction()
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    tx.execute(
        &format!(
            "UPDATE {}.channel_metadata
             SET last_activity_unix_ms = $3
             WHERE app_id = $1 AND channel = $2",
            store.schema
        ),
        &[&store.app_id, &channel, &now_unix_ms()],
    )
    .map_err(|e| ChannelError::Storage(e.to_string()))?;
    let row = tx
        .query_one(
            &format!(
                "INSERT INTO {}.channel_messages (app_id, channel, type, data_json)
                 VALUES ($1, $2, $3, $4)
                 RETURNING id",
                store.schema
            ),
            &[&store.app_id, &channel, &payload.r#type, &data_json],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    let id: i64 = row.get(0);
    tx.commit()
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

    Ok(ChannelMessage {
        id: id.to_string(),
        channel: channel.to_string(),
        r#type: payload.r#type.clone(),
        data: payload.data.clone(),
    })
}

fn read_after_postgres(
    store: &PostgresChannelStore,
    channel: &str,
    after: Option<i64>,
    limit: u32,
) -> Result<Vec<ChannelMessage>, ChannelError> {
    let mut client = store.client.lock();
    let rows = client
        .query(
            &format!(
                "SELECT id, channel, type, data_json
                 FROM {}.channel_messages
                 WHERE app_id = $1 AND channel = $2 AND ($3::BIGINT IS NULL OR id > $3)
                 ORDER BY id ASC
                 LIMIT $4",
                store.schema
            ),
            &[&store.app_id, &channel, &after, &i64::from(limit)],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

    rows.into_iter()
        .map(|row| {
            channel_message_from_row((
                row.get::<_, i64>(0),
                row.get::<_, String>(1),
                row.get::<_, String>(2),
                row.get::<_, String>(3),
            ))
        })
        .collect()
}

fn replay_cursor_postgres(
    store: &PostgresChannelStore,
    channel: &str,
    requested: Option<i64>,
) -> Result<Option<i64>, ChannelError> {
    let mut client = store.client.lock();
    let latest = postgres_message_id(&mut client, store, channel, "MAX")?;
    let Some(requested) = requested else {
        return Ok(latest);
    };

    let Some(oldest) = postgres_message_id(&mut client, store, channel, "MIN")? else {
        return Ok(Some(requested));
    };

    if requested < oldest.saturating_sub(1) {
        return Err(ChannelError::StaleCursor);
    }

    Ok(Some(requested))
}

fn sync_channel_postgres(
    store: &PostgresChannelStore,
    channel: &str,
    auth: &ChannelAuthResponse,
) -> Result<(), ChannelError> {
    let mut client = store.client.lock();
    let now = now_unix_ms();
    client
        .execute(
            &format!(
                "INSERT INTO {}.channel_metadata (
                    app_id,
                    channel,
                    replay_window_ms,
                    inactivity_ttl_ms,
                    keepalive_interval_ms,
                    max_connection_lifetime_ms,
                    last_activity_unix_ms
                ) VALUES ($1, $2, $3, $4, $5, $6, $7)
                ON CONFLICT(app_id, channel) DO UPDATE SET
                    replay_window_ms = excluded.replay_window_ms,
                    inactivity_ttl_ms = excluded.inactivity_ttl_ms,
                    keepalive_interval_ms = excluded.keepalive_interval_ms,
                    max_connection_lifetime_ms = excluded.max_connection_lifetime_ms,
                    last_activity_unix_ms = excluded.last_activity_unix_ms",
                store.schema
            ),
            &[
                &store.app_id,
                &channel,
                &(auth.replay_window_ms as i64),
                &(auth.inactivity_ttl_ms as i64),
                &(auth.keepalive_interval_ms as i64),
                &(auth.max_connection_lifetime_ms as i64),
                &now,
            ],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

    if auth.replay_window_ms > 0 {
        let cutoff = now - auth.replay_window_ms as i64;
        client
            .execute(
                &format!(
                    "DELETE FROM {}.channel_messages
                     WHERE app_id = $1 AND channel = $2 AND created_at_unix_ms < $3",
                    store.schema
                ),
                &[&store.app_id, &channel, &cutoff],
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
    }

    client
        .execute(
            &format!(
                "DELETE FROM {}.channel_messages
                 WHERE app_id = $1
                   AND channel IN (
                    SELECT channel
                    FROM {}.channel_metadata
                    WHERE app_id = $1
                      AND inactivity_ttl_ms > 0
                      AND last_activity_unix_ms < ($2 - inactivity_ttl_ms)
                 )",
                store.schema, store.schema
            ),
            &[&store.app_id, &now],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    client
        .execute(
            &format!(
                "DELETE FROM {}.channel_metadata
                 WHERE app_id = $1
                   AND inactivity_ttl_ms > 0
                   AND last_activity_unix_ms < ($2 - inactivity_ttl_ms)",
                store.schema
            ),
            &[&store.app_id, &now],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

    Ok(())
}

fn channel_message_from_row(
    row: (i64, String, String, String),
) -> Result<ChannelMessage, ChannelError> {
    let (id, channel, r#type, data_json) = row;
    let data =
        serde_json::from_str(&data_json).map_err(|e| ChannelError::Storage(e.to_string()))?;
    Ok(ChannelMessage {
        id: id.to_string(),
        channel,
        r#type,
        data,
    })
}

fn postgres_message_id(
    client: &mut Client,
    store: &PostgresChannelStore,
    channel: &str,
    aggregate: &str,
) -> Result<Option<i64>, ChannelError> {
    let sql = format!(
        "SELECT {aggregate}(id) FROM {}.channel_messages WHERE app_id = $1 AND channel = $2",
        store.schema
    );
    client
        .query_one(&sql, &[&store.app_id, &channel])
        .map(|row| row.get(0))
        .map_err(|e| ChannelError::Storage(e.to_string()))
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

fn init_postgres(client: &mut Client, schema: &str) -> Result<(), ChannelError> {
    client
        .batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS {schema};
             CREATE TABLE IF NOT EXISTS {schema}.channel_messages (
                 id BIGSERIAL PRIMARY KEY,
                 app_id TEXT NOT NULL,
                 channel TEXT NOT NULL,
                 type TEXT NOT NULL,
                 data_json TEXT NOT NULL,
                 created_at_unix_ms BIGINT NOT NULL DEFAULT ((extract(epoch from clock_timestamp()) * 1000)::BIGINT)
             );
             CREATE INDEX IF NOT EXISTS idx_channel_messages_app_channel_id
               ON {schema}.channel_messages(app_id, channel, id);
             CREATE TABLE IF NOT EXISTS {schema}.channel_metadata (
                 app_id TEXT NOT NULL,
                 channel TEXT NOT NULL,
                 replay_window_ms BIGINT NOT NULL,
                 inactivity_ttl_ms BIGINT NOT NULL,
                 keepalive_interval_ms BIGINT NOT NULL,
                 max_connection_lifetime_ms BIGINT NOT NULL,
                 last_activity_unix_ms BIGINT NOT NULL,
                 PRIMARY KEY(app_id, channel)
             );"
        ))
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    Ok(())
}

fn validate_pg_identifier(identifier: &str) -> Result<(), ChannelError> {
    let mut chars = identifier.chars();
    let Some(first) = chars.next() else {
        return Err(ChannelError::Storage(
            "postgres schema name cannot be empty".to_string(),
        ));
    };
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    {
        return Err(ChannelError::Storage(format!(
            "invalid postgres schema name '{identifier}'"
        )));
    }
    Ok(())
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

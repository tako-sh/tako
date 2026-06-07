mod postgres;
mod sqlite;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{ChannelAuthResponse, ChannelError, ChannelMessage, ChannelPublishPayload};
use postgres::PostgresChannelStore;
use sqlite::SqliteChannelStore;

const CHANNELS_DB_FILENAME: &str = "channels.sqlite";
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

pub struct ChannelStore {
    backend: ChannelStoreBackend,
}

enum ChannelStoreBackend {
    Sqlite(SqliteChannelStore),
    Postgres(Box<PostgresChannelStore>),
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
        Ok(Self {
            backend: ChannelStoreBackend::Sqlite(SqliteChannelStore::open(path)?),
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
        Ok(Self {
            backend: ChannelStoreBackend::Postgres(Box::new(PostgresChannelStore::open(
                url, schema, app_id,
            )?)),
        })
    }

    /// Open an in-memory channel DB. Used by local dev where replay only
    /// needs to survive reconnects within the current daemon process.
    pub fn open_in_memory() -> Result<Self, ChannelError> {
        Ok(Self {
            backend: ChannelStoreBackend::Sqlite(SqliteChannelStore::open_in_memory()?),
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
            ChannelStoreBackend::Sqlite(store) => store.append(channel, payload),
            ChannelStoreBackend::Postgres(store) => store.append(channel, payload),
        }
    }

    pub fn read_after(
        &self,
        channel: &str,
        after: Option<i64>,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        match &self.backend {
            ChannelStoreBackend::Sqlite(store) => store.read_after(channel, after, limit),
            ChannelStoreBackend::Postgres(store) => store.read_after(channel, after, limit),
        }
    }

    pub fn replay_cursor(
        &self,
        channel: &str,
        requested: Option<i64>,
    ) -> Result<Option<i64>, ChannelError> {
        match &self.backend {
            ChannelStoreBackend::Sqlite(store) => store.replay_cursor(channel, requested),
            ChannelStoreBackend::Postgres(store) => store.replay_cursor(channel, requested),
        }
    }

    pub fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        match &self.backend {
            ChannelStoreBackend::Sqlite(store) => store.sync_channel(channel, auth),
            ChannelStoreBackend::Postgres(store) => store.sync_channel(channel, auth),
        }
    }
}

pub(super) fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub(super) fn channel_message_from_row(
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

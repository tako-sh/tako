mod postgres;
mod presence;
mod sqlite;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    ChannelAuthResponse, ChannelError, ChannelMessage, ChannelPublishPayload, ReplayCursor,
};
use postgres::PostgresChannelStore;
use sqlite::SqliteChannelStore;
pub use sqlite::SqliteReplayLimits;

const CHANNELS_DB_FILENAME: &str = "channels.sqlite";
pub const POSTGRES_CHANNELS_SCHEMA: &str = "tako_channels";
use crate::mesh::DeliveryState;
use crate::{ChannelEvent, ChannelPublication, RemoteIngest};

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
    backend: Arc<ChannelStoreBackend>,
    delivery: Arc<DeliveryState>,
    _lifetime: Arc<()>,
}

enum ChannelStoreBackend {
    Sqlite(SqliteChannelStore),
    Postgres(Box<PostgresChannelStore>),
}

impl ChannelStoreBackend {
    fn prune(&self) -> Result<(), ChannelError> {
        match self {
            Self::Sqlite(store) => store.prune(),
            Self::Postgres(store) => store.prune(),
        }
    }
}

impl ChannelStore {
    fn from_backend(
        backend: ChannelStoreBackend,
        delivery: Arc<DeliveryState>,
    ) -> Result<Self, ChannelError> {
        let backend = Arc::new(backend);
        let changes = delivery.changes.clone();
        let lifetime = Arc::new(());
        let weak = Arc::downgrade(&lifetime);
        let worker_backend = backend.clone();
        let sender = changes.clone();
        let poll_remote = matches!(backend.as_ref(), ChannelStoreBackend::Postgres(_));
        std::thread::Builder::new()
            .name("channel-store".into())
            .spawn(move || {
                let mut next_prune = std::time::Instant::now();
                loop {
                    if weak.strong_count() == 0 {
                        break;
                    }
                    let backend = &worker_backend;
                    if std::time::Instant::now() >= next_prune {
                        if let Err(error) = backend.prune() {
                            tracing::warn!(%error, "channel replay cleanup failed");
                        }
                        next_prune = std::time::Instant::now() + std::time::Duration::from_secs(1);
                    }
                    let subscribed_channels = {
                        let mut streams = sender.lock();
                        streams.retain(|_, changes| changes.receiver_count() > 0);
                        streams.keys().cloned().collect::<Vec<_>>()
                    };
                    if let ChannelStoreBackend::Postgres(store) = backend.as_ref()
                        && !subscribed_channels.is_empty()
                    {
                        match store.latest_ids(&subscribed_channels) {
                            Ok(heads) => {
                                let streams = sender.lock();
                                for (channel, latest) in heads {
                                    if let Some(changes) = streams.get(&channel) {
                                        changes.send_if_modified(|seen| {
                                            if seen.is_some_and(|seen| seen >= latest) {
                                                false
                                            } else {
                                                *seen = Some(latest);
                                                true
                                            }
                                        });
                                    }
                                }
                            }
                            Err(error) => tracing::warn!(%error, "channel change poll failed"),
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(if poll_remote {
                        100
                    } else {
                        1000
                    }));
                }
            })
            .map_err(|error| ChannelError::Storage(error.to_string()))?;
        // The worker owns the last backend reference when the store is dropped,
        // keeping synchronous Postgres client shutdown off async executor threads.
        Ok(Self {
            backend,
            delivery,
            _lifetime: lifetime,
        })
    }

    /// Subscribe before reading replay so changes racing with the read remain visible.
    pub fn changes(&self, channel: &str) -> tokio::sync::watch::Receiver<Option<i64>> {
        self.delivery
            .changes
            .lock()
            .entry(channel.to_owned())
            .or_insert_with(|| tokio::sync::watch::channel(None).0)
            .subscribe()
    }

    /// Execute synchronous database work on Tokio's blocking pool.
    pub async fn run<T: Send + 'static>(
        self: &Arc<Self>,
        operation: impl FnOnce(&Self) -> Result<T, ChannelError> + Send + 'static,
    ) -> Result<T, ChannelError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || operation(&store))
            .await
            .map_err(|error| ChannelError::Storage(error.to_string()))?
    }

    pub async fn read_after_async(
        self: &Arc<Self>,
        channel: &str,
        after: Option<ReplayCursor>,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        let channel = channel.to_owned();
        self.run(move |store| store.read_after(&channel, after, limit))
            .await
    }

    pub async fn replay_cursor_async(
        self: &Arc<Self>,
        channel: &str,
        after: Option<ReplayCursor>,
    ) -> Result<Option<ReplayCursor>, ChannelError> {
        let channel = channel.to_owned();
        self.run(move |store| store.replay_cursor(&channel, after))
            .await
    }

    pub async fn sync_channel_async(
        self: &Arc<Self>,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        let channel = channel.to_owned();
        let auth = auth.clone();
        self.run(move |store| store.sync_channel(&channel, &auth))
            .await
    }

    pub async fn append_async(
        self: &Arc<Self>,
        channel: &str,
        payload: &ChannelPublishPayload,
    ) -> Result<ChannelMessage, ChannelError> {
        let channel = channel.to_owned();
        let payload = payload.clone();
        self.run(move |store| store.append(&channel, &payload))
            .await
    }

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
        Self::open_sqlite_config(path, None)
    }

    pub fn open_sqlite_with_limits(
        path: &Path,
        limits: SqliteReplayLimits,
    ) -> Result<Self, ChannelError> {
        Self::open_sqlite_config(path, Some(limits))
    }

    fn open_sqlite_config(
        path: &Path,
        limits: Option<SqliteReplayLimits>,
    ) -> Result<Self, ChannelError> {
        let sqlite = SqliteChannelStore::open(path, limits)?;
        let path = std::fs::canonicalize(path).map_err(|e| ChannelError::Storage(e.to_string()))?;
        Self::from_backend(
            ChannelStoreBackend::Sqlite(sqlite),
            DeliveryState::for_path(path),
        )
    }

    pub fn open_postgres(url: &str, app_id: &str) -> Result<Self, ChannelError> {
        Self::open_config(ChannelStoreConfig::postgres(url, app_id))
    }

    pub fn open_postgres_with_schema(
        url: &str,
        schema: &str,
        app_id: &str,
    ) -> Result<Self, ChannelError> {
        Self::from_backend(
            ChannelStoreBackend::Postgres(Box::new(PostgresChannelStore::open(
                url, schema, app_id,
            )?)),
            Arc::default(),
        )
    }

    /// Open an in-memory channel DB. Used by local dev where replay only
    /// needs to survive reconnects within the current daemon process.
    pub fn open_in_memory() -> Result<Self, ChannelError> {
        Self::from_backend(
            ChannelStoreBackend::Sqlite(SqliteChannelStore::open_in_memory()?),
            Arc::default(),
        )
    }

    #[cfg(test)]
    pub(crate) fn sqlite_conn(&self) -> parking_lot::MutexGuard<'_, rusqlite::Connection> {
        match self.backend.as_ref() {
            ChannelStoreBackend::Sqlite(store) => store.conn.lock(),
            ChannelStoreBackend::Postgres(_) => {
                panic!("sqlite connection requested for postgres channel store")
            }
        }
    }

    /// Test-only raw SQL escape hatches against the sqlite backend.
    #[cfg(test)]
    pub(crate) fn raw_execute(&self, sql: &str, params: impl rusqlite::Params) {
        let conn = self.sqlite_conn();
        conn.execute(sql, params).expect("raw execute");
    }

    #[cfg(test)]
    pub(crate) fn raw_query_i64(&self, sql: &str, params: impl rusqlite::Params) -> i64 {
        let conn = self.sqlite_conn();
        conn.query_row(sql, params, |row| row.get(0))
            .expect("raw query i64")
    }

    #[cfg(test)]
    pub(crate) fn raw_query_string(&self, sql: &str, params: impl rusqlite::Params) -> String {
        let conn = self.sqlite_conn();
        conn.query_row(sql, params, |row| row.get(0))
            .expect("raw query string")
    }

    pub fn ensure_ephemeral_supported(&self) -> Result<(), ChannelError> {
        if matches!(self.backend.as_ref(), ChannelStoreBackend::Postgres(_)) {
            return Err(crate::ephemeral::unsupported());
        }
        Ok(())
    }

    pub fn subscribe_ephemeral(
        &self,
        channel: &str,
    ) -> Result<crate::ephemeral::EphemeralSubscription, ChannelError> {
        self.ensure_ephemeral_supported()?;
        Ok(self
            .delivery
            .ephemeral
            .subscribe(channel, self.resets(channel)))
    }

    pub fn publish_ephemeral(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
    ) -> Result<ChannelMessage, ChannelError> {
        self.publish(channel, payload, true)
    }

    pub fn append(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
    ) -> Result<ChannelMessage, ChannelError> {
        self.publish(channel, payload, false)
    }

    fn publish(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
        ephemeral: bool,
    ) -> Result<ChannelMessage, ChannelError> {
        let _publication = self.delivery.publish.lock();
        let (message, publication) = match self.backend.as_ref() {
            ChannelStoreBackend::Sqlite(store) => {
                let (message, event) = store.publish(channel, payload, ephemeral)?;
                (message, ChannelPublication::Event(event))
            }
            ChannelStoreBackend::Postgres(store) => {
                if ephemeral {
                    return Err(crate::ephemeral::unsupported());
                }
                (
                    store.append(channel, payload)?,
                    ChannelPublication::Wake {
                        channel: channel.into(),
                    },
                )
            }
        };
        self.deliver(&message, ephemeral);
        let _ = self.delivery.publications.send(publication);
        Ok(message)
    }

    fn deliver(&self, message: &ChannelMessage, ephemeral: bool) {
        if ephemeral {
            self.delivery.ephemeral.deliver(message);
        }
        self.delivery.wake(
            &message.channel,
            message
                .id
                .parse::<ReplayCursor>()
                .ok()
                .map(|cursor| cursor.position),
        );
    }

    /// Subscribe once per app before accepting local publications. Lag requires peer reset.
    pub fn publications(&self) -> tokio::sync::broadcast::Receiver<ChannelPublication> {
        self.delivery.publications.subscribe()
    }

    /// Ingest authenticated, app-scoped events in FIFO order per origin. Never republishes.
    pub fn ingest_remote(&self, event: &ChannelEvent) -> Result<RemoteIngest, ChannelError> {
        let _publication = self.delivery.publish.lock();
        let ChannelStoreBackend::Sqlite(store) = self.backend.as_ref() else {
            return Err(ChannelError::BadRequest(
                "PostgreSQL delivery accepts wake hints, not remote events".into(),
            ));
        };
        let Some((message, gap)) = store.ingest_remote(event)? else {
            return Ok(RemoteIngest::Duplicate);
        };
        if gap {
            self.signal_gap(None);
        }
        self.deliver(&message, event.ephemeral);
        Ok(RemoteIngest::Applied)
    }

    /// Wake a stream after a PostgreSQL peer commit without inserting another row.
    pub fn wake(&self, channel: &str) {
        self.delivery.wake(channel, None);
    }

    /// Subscribe before capturing a replay head. Reset generations are out of band.
    pub fn resets(&self, channel: &str) -> tokio::sync::watch::Receiver<u64> {
        let mut resets = self.delivery.resets.lock();
        resets.retain(|_, sender| sender.receiver_count() > 0);
        resets
            .entry(channel.into())
            .or_insert_with(|| tokio::sync::watch::channel(0).0)
            .subscribe()
    }

    /// Signal transport loss or lag. None resets all currently connected streams.
    pub fn signal_gap(&self, channel: Option<&str>) {
        let mut resets = self.delivery.resets.lock();
        resets.retain(|_, sender| sender.receiver_count() > 0);
        for (stream, sender) in resets.iter() {
            if channel.is_none_or(|channel| channel == stream) {
                sender.send_modify(|generation| *generation = generation.wrapping_add(1));
            }
        }
    }

    pub fn read_after(
        &self,
        channel: &str,
        after: Option<ReplayCursor>,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        match self.backend.as_ref() {
            ChannelStoreBackend::Sqlite(store) => store.read_after(channel, after, limit),
            ChannelStoreBackend::Postgres(store) => store.read_after(
                channel,
                after.map(ReplayCursor::pg_position).transpose()?,
                limit,
            ),
        }
    }

    pub fn replay_cursor(
        &self,
        channel: &str,
        requested: Option<ReplayCursor>,
    ) -> Result<Option<ReplayCursor>, ChannelError> {
        match self.backend.as_ref() {
            ChannelStoreBackend::Sqlite(store) => store.replay_cursor(channel, requested),
            ChannelStoreBackend::Postgres(store) => store
                .replay_cursor(
                    channel,
                    requested.map(ReplayCursor::pg_position).transpose()?,
                )
                .map(|cursor| cursor.map(ReplayCursor::postgres)),
        }
    }

    pub fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        if auth.ephemeral {
            return Ok(());
        }
        match self.backend.as_ref() {
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

pub(super) fn resolve_replay_cursor(
    requested: Option<i64>,
    oldest: Option<i64>,
    latest: Option<i64>,
) -> Result<Option<i64>, ChannelError> {
    let Some(requested) = requested else {
        return Ok(latest);
    };
    let floor = oldest.map(|id| id.saturating_sub(1)).or(latest);
    if floor.is_none_or(|floor| requested < floor) || latest.is_none_or(|head| requested > head) {
        return Err(ChannelError::StaleCursor);
    }
    Ok(Some(requested))
}

pub(super) fn channel_message_from_row(
    row: (i64, String, String, String),
) -> Result<ChannelMessage, ChannelError> {
    let (id, channel, r#type, data_json) = row;
    let data =
        serde_json::from_str(&data_json).map_err(|e| ChannelError::Storage(e.to_string()))?;
    Ok(ChannelMessage {
        id: id.to_string(),
        event_id: None,
        channel,
        r#type,
        data,
    })
}

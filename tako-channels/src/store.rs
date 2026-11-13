mod postgres;
mod presence;
mod sqlite;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::stream::close_payload;
use crate::{
    CLOSE_MESSAGE_TYPE, ChannelAuthResponse, ChannelError, ChannelLifecycle, ChannelMessage,
    ChannelPublishPayload, ReplayCursor, ReplayFrom,
};
use postgres::PostgresChannelStore;
use sqlite::SqliteChannelStore;
pub use sqlite::SqliteReplayLimits;

const CHANNELS_DB_FILENAME: &str = "channels.sqlite";
/// A connection may outlive its lifetime by the time its last frames take.
const IDLE_STREAM_GRACE_MS: i64 = 60_000;
pub const POSTGRES_CHANNELS_SCHEMA: &str = "tako_channels";
/// Most streams one app keeps bookkeeping for. Each distinct params binding is
/// a stream, so without a bound a client choosing new values could grow the
/// metadata maintenance scans without limit. It matches the default total
/// replay budget (`SqliteReplayLimits::max_messages`): every stream holding a
/// message fits, and the rest is headroom for streams with subscribers only.
pub const MAX_STREAMS: i64 = 100_000;

/// A new stream refused by [`MAX_STREAMS`]. Logged here, where every path
/// that starts a stream (subscribe, publish, mesh ingest) meets it.
fn stream_limit_error(channel: &str) -> ChannelError {
    tracing::warn!(
        stream = channel,
        limit = MAX_STREAMS,
        "channel stream limit reached; refusing to start a new stream"
    );
    ChannelError::StreamLimit
}
use crate::mesh::{DeliveryState, notify_committed};
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
    read_only: bool,
}

enum ChannelStoreBackend {
    Sqlite(SqliteChannelStore),
    Postgres(Box<PostgresChannelStore>),
}

impl ChannelStoreBackend {
    fn prune(&self, now: i64) -> Result<(), ChannelError> {
        match self {
            Self::Sqlite(store) => store.prune(now),
            Self::Postgres(store) => store.prune(now),
        }
    }

    fn latest_ids(&self, channels: &[String]) -> Result<Vec<(String, i64)>, ChannelError> {
        match self {
            Self::Sqlite(store) => store.latest_ids(channels),
            Self::Postgres(store) => store.latest_ids(channels),
        }
    }
}

impl ChannelStore {
    fn from_backend(
        backend: ChannelStoreBackend,
        delivery: Arc<DeliveryState>,
    ) -> Result<Self, ChannelError> {
        Self::from_backend_mode(backend, delivery, false)
    }

    fn from_backend_mode(
        backend: ChannelStoreBackend,
        delivery: Arc<DeliveryState>,
        read_only: bool,
    ) -> Result<Self, ChannelError> {
        let backend = Arc::new(backend);
        let changes = delivery.changes.clone();
        let lifetime = Arc::new(());
        let weak = Arc::downgrade(&lifetime);
        let worker_backend = backend.clone();
        let sender = changes.clone();
        let poll_remote = matches!(backend.as_ref(), ChannelStoreBackend::Postgres(_));
        if read_only && !poll_remote {
            return Ok(Self {
                backend,
                delivery,
                _lifetime: lifetime,
                read_only,
            });
        }
        std::thread::Builder::new()
            .name("channel-store".into())
            .spawn(move || {
                let mut next_prune = std::time::Instant::now();
                loop {
                    if weak.strong_count() == 0 {
                        break;
                    }
                    let backend = &worker_backend;
                    if !read_only && std::time::Instant::now() >= next_prune {
                        if let Err(error) = backend.prune(now_unix_ms()) {
                            tracing::warn!(%error, "channel replay cleanup failed");
                        }
                        next_prune = std::time::Instant::now() + std::time::Duration::from_secs(1);
                    }
                    // Commits in this process wake subscribers directly. Polling
                    // covers commits by other writers of the same store: remote
                    // Postgres servers, or the previous server process sharing
                    // this SQLite file during a zero-downtime reload.
                    let subscribed_channels = {
                        let mut streams = sender.lock();
                        streams.retain(|_, changes| changes.receiver_count() > 0);
                        streams.keys().cloned().collect::<Vec<_>>()
                    };
                    if !subscribed_channels.is_empty() {
                        match backend.latest_ids(&subscribed_channels) {
                            Ok(heads) => {
                                let streams = sender.lock();
                                for (channel, latest) in heads {
                                    notify_committed(&streams, &channel, latest);
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
            read_only,
        })
    }

    fn ensure_writable(&self) -> Result<(), ChannelError> {
        if self.read_only {
            Err(ChannelError::Storage(
                "channel mutations require the owning broker".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// Subscribe before reading replay so changes racing with the read remain visible.
    pub fn changes(&self, channel: &str) -> tokio::sync::watch::Receiver<Option<i64>> {
        let mut changes = self.delivery.changes.lock();
        // Readers without a maintenance worker release finished streams here.
        changes.retain(|_, sender| sender.receiver_count() > 0);
        changes
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

    pub async fn close_marker_async(
        self: &Arc<Self>,
        channel: &str,
    ) -> Result<Option<ChannelMessage>, ChannelError> {
        let channel = channel.to_owned();
        self.run(move |store| store.close_marker(&channel)).await
    }

    /// Resolve where a reader starts. `None` means every retained message:
    /// retention may already have dropped the beginning, which is no reset.
    pub async fn replay_from_async(
        self: &Arc<Self>,
        channel: &str,
        from: ReplayFrom,
    ) -> Result<Option<ReplayCursor>, ChannelError> {
        match from {
            ReplayFrom::Head => self.replay_cursor_async(channel, None).await,
            ReplayFrom::After(cursor) => self.replay_cursor_async(channel, Some(cursor)).await,
            ReplayFrom::Start => Ok(None),
        }
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

    /// Open an existing broker-owned store without schema or replay mutations.
    pub fn open_reader_config(config: ChannelStoreConfig) -> Result<Self, ChannelError> {
        match config {
            ChannelStoreConfig::Sqlite { path } => {
                let sqlite = SqliteChannelStore::open_reader(&path)?;
                let path = std::fs::canonicalize(path)
                    .map_err(|e| ChannelError::Storage(e.to_string()))?;
                Self::from_backend_mode(
                    ChannelStoreBackend::Sqlite(sqlite),
                    DeliveryState::for_path(path),
                    true,
                )
            }
            ChannelStoreConfig::Postgres {
                url,
                schema,
                app_id,
            } => Self::from_backend_mode(
                ChannelStoreBackend::Postgres(Box::new(PostgresChannelStore::open_reader(
                    &url, &schema, &app_id,
                )?)),
                Arc::default(),
                true,
            ),
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

    /// Logical-clock histories drive maintenance themselves.
    #[cfg(test)]
    pub(crate) fn open_in_memory_without_maintenance() -> Self {
        Self {
            backend: Arc::new(ChannelStoreBackend::Sqlite(
                SqliteChannelStore::open_in_memory().unwrap(),
            )),
            delivery: Arc::default(),
            _lifetime: Arc::new(()),
            read_only: false,
        }
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
    pub(crate) fn prune_at(&self, now: i64) {
        self.backend.prune(now).unwrap();
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
        self.publish(channel, payload, true, None)
    }

    pub fn append(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
    ) -> Result<ChannelMessage, ChannelError> {
        self.publish(channel, payload, false, None)
    }

    /// Append to a durable stream and apply the lifecycle its definition
    /// declares in the same write, after the close check: a publish rejected
    /// because the stream is closed changes neither the stream's lifecycle
    /// nor its activity. Without a lifecycle this is [`Self::append`].
    pub fn append_with_lifecycle(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
        lifecycle: Option<&ChannelLifecycle>,
    ) -> Result<ChannelMessage, ChannelError> {
        self.publish(channel, payload, false, lifecycle)
    }

    /// Close a stream durably by appending its terminal marker. Later
    /// publishes fail with [`ChannelError::StreamClosed`] while the marker is
    /// retained. Closing a closed stream returns its existing marker.
    pub fn close(&self, channel: &str) -> Result<ChannelMessage, ChannelError> {
        self.append(channel, &close_payload())
    }

    /// Appending a close marker closes the stream; see [`Self::close`].
    fn publish(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
        ephemeral: bool,
        lifecycle: Option<&ChannelLifecycle>,
    ) -> Result<ChannelMessage, ChannelError> {
        self.ensure_writable()?;
        if ephemeral && payload.r#type == CLOSE_MESSAGE_TYPE {
            return Err(ChannelError::BadRequest(
                "ephemeral channel streams cannot be closed".into(),
            ));
        }
        let _publication = self.delivery.publish.lock();
        let (message, publication) = match self.backend.as_ref() {
            ChannelStoreBackend::Sqlite(store) => {
                let (message, event) = store.publish(channel, payload, ephemeral, lifecycle)?;
                // An already closed stream returns its marker; nothing new to deliver.
                let Some(event) = event else {
                    return Ok(message);
                };
                (message, ChannelPublication::Event(event))
            }
            ChannelStoreBackend::Postgres(store) => {
                if ephemeral {
                    return Err(crate::ephemeral::unsupported());
                }
                let (message, appended) = store.append(channel, payload, lifecycle)?;
                let message = scope_postgres_id(channel, message)?;
                if !appended {
                    return Ok(message);
                }
                (
                    message,
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

    /// Deliver an authenticated local broker notification after its commit.
    /// The broker owns persistence and FIFO ordering; this must not publish again.
    pub fn notify_committed(&self, message: &ChannelMessage, ephemeral: bool) {
        self.deliver(message, ephemeral);
    }

    /// Subscribe once per app before accepting local publications. Lag requires peer reset.
    pub fn publications(&self) -> tokio::sync::broadcast::Receiver<ChannelPublication> {
        self.delivery.publications.subscribe()
    }

    /// Ingest authenticated, app-scoped events in FIFO order per origin. Never
    /// republishes. An event its stream cannot take is acknowledged and
    /// dropped ([`RemoteIngest::Dropped`]), so it does not read as a gap.
    pub fn ingest_remote(&self, event: &ChannelEvent) -> Result<RemoteIngest, ChannelError> {
        self.ensure_writable()?;
        let _publication = self.delivery.publish.lock();
        let ChannelStoreBackend::Sqlite(store) = self.backend.as_ref() else {
            return Err(ChannelError::BadRequest(
                "PostgreSQL delivery accepts wake hints, not remote events".into(),
            ));
        };
        match store.ingest_remote(event, || self.signal_gap(None))? {
            sqlite::Ingested::Duplicate => Ok(RemoteIngest::Duplicate),
            sqlite::Ingested::Dropped => Ok(RemoteIngest::Dropped),
            sqlite::Ingested::StreamLimit => Ok(RemoteIngest::StreamLimit),
            sqlite::Ingested::Applied(message) => {
                self.deliver(&message, event.ephemeral);
                Ok(RemoteIngest::Applied)
            }
        }
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

    /// Invalidate SQLite replay after transport loss, including clients that reconnect later.
    /// The persisted event origin remains stable; PostgreSQL retains its shared history.
    pub fn invalidate_replay(&self) -> Result<(), ChannelError> {
        self.ensure_writable()?;
        let _publication = self.delivery.publish.lock();
        match self.backend.as_ref() {
            ChannelStoreBackend::Sqlite(store) => store.invalidate_replay(|| self.signal_gap(None)),
            ChannelStoreBackend::Postgres(_) => {
                self.signal_gap(None);
                Ok(())
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
            ChannelStoreBackend::Postgres(store) => store
                .read_after(
                    channel,
                    after
                        .map(|cursor| cursor.pg_position(channel))
                        .transpose()?,
                    limit,
                )?
                .into_iter()
                .map(|message| scope_postgres_id(channel, message))
                .collect(),
        }
    }

    /// The retained terminal marker of a closed stream. Messages a reader
    /// has not seen up to this marker are still delivered before it.
    pub fn close_marker(&self, channel: &str) -> Result<Option<ChannelMessage>, ChannelError> {
        match self.backend.as_ref() {
            ChannelStoreBackend::Sqlite(store) => store.close_marker(channel),
            ChannelStoreBackend::Postgres(store) => store
                .close_marker(channel)?
                .map(|message| scope_postgres_id(channel, message))
                .transpose(),
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
                    requested
                        .map(|cursor| cursor.pg_position(channel))
                        .transpose()?,
                )
                .map(|cursor| cursor.map(|position| ReplayCursor::postgres(channel, position))),
        }
    }

    pub fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        self.ensure_writable()?;
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    #[cfg(test)]
    let now = now + SERVER_CLOCK_SKEW_MS.get();
    now
}

#[cfg(test)]
thread_local! {
    /// How far this test thread's server clock runs ahead of the real one.
    pub(super) static SERVER_CLOCK_SKEW_MS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

pub(super) fn resolve_replay_cursor(
    requested: Option<i64>,
    highest_deleted: i64,
    latest: Option<i64>,
) -> Result<Option<i64>, ChannelError> {
    let Some(requested) = requested else {
        return Ok(latest);
    };
    if requested < highest_deleted || latest.is_none_or(|head| requested > head) {
        return Err(ChannelError::StaleCursor);
    }
    Ok(Some(requested))
}

/// PostgreSQL rows carry bare positions; clients get stream-bound cursors.
fn scope_postgres_id(
    channel: &str,
    mut message: ChannelMessage,
) -> Result<ChannelMessage, ChannelError> {
    let position = message
        .id
        .parse::<i64>()
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    message.id = ReplayCursor::postgres(channel, position).to_string();
    Ok(message)
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

#[cfg(test)]
mod ordering_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn gap_reset_precedes_remote_row_visibility_to_an_independent_reader() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("channels.sqlite");
        let store = Arc::new(ChannelStore::open(&path).unwrap());
        let reader = ChannelStore::open_reader_config(ChannelStoreConfig::sqlite(&path)).unwrap();
        let observer = rusqlite::Connection::open(&path).unwrap();
        let reset = reader.resets("a");
        let after = reader.replay_cursor("a", None).unwrap();
        let mut publications = reader.publications();
        let held = reset.borrow();
        let worker_store = store.clone();
        let worker = std::thread::spawn(move || {
            worker_store.ingest_remote(&ChannelEvent {
                id: crate::ChannelEventId {
                    origin: "remote".into(),
                    sequence: 2,
                },
                channel: "a".into(),
                payload: ChannelPublishPayload {
                    r#type: "event".into(),
                    data: 2.into(),
                },
                ephemeral: false,
                lifecycle: None,
                created_at_unix_ms: None,
            })
        });
        // The held watch borrow pauses signal_gap while it owns the reset map.
        let deadline = Instant::now() + Duration::from_secs(5);
        while store.delivery.resets.try_lock().is_some() {
            assert!(
                Instant::now() < deadline,
                "ingest did not reach reset publication"
            );
            std::thread::yield_now();
        }
        let visible: (i64, i64) = observer
            .query_row(
                "SELECT (SELECT COUNT(*) FROM channel_messages),
                    (SELECT COUNT(*) FROM channel_remote_origins)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(*held, 0);
        drop(held);
        assert_eq!(worker.join().unwrap().unwrap(), RemoteIngest::Applied);
        assert_eq!(
            visible,
            (0, 0),
            "row and dedup must stay hidden until reset"
        );
        assert!(matches!(
            reader.read_after("a", after, 10),
            Err(ChannelError::StaleCursor)
        ));
        let messages = reader.read_after("a", None, 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(reset.has_changed().unwrap());
        assert_eq!(messages[0].event_id.as_ref().unwrap().sequence, 2);
        assert!(publications.try_recv().is_err());
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;

    #[test]
    fn fanout_state_is_released_with_the_last_subscriber_of_a_stream() {
        // Readers of a broker-owned database run no maintenance worker.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("channels.sqlite");
        let _writer = ChannelStore::open(&path).unwrap();
        let reader = ChannelStore::open_reader_config(ChannelStoreConfig::sqlite(&path)).unwrap();
        let streams: Vec<crate::ChannelStream> = (0..100)
            .map(|n| crate::ChannelStream::new("chat", &serde_json::json!({ "room": n })))
            .collect();
        let held = (reader.changes("kept"), reader.resets("kept"));
        for stream in &streams {
            let _subscriber = (
                reader.changes(stream.key()),
                reader.resets(stream.key()),
                reader.subscribe_ephemeral(stream.key()).unwrap(),
            );
        }
        // The next subscription releases every stream nobody listens to.
        let _next = (reader.changes("next"), reader.resets("next"));
        assert_eq!(reader.delivery.changes.lock().len(), 2);
        assert_eq!(reader.delivery.resets.lock().len(), 2);
        drop(held);
    }

    #[test]
    fn delayed_poll_or_publisher_cannot_regress_the_committed_head() {
        let store = ChannelStore::open_in_memory_without_maintenance();
        let mut receiver = store.changes("events");
        store.delivery.wake("events", Some(20));
        assert_eq!(*receiver.borrow_and_update(), Some(20));
        store.delivery.wake("events", Some(10));
        store.delivery.wake("events", Some(20));
        assert!(!receiver.has_changed().unwrap());
        assert_eq!(*receiver.borrow(), Some(20));
        store.delivery.wake("events", Some(21));
        assert!(receiver.has_changed().unwrap());
        for _ in 0..2 {
            assert_eq!(*receiver.borrow_and_update(), Some(21));
            store.wake("events");
            assert!(receiver.has_changed().unwrap());
        }
    }
}

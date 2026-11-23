mod hosted;
mod presence;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::stream::close_payload;
use crate::{
    CLOSE_MESSAGE_TYPE, ChannelAuthResponse, ChannelError, ChannelLifecycle, ChannelMessage,
    ChannelPublishPayload, ReplayCursor, ReplayFrom,
};
use hosted::HostedChannelStore;

/// One hosted head query per poll tick; larger subscription sets span ticks.
const HOSTED_HEAD_BATCH: usize = 256;
/// A connection may outlive its lifetime by the time its last frames take.
const IDLE_STREAM_GRACE_MS: i64 = 60_000;
/// Most streams one app keeps bookkeeping for. Each distinct params binding is
/// a stream, so without a bound a client choosing new values could grow the
/// metadata maintenance scans without limit. Every stream holding a message
/// counts, and the rest is headroom for streams with subscribers only.
pub const MAX_STREAMS: i64 = 100_000;

/// A new stream refused by [`MAX_STREAMS`]. Logged here, where every path
/// that starts a stream (subscribe and publish) meets it.
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

/// An app's replay history in a hosted SQLite database, reached through a
/// server-issued binding.
#[derive(Clone, PartialEq, Eq)]
pub struct ChannelStoreConfig {
    pub url: String,
    pub token: String,
    pub app_id: String,
}

impl std::fmt::Debug for ChannelStoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelStoreConfig")
            .field("url", &"[REDACTED]")
            .field("token", &"[REDACTED]")
            .field("app_id", &self.app_id)
            .finish()
    }
}

impl ChannelStoreConfig {
    pub fn hosted(
        url: impl Into<String>,
        token: impl Into<String>,
        app_id: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
            app_id: app_id.into(),
        }
    }
}

pub struct ChannelStore {
    backend: Arc<HostedChannelStore>,
    delivery: Arc<DeliveryState>,
    presence: parking_lot::Mutex<presence::Leases>,
    _lifetime: Arc<()>,
    read_only: bool,
}

impl ChannelStore {
    fn from_backend(backend: HostedChannelStore, read_only: bool) -> Result<Self, ChannelError> {
        let backend = Arc::new(backend);
        let delivery = Arc::<DeliveryState>::default();
        let lifetime = Arc::new(());
        let weak = Arc::downgrade(&lifetime);
        let worker_backend = backend.clone();
        // Weak, so the worker never extends the delivery channels' lifetime.
        let worker_delivery = Arc::downgrade(&delivery);
        std::thread::Builder::new()
            .name("channel-store".into())
            .spawn(move || {
                let mut next_prune = std::time::Instant::now();
                let mut pending_channels = Vec::new();
                loop {
                    let (true, Some(worker_delivery)) =
                        (weak.strong_count() > 0, worker_delivery.upgrade())
                    else {
                        break;
                    };
                    let backend = &worker_backend;
                    if !read_only && std::time::Instant::now() >= next_prune {
                        if let Err(error) = backend.prune() {
                            tracing::warn!(%error, "channel replay cleanup failed");
                        }
                        next_prune = std::time::Instant::now() + std::time::Duration::from_secs(1);
                    }
                    // Commits in this process wake subscribers directly. Polling
                    // covers other processes and servers writing to the database.
                    if pending_channels.is_empty() {
                        // Snapshot a round so subscription churn cannot starve later batches.
                        let mut streams = worker_delivery.changes.lock();
                        streams.retain(|_, changes| changes.receiver_count() > 0);
                        pending_channels = streams.keys().cloned().collect();
                    }
                    if !pending_channels.is_empty() {
                        let start = pending_channels.len().saturating_sub(HOSTED_HEAD_BATCH);
                        match backend.latest_ids(&pending_channels[start..]) {
                            Ok((heads, replaced)) => {
                                if replaced {
                                    worker_delivery.replaced();
                                }
                                let streams = worker_delivery.changes.lock();
                                for (channel, latest) in heads {
                                    notify_committed(&streams, &channel, latest);
                                }
                            }
                            Err(error) => tracing::warn!(%error, "channel change poll failed"),
                        }
                        pending_channels.truncate(start);
                    }
                    drop(worker_delivery);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            })
            .map_err(|error| ChannelError::Storage(error.to_string()))?;
        // The worker owns the last backend reference when the store is dropped,
        // keeping blocking client shutdown off async executor threads.
        Ok(Self::assemble(backend, delivery, lifetime, read_only))
    }

    fn assemble(
        backend: Arc<HostedChannelStore>,
        delivery: Arc<DeliveryState>,
        lifetime: Arc<()>,
        read_only: bool,
    ) -> Self {
        Self {
            backend,
            delivery,
            presence: Default::default(),
            _lifetime: lifetime,
            read_only,
        }
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
        // Release finished streams here too, between maintenance polls.
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

    /// Open the app's replay history as its writer, creating the app's state
    /// in the database when it has none. Blocking: never call it on an async
    /// executor thread.
    pub fn open_config(config: ChannelStoreConfig) -> Result<Self, ChannelError> {
        Self::open_mode(config, false)
    }

    /// Open an existing broker-owned history without schema or replay mutations.
    pub fn open_reader_config(config: ChannelStoreConfig) -> Result<Self, ChannelError> {
        Self::open_mode(config, true)
    }

    fn open_mode(config: ChannelStoreConfig, read_only: bool) -> Result<Self, ChannelError> {
        Self::from_backend(
            HostedChannelStore::open(&config.url, &config.token, &config.app_id, read_only)?,
            read_only,
        )
    }

    /// A writer whose maintenance runs only when a test asks.
    #[cfg(test)]
    pub(crate) fn open_without_maintenance(config: ChannelStoreConfig) -> Self {
        let backend =
            HostedChannelStore::open(&config.url, &config.token, &config.app_id, false).unwrap();
        Self::assemble(Arc::new(backend), Arc::default(), Arc::new(()), false)
    }

    #[cfg(test)]
    pub(crate) fn prune(&self) {
        self.backend.prune().unwrap();
    }

    /// Streams the live hub still keeps fanout state for.
    #[cfg(test)]
    pub(crate) fn ephemeral_channels(&self) -> usize {
        self.delivery.ephemeral.channel_count()
    }

    pub fn subscribe_ephemeral(
        &self,
        channel: &str,
    ) -> Result<crate::ephemeral::EphemeralSubscription, ChannelError> {
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
        self.ensure_writable()?;
        if payload.r#type == CLOSE_MESSAGE_TYPE {
            return Err(ChannelError::BadRequest(
                "ephemeral channel streams cannot be closed".into(),
            ));
        }
        self.delivery.ephemeral.publish(channel, payload, |event| {
            let _ = self.delivery.ephemeral_publications.send(event);
        })
    }

    pub fn append(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
    ) -> Result<ChannelMessage, ChannelError> {
        self.publish(channel, payload, None)
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
        self.publish(channel, payload, lifecycle)
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
        lifecycle: Option<&ChannelLifecycle>,
    ) -> Result<ChannelMessage, ChannelError> {
        self.ensure_writable()?;
        let _publication = self.delivery.publish.lock();
        let (message, appended) = self.backend.publish(channel, payload, lifecycle)?;
        // Closing an already closed stream returns its marker without a commit.
        if appended {
            self.deliver(&message, false);
            let _ = self.delivery.publications.send(ChannelPublication::Wake {
                channel: channel.into(),
            });
        }
        Ok(message)
    }

    fn deliver(&self, message: &ChannelMessage, ephemeral: bool) {
        if ephemeral {
            self.delivery.ephemeral.deliver(message);
            return;
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

    /// Subscribe before local publication. Lag signals a gap: wake hints were lost.
    pub fn publications(&self) -> tokio::sync::broadcast::Receiver<ChannelPublication> {
        self.delivery.publications.subscribe()
    }

    /// Live publications have a separate queue; their loss never invalidates replay.
    pub fn ephemeral_publications(&self) -> tokio::sync::broadcast::Receiver<ChannelEvent> {
        self.delivery.ephemeral_publications.subscribe()
    }

    /// Ingest an authenticated, app-scoped live event from a peer, in FIFO
    /// order per origin. Never republishes. Durable history is shared through
    /// the database, so peers send wake hints for it, never events.
    pub fn ingest_remote(&self, event: &ChannelEvent) -> Result<RemoteIngest, ChannelError> {
        self.ensure_writable()?;
        if !event.ephemeral {
            return Err(ChannelError::BadRequest(
                "shared channel storage accepts wake hints, not durable remote events".into(),
            ));
        }
        self.delivery.ephemeral.ingest(event)
    }

    /// Wake a stream after a commit elsewhere without inserting another row.
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
        self.delivery.signal_gap(channel);
    }

    /// Fence live delivery after transport loss, including on reconnect. The
    /// database retains its history, so only live subscribers reload.
    pub fn invalidate_replay(&self) -> Result<(), ChannelError> {
        self.ensure_writable()?;
        let _publication = self.delivery.publish.lock();
        self.signal_gap(None);
        Ok(())
    }

    pub fn read_after(
        &self,
        channel: &str,
        after: Option<ReplayCursor>,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        self.backend.read_after(channel, after, limit)
    }

    /// The retained terminal marker of a closed stream. Messages a reader
    /// has not seen up to this marker are still delivered before it.
    pub fn close_marker(&self, channel: &str) -> Result<Option<ChannelMessage>, ChannelError> {
        self.backend.close_marker(channel)
    }

    pub fn replay_cursor(
        &self,
        channel: &str,
        requested: Option<ReplayCursor>,
    ) -> Result<Option<ReplayCursor>, ChannelError> {
        self.backend.replay_cursor(channel, requested)
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
        self.backend.sync_channel(channel, auth)
    }
}

pub(super) fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests;

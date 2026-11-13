use crate::{ChannelPublishPayload, ephemeral::EphemeralHub};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    path::PathBuf,
    sync::{Arc, OnceLock, Weak},
};
use tokio::sync::{broadcast, watch};

/// Identity allocated by the origin, independent of each peer's replay position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelEventId {
    pub origin: String,
    pub sequence: u64,
}

impl fmt::Display for ChannelEventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.origin, self.sequence)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelEvent {
    pub id: ChannelEventId,
    pub channel: String,
    pub payload: ChannelPublishPayload,
    pub ephemeral: bool,
    /// The lifecycle the origin's stream had after this durable event, so a
    /// peer retains the stream, and its close marker, as long as the origin.
    /// A peer without one keeps the stream's current lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<crate::ChannelLifecycle>,
    /// When the origin stored this durable event, in Unix milliseconds. Peers
    /// stamp their copy with it, so a message, and a close marker, expire at
    /// the same moment on every node whenever each one prunes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_unix_ms: Option<i64>,
}

/// Transport authenticates and app-scopes these publications before remote ingest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelPublication {
    Event(ChannelEvent),
    Wake { channel: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteIngest {
    Applied,
    Duplicate,
    /// Acknowledged in sequence but not stored: the stream was already
    /// closed. Nothing to deliver.
    Dropped,
    /// Acknowledged in sequence but not stored: the event would start a new
    /// stream and the app is at [`crate::MAX_STREAMS`]. This node's readers
    /// of that stream never receive it; the caller reports it.
    StreamLimit,
}

pub(crate) type StreamChanges = HashMap<String, watch::Sender<Option<i64>>>;

pub(crate) fn notify_committed(streams: &StreamChanges, channel: &str, latest: i64) {
    if let Some(changes) = streams.get(channel) {
        changes.send_if_modified(|seen| {
            if seen.is_none_or(|previous| latest > previous) {
                *seen = Some(latest);
                true
            } else {
                false
            }
        });
    }
}

pub(crate) struct DeliveryState {
    pub(crate) ephemeral: Arc<EphemeralHub>,
    pub(crate) changes: Arc<Mutex<StreamChanges>>,
    pub(crate) resets: Mutex<HashMap<String, watch::Sender<u64>>>,
    pub(crate) publications: broadcast::Sender<ChannelPublication>,
    pub(crate) ephemeral_publications: broadcast::Sender<ChannelEvent>,
    // Allocation, commit and publication must have the same order across handles.
    pub(crate) publish: Mutex<()>,
}

impl Default for DeliveryState {
    fn default() -> Self {
        Self {
            ephemeral: Arc::default(),
            changes: Arc::default(),
            resets: Mutex::default(),
            publications: broadcast::channel(256).0,
            ephemeral_publications: broadcast::channel(64).0,
            publish: Mutex::new(()),
        }
    }
}

impl DeliveryState {
    pub(crate) fn for_path(path: PathBuf) -> Arc<Self> {
        static STATES: OnceLock<Mutex<HashMap<PathBuf, Weak<DeliveryState>>>> = OnceLock::new();
        let mut states = STATES.get_or_init(Default::default).lock();
        states.retain(|_, state| state.strong_count() > 0);
        if let Some(state) = states.get(&path).and_then(Weak::upgrade) {
            return state;
        }
        let state = Arc::new(Self {
            ephemeral: EphemeralHub::for_path(path.clone()),
            ..Self::default()
        });
        states.insert(path, Arc::downgrade(&state));
        state
    }

    pub(crate) fn wake(&self, channel: &str, head: Option<i64>) {
        let streams = self.changes.lock();
        if let Some(head) = head {
            notify_committed(&streams, channel, head);
        } else if let Some(changes) = streams.get(channel) {
            changes.send_modify(|_| {});
        }
    }
}

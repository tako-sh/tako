use crate::{ChannelPublishPayload, ephemeral::EphemeralHub};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt, sync::Arc};
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
}

/// Transport authenticates and app-scopes these publications before remote
/// ingest. Peers share durable history through the replay database, so a
/// durable commit travels as a wake hint; only live events travel whole.
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
    pub(crate) fn signal_gap(&self, channel: Option<&str>) {
        let mut resets = self.resets.lock();
        resets.retain(|_, sender| sender.receiver_count() > 0);
        for (stream, sender) in resets.iter() {
            if channel.is_none_or(|channel| channel == stream) {
                sender.send_modify(|generation| *generation = generation.wrapping_add(1));
            }
        }
    }

    /// Positions restart under a replaced database's new epoch: forget the
    /// heads each stream has seen and make every live subscriber reload.
    pub(crate) fn replaced(&self) {
        for changes in self.changes.lock().values() {
            changes.send_replace(None);
        }
        self.signal_gap(None);
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

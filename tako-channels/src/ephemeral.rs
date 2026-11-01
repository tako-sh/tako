//! Process-local, bounded live fanout. Nothing is retained without a subscriber.
use crate::{ChannelAuthResponse, ChannelError, ChannelMessage, ChannelPublishPayload};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, OnceLock, Weak},
};
use tokio::sync::broadcast;

const CAPACITY: usize = 64;
#[derive(Default)]
pub(crate) struct EphemeralHub {
    channels: Mutex<HashMap<String, broadcast::Sender<Arc<ChannelMessage>>>>,
}

impl EphemeralHub {
    /// Publishing and proxy stores open separate connections to the same app DB.
    pub(crate) fn for_path(path: PathBuf) -> Arc<Self> {
        static HUBS: OnceLock<Mutex<HashMap<PathBuf, Weak<EphemeralHub>>>> = OnceLock::new();
        let mut hubs = HUBS.get_or_init(Default::default).lock();
        hubs.retain(|_, hub| hub.strong_count() > 0);
        if let Some(hub) = hubs.get(&path).and_then(Weak::upgrade) {
            return hub;
        }
        let hub = Arc::new(Self::default());
        hubs.insert(path, Arc::downgrade(&hub));
        hub
    }

    pub(crate) fn subscribe(self: &Arc<Self>, channel: &str) -> EphemeralSubscription {
        let receiver = self
            .channels
            .lock()
            .entry(channel.to_owned())
            .or_insert_with(|| broadcast::channel(CAPACITY).0)
            .subscribe();
        EphemeralSubscription {
            hub: self.clone(),
            channel: channel.to_owned(),
            receiver: Some(receiver),
        }
    }

    pub(crate) fn publish(&self, channel: &str, payload: &ChannelPublishPayload) -> ChannelMessage {
        let message = ChannelMessage {
            id: String::new(),
            channel: channel.to_owned(),
            r#type: payload.r#type.clone(),
            data: payload.data.clone(),
        };
        if let Some(sender) = self.channels.lock().get(channel) {
            let _ = sender.send(Arc::new(message.clone()));
        }
        message
    }
}

pub struct EphemeralSubscription {
    hub: Arc<EphemeralHub>,
    channel: String,
    receiver: Option<broadcast::Receiver<Arc<ChannelMessage>>>,
}

impl EphemeralSubscription {
    /// Slow subscribers skip evicted messages; reconnecting never replays events.
    pub async fn recv(&mut self) -> Option<Arc<ChannelMessage>> {
        loop {
            match self.receiver.as_mut()?.recv().await {
                Ok(message) => return Some(message),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

impl Drop for EphemeralSubscription {
    fn drop(&mut self) {
        let mut channels = self.hub.channels.lock();
        self.receiver.take();
        if channels
            .get(&self.channel)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            channels.remove(&self.channel);
        }
    }
}

/// Writes a live SSE stream after subscription has been established.
/// The caller writes response headers only after creating the subscription.
pub async fn serve_events(
    session: &mut pingora_proxy::Session,
    mut events: EphemeralSubscription,
    auth: &ChannelAuthResponse,
) -> pingora_core::Result<bool> {
    use bytes::Bytes;
    use std::time::Duration;
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(auth.max_connection_lifetime_ms.max(1));
    let interval = Duration::from_millis(auth.keepalive_interval_ms.max(1));
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                session.write_response_body(None, true).await?;
                return Ok(true);
            }
            message = events.recv() => {
                let Some(message) = message else { return Ok(true); };
                let frame = format!("data: {}\n\n", serde_json::to_string(&*message).expect("JSON channel message"));
                session.write_response_body(Some(Bytes::from(frame)), false).await?;
            }
            _ = tokio::time::sleep(interval) => {
                session.write_response_body(Some(Bytes::from_static(b": keepalive\n\n")), false).await?;
            }
        }
    }
}

pub(crate) fn unsupported() -> ChannelError {
    ChannelError::BadRequest(
        "ephemeral channels require a single server without shared Postgres".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelLifecycle, ChannelStore};
    use std::time::Duration;
    fn payload(n: i32) -> ChannelPublishPayload {
        ChannelPublishPayload {
            r#type: "progress".into(),
            data: serde_json::json!(n),
        }
    }

    #[tokio::test]
    async fn separate_stores_share_live_events_without_database_writes_or_replay() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("channels.sqlite");
        let writer = ChannelStore::open(&path).unwrap();
        let reader = ChannelStore::open(&path).unwrap();
        let auth = serde_json::from_value(serde_json::json!({"ok":true,"ephemeral":true})).unwrap();
        writer.sync_channel("live", &auth).unwrap();
        writer.publish_ephemeral("live", &payload(0)).unwrap();
        let mut first = reader.subscribe_ephemeral("live").unwrap();
        let mut second = writer.subscribe_ephemeral("live").unwrap();
        let mut unrelated = reader.subscribe_ephemeral("other").unwrap();
        let sent = writer.publish_ephemeral("live", &payload(1)).unwrap();
        assert_eq!(sent.id, "");
        for events in [&mut first, &mut second] {
            assert_eq!(
                *tokio::time::timeout(Duration::from_secs(1), events.recv())
                    .await
                    .unwrap()
                    .unwrap(),
                sent
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), unrelated.recv())
                .await
                .is_err()
        );
        assert!(reader.read_after("live", None, 100).unwrap().is_empty());
        assert_eq!(
            writer.raw_query_i64("SELECT COUNT(*) FROM channel_metadata", []),
            0
        );
        assert_eq!(
            writer.raw_query_i64("SELECT COUNT(*) FROM channel_messages", []),
            0
        );
        drop(first);
        drop(second);
        writer.publish_ephemeral("live", &payload(2)).unwrap();
        let mut reconnected = reader.subscribe_ephemeral("live").unwrap();
        writer.publish_ephemeral("live", &payload(3)).unwrap();
        assert_eq!(reconnected.recv().await.unwrap().data, 3);
        drop(reconnected);
        drop(unrelated);
        assert!(
            EphemeralHub::for_path(std::fs::canonicalize(&path).unwrap())
                .channels
                .lock()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn slow_subscribers_skip_evicted_events_and_apps_are_isolated() {
        let one = ChannelStore::open_in_memory().unwrap();
        let two = ChannelStore::open_in_memory().unwrap();
        let mut events = one.subscribe_ephemeral("live").unwrap();
        let mut other_app = two.subscribe_ephemeral("live").unwrap();
        for n in 0..100 {
            one.publish_ephemeral("live", &payload(n)).unwrap();
        }
        assert_eq!(events.recv().await.unwrap().data, 36);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), other_app.recv())
                .await
                .is_err()
        );
        one.append("durable", &payload(1)).unwrap();
        assert_eq!(one.read_after("durable", None, 10).unwrap().len(), 1);
    }

    #[test]
    fn lifecycle_rejects_explicit_replay_and_round_trips() {
        for replay in [0, 100, 600000] {
            let value = serde_json::json!({"ephemeral":true,"replayWindowMs":replay});
            assert!(serde_json::from_value::<ChannelLifecycle>(value.clone()).is_err());
            let mut auth = value;
            auth["ok"] = true.into();
            assert!(serde_json::from_value::<ChannelAuthResponse>(auth).is_err());
        }
        let lifecycle: ChannelLifecycle =
            serde_json::from_value(serde_json::json!({"ephemeral":true})).unwrap();
        let encoded = serde_json::to_value(&lifecycle).unwrap();
        assert!(encoded.get("replayWindowMs").is_none());
        assert_eq!(
            serde_json::from_value::<ChannelLifecycle>(encoded).unwrap(),
            lifecycle
        );
        assert!(
            !serde_json::from_value::<ChannelLifecycle>(serde_json::json!({}))
                .unwrap()
                .ephemeral
        );
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use crate::ChannelStore;
    use pingora_core::protocols::l4::stream::Stream;
    use pingora_proxy::Session;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn sse_delivers_live_data_without_replay_ids() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let store = Arc::new(ChannelStore::open_in_memory().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut session = Session::new_h1(Box::new(Stream::from(socket)));
            session.read_request().await.unwrap();
            let payload = ChannelPublishPayload {
                r#type: "progress".into(),
                data: 1.into(),
            };
            store.publish_ephemeral("live", &payload).unwrap();
            let events = store.subscribe_ephemeral("live").unwrap();
            let mut header = pingora_http::ResponseHeader::build(200, None).unwrap();
            header
                .insert_header("Content-Type", "text/event-stream")
                .unwrap();
            session
                .write_response_header(Box::new(header), false)
                .await
                .unwrap();
            store
                .publish_ephemeral(
                    "live",
                    &ChannelPublishPayload {
                        data: 2.into(),
                        ..payload
                    },
                )
                .unwrap();
            let auth = serde_json::from_value(
                serde_json::json!({"ok":true,"ephemeral":true,"maxConnectionLifetimeMs":100}),
            )
            .unwrap();
            serve_events(&mut session, events, &auth).await.unwrap();
        });
        let response = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .header("Last-Event-ID", "123")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.text().await.unwrap();
        assert_eq!(
            body,
            "data: {\"id\":\"\",\"channel\":\"live\",\"type\":\"progress\",\"data\":2}\n\n"
        );
        server.await.unwrap();
    }
}

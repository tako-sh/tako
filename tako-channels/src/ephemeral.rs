//! Process-local, bounded live fanout. Nothing is retained without a subscriber.
use crate::{ChannelAuthResponse, ChannelError, ChannelMessage, ChannelStream};
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

    pub(crate) fn subscribe(
        self: &Arc<Self>,
        channel: &str,
        resets: tokio::sync::watch::Receiver<u64>,
    ) -> EphemeralSubscription {
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
            resets,
        }
    }

    pub(crate) fn deliver(&self, message: &ChannelMessage) {
        if let Some(sender) = self.channels.lock().get(&message.channel) {
            let _ = sender.send(Arc::new(message.clone()));
        }
    }
}

pub struct EphemeralSubscription {
    hub: Arc<EphemeralHub>,
    channel: String,
    receiver: Option<broadcast::Receiver<Arc<ChannelMessage>>>,
    resets: tokio::sync::watch::Receiver<u64>,
}

impl EphemeralSubscription {
    /// Slow subscribers skip evicted messages; reconnecting never replays events.
    pub async fn recv(&mut self) -> Option<Arc<ChannelMessage>> {
        loop {
            let result = tokio::select! {
                biased;
                _ = self.resets.changed() => {
                    self.receiver.take();
                    return None;
                }
                message = self.receiver.as_mut()?.recv() => message,
            };
            match result {
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
/// `stream` is the stream `events` subscribed to; frames carry its public name.
pub async fn serve_events(
    session: &mut pingora_proxy::Session,
    mut events: EphemeralSubscription,
    auth: &ChannelAuthResponse,
    stream: &ChannelStream,
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
                let Some(message) = message else {
                    session.write_response_body(None, true).await?;
                    return Ok(true);
                };
                let message = stream.public((*message).clone());
                let frame = format!("data: {}\n\n", serde_json::to_string(&message).expect("JSON channel message"));
                session.write_response_body(Some(Bytes::from(frame)), false).await?;
            }
            _ = tokio::time::sleep(interval) => {
                session.write_response_body(Some(Bytes::from_static(b": keepalive\n\n")), false).await?;
            }
        }
    }
}

pub(crate) fn unsupported() -> ChannelError {
    ChannelError::BadRequest("ephemeral channels require SQLite channel storage".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelLifecycle, ChannelPublishPayload, ChannelStore};
    use std::time::Duration;
    fn payload(n: i32) -> ChannelPublishPayload {
        ChannelPublishPayload {
            r#type: "progress".into(),
            data: serde_json::json!(n),
        }
    }

    #[tokio::test]
    async fn separate_stores_share_live_events_without_replay_rows() {
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
    use crate::{ChannelPublishPayload, ChannelStore};
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
            let sent = store
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
            serve_events(
                &mut session,
                events,
                &auth,
                &ChannelStream::new("live", &serde_json::json!({})),
            )
            .await
            .unwrap();
            sent
        });
        let response = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .header("Last-Event-ID", "123")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.text().await.unwrap();
        let sent = server.await.unwrap();
        assert!(!body.lines().any(|line| line.starts_with("id:")));
        let data = body
            .strip_prefix("data: ")
            .unwrap()
            .strip_suffix("\n\n")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(data).unwrap(),
            serde_json::json!({"id":"", "eventId":sent.event_id, "channel":"live", "type":"progress", "data":2})
        );
        assert_eq!(sent.event_id.as_ref().unwrap().sequence, 2);
    }
}

#[cfg(test)]
mod duplex_tests {
    use super::*;
    use crate::{ChannelPublishPayload, ChannelStore};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn sse_preserves_event_identity_without_replay_over_duplex_transport() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (mut client, io) = tokio::io::duplex(16384);
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: local\r\nLast-Event-ID: 123\r\n\r\n")
                .await
                .unwrap();
            let mut session = pingora_proxy::Session::new_h1(Box::new(io));
            session.read_request().await.unwrap();
            let store = ChannelStore::open_in_memory().unwrap();
            let payload = ChannelPublishPayload {
                r#type: "progress".into(),
                data: 1.into(),
            };
            let old = store.publish_ephemeral("live", &payload).unwrap();
            let events = store.subscribe_ephemeral("live").unwrap();
            let sent = store
                .publish_ephemeral(
                    "live",
                    &ChannelPublishPayload {
                        data: 2.into(),
                        ..payload
                    },
                )
                .unwrap();
            let mut header = pingora_http::ResponseHeader::build(200, None).unwrap();
            header
                .insert_header("Content-Type", "text/event-stream")
                .unwrap();
            session
                .write_response_header(Box::new(header), false)
                .await
                .unwrap();
            let auth =
                serde_json::from_value(serde_json::json!({"ok":true,"ephemeral":true})).unwrap();
            let server = tokio::spawn(async move {
                serve_events(
                    &mut session,
                    events,
                    &auth,
                    &ChannelStream::new("live", &serde_json::json!({})),
                )
                .await
                .unwrap();
            });
            let mut client = BufReader::new(client);
            let mut response = String::new();
            loop {
                let mut line = String::new();
                assert!(
                    client.read_line(&mut line).await.unwrap() > 0,
                    "SSE ended before the event"
                );
                response.push_str(&line);
                if line.starts_with("data: ") {
                    break;
                }
            }
            store.signal_gap(Some("live"));
            client.read_to_string(&mut response).await.unwrap();
            server.await.unwrap();
            assert!(response.starts_with("HTTP/1.1 200"));
            assert!(!response.lines().any(|line| line.starts_with("id:")));
            let events: Vec<serde_json::Value> = response
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(|data| serde_json::from_str(data).unwrap())
                .collect();
            assert_eq!(
                events,
                vec![serde_json::json!({
                    "id":"", "eventId":{"origin":old.event_id.unwrap().origin,"sequence":2},
                    "channel":"live", "type":"progress", "data":2
                })]
            );
            assert_eq!(events[0], serde_json::to_value(sent).unwrap());
            assert!(store.read_after("live", None, 10).unwrap().is_empty());
            assert_eq!(
                store.raw_query_i64("SELECT COUNT(*) FROM channel_metadata", []),
                0
            );
        })
        .await
        .unwrap();
    }
}

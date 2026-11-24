//! Process-local, bounded live fanout. Nothing is retained without a subscriber.
use crate::{
    ChannelAuthResponse, ChannelError, ChannelEvent, ChannelEventId, ChannelMessage,
    ChannelPublishPayload, ChannelStream, RemoteIngest,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use parking_lot::Mutex;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::broadcast;

const CAPACITY: usize = 64;
// Deduplication is only a bounded live-delivery aid, never a durable receipt.
const MAX_ORIGINS: usize = 256;

#[derive(Default)]
struct Publications {
    origin: Option<String>,
    sequence: u64,
    received: std::collections::VecDeque<(String, u64)>,
}

#[derive(Default)]
pub(crate) struct EphemeralHub {
    channels: Mutex<HashMap<String, broadcast::Sender<Arc<ChannelMessage>>>>,
    publications: Mutex<Publications>,
}

impl EphemeralHub {
    /// Allocation, local delivery and enqueueing have one order, independent
    /// of the durable writer lock. A new hub gets a new incarnation.
    pub(crate) fn publish(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
        enqueue: impl FnOnce(ChannelEvent),
    ) -> Result<ChannelMessage, ChannelError> {
        let mut state = self.publications.lock();
        if state.origin.is_none() {
            let mut nonce = [0; 32];
            getrandom::fill(&mut nonce)
                .map_err(|error| ChannelError::Storage(error.to_string()))?;
            state.origin = Some(format!("live_{}", URL_SAFE_NO_PAD.encode(nonce)));
        }
        state.sequence = state
            .sequence
            .checked_add(1)
            .filter(|sequence| *sequence <= i64::MAX as u64)
            .ok_or_else(|| ChannelError::Storage("ephemeral sequence exhausted".into()))?;
        let event = ChannelEvent {
            id: ChannelEventId {
                origin: state.origin.clone().unwrap(),
                sequence: state.sequence,
            },
            channel: channel.into(),
            payload: payload.clone(),
            ephemeral: true,
        };
        let message = message(&event);
        self.deliver(&message);
        enqueue(event);
        Ok(message)
    }

    pub(crate) fn ingest(&self, event: &ChannelEvent) -> Result<RemoteIngest, ChannelError> {
        if !event.ephemeral
            || !event.id.origin.starts_with("live_")
            || event.id.origin.len() > 128
            || !event
                .id
                .origin
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            || event.id.sequence == 0
            || event.id.sequence > i64::MAX as u64
            || event.payload.r#type == crate::CLOSE_MESSAGE_TYPE
        {
            return Err(ChannelError::BadRequest("invalid ephemeral event".into()));
        }
        let mut state = self.publications.lock();
        if state.origin.as_ref() == Some(&event.id.origin) {
            return Ok(RemoteIngest::Duplicate);
        }
        if let Some(index) = state
            .received
            .iter()
            .position(|(origin, _)| origin == &event.id.origin)
        {
            if event.id.sequence <= state.received[index].1 {
                return Ok(RemoteIngest::Duplicate);
            }
            state.received.remove(index);
        }
        if state.received.len() == MAX_ORIGINS {
            state.received.pop_front();
        }
        state
            .received
            .push_back((event.id.origin.clone(), event.id.sequence));
        // A missing transient event neither rotates durable cursors nor replays.
        self.deliver(&message(event));
        Ok(RemoteIngest::Applied)
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

    #[cfg(test)]
    pub(crate) fn channel_count(&self) -> usize {
        self.channels.lock().len()
    }

    pub(crate) fn deliver(&self, message: &ChannelMessage) {
        if let Some(sender) = self.channels.lock().get(&message.channel) {
            let _ = sender.send(Arc::new(message.clone()));
        }
    }
}

pub fn message(event: &ChannelEvent) -> ChannelMessage {
    ChannelMessage {
        id: String::new(),
        event_id: Some(event.id.clone()),
        channel: event.channel.clone(),
        r#type: event.payload.r#type.clone(),
        data: event.payload.data.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_db::TestDb;
    use crate::{ChannelLifecycle, ChannelPublishPayload};
    use std::time::Duration;
    fn payload(n: i32) -> ChannelPublishPayload {
        ChannelPublishPayload {
            r#type: "progress".into(),
            data: serde_json::json!(n),
        }
    }

    #[tokio::test]
    async fn publishing_and_receiving_need_no_database_and_gaps_preserve_replay() {
        let db = TestDb::start();
        let (a, b) = (db.open_for("one").await, db.open_for("two").await);
        let durable = a.append_async("durable", &payload(0)).await.unwrap();
        let replay = durable.id.parse().ok();
        let resets = b.resets("durable");
        let mut outbound = a.ephemeral_publications();
        let mut inbound = b.subscribe_ephemeral("live").unwrap();
        a.publish_ephemeral("live", &payload(1)).unwrap();
        let skipped = outbound.try_recv().unwrap();
        a.publish_ephemeral("live", &payload(2)).unwrap();
        let received = outbound.try_recv().unwrap();
        assert_eq!(b.ingest_remote(&received).unwrap(), RemoteIngest::Applied);
        assert_eq!(b.ingest_remote(&received).unwrap(), RemoteIngest::Duplicate);
        assert_eq!(b.ingest_remote(&skipped).unwrap(), RemoteIngest::Duplicate);
        assert_eq!(inbound.recv().await.unwrap().data, 2);
        assert!(!resets.has_changed().unwrap());
        assert_eq!(
            a.replay_cursor_async("durable", replay).await.unwrap(),
            replay
        );
        assert_eq!(
            db.query_i64("SELECT COUNT(*) FROM tako_replay_streams WHERE channel = 'live'"),
            0
        );
        // Live delivery keeps working while the database is unreachable.
        drop(db);
        let event = b.publish_ephemeral("live", &payload(3)).unwrap();
        assert_eq!(inbound.recv().await.unwrap().event_id, event.event_id);
    }

    #[test]
    fn reopening_starts_a_new_live_identity_without_persisting_a_sequence() {
        let db = TestDb::start();
        let first = db
            .writer()
            .publish_ephemeral("live", &payload(1))
            .unwrap()
            .event_id
            .unwrap();
        let store = db.writer();
        let second = store
            .publish_ephemeral("live", &payload(2))
            .unwrap()
            .event_id
            .unwrap();
        assert_ne!(first.origin, second.origin);
        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 1);
        assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_streams"), 0);
    }

    #[tokio::test]
    async fn subscriptions_share_live_events_without_replay_rows() {
        let db = TestDb::start();
        let store = db.open().await;
        let auth = serde_json::from_value(serde_json::json!({"ok":true,"ephemeral":true})).unwrap();
        store.sync_channel_async("live", &auth).await.unwrap();
        store.publish_ephemeral("live", &payload(0)).unwrap();
        let mut first = store.subscribe_ephemeral("live").unwrap();
        let mut second = store.subscribe_ephemeral("live").unwrap();
        let mut unrelated = store.subscribe_ephemeral("other").unwrap();
        let sent = store.publish_ephemeral("live", &payload(1)).unwrap();
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
        assert!(
            store
                .read_after_async("live", None, 100)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_streams"), 0);
        assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_messages"), 0);
        drop(first);
        drop(second);
        store.publish_ephemeral("live", &payload(2)).unwrap();
        let mut reconnected = store.subscribe_ephemeral("live").unwrap();
        store.publish_ephemeral("live", &payload(3)).unwrap();
        assert_eq!(reconnected.recv().await.unwrap().data, 3);
        drop(reconnected);
        drop(unrelated);
        assert_eq!(store.ephemeral_channels(), 0);
    }

    #[tokio::test]
    async fn slow_subscribers_skip_evicted_events_and_apps_are_isolated() {
        let db = TestDb::start();
        let (one, two) = (db.open_for("one").await, db.open_for("two").await);
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
        one.append_async("durable", &payload(1)).await.unwrap();
        assert_eq!(
            one.read_after_async("durable", None, 10)
                .await
                .unwrap()
                .len(),
            1
        );
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
    use crate::ChannelPublishPayload;
    use pingora_core::protocols::l4::stream::Stream;
    use pingora_proxy::Session;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn sse_delivers_live_data_without_replay_ids() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let db = crate::test_db::TestDb::start();
        let store = db.open().await;
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
    use crate::ChannelPublishPayload;
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
            let db = crate::test_db::TestDb::start();
            let store = db.open().await;
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
            assert!(
                store
                    .read_after_async("live", None, 10)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(db.query_i64("SELECT COUNT(*) FROM tako_replay_streams"), 0);
        })
        .await
        .unwrap();
    }
}

//! Live delivery is independent of replay storage and its failure handling.
use super::*;
use parking_lot::Mutex;
use tako_channels::{ChannelEvent, ChannelPublishPayload};

pub(super) type Peers = Arc<Mutex<BTreeMap<DeviceId, wire::Sender>>>;
pub(super) type Subscribers = Arc<Mutex<Vec<wire::Sender>>>;
pub(super) struct Application {
    pub store: Arc<ChannelStore>,
    pub subscribers: Subscribers,
}
#[derive(Default)]
pub(super) struct Live {
    pub apps: BTreeMap<AppEnv, Application>,
    pub peers: Peers,
}
impl Live {
    pub fn handle(&mut self, command: Command, generation: u64) {
        match command {
            Command::Local {
                request,
                reply,
                deadline,
                admission,
            } => {
                if reply.is_closed() || deadline <= tokio::time::Instant::now() {
                    return;
                }
                let (request, _bytes) = request.into_parts();
                let response = match request {
                    Request::Publish {
                        app,
                        channel,
                        payload,
                        ephemeral: true,
                        ..
                    } => self.publish(&app, &channel, &payload),
                    _ => Response::Rejected,
                };
                let _ = reply.send((Reply::Response(response), admission));
            }
            Command::Remote {
                peer,
                generation: received,
                session,
                frame,
            } => {
                if received != generation
                    || !self
                        .peers
                        .lock()
                        .get(&peer)
                        .is_some_and(|sender| sender.is_session(&session))
                {
                    return;
                }
                if let Frame::Publication {
                    app,
                    publication: ChannelPublication::Event(event),
                } = frame.value
                {
                    if let Err(error) = self.remote(&app, &event) {
                        tracing::warn!(%error, "discarding invalid ephemeral channel event");
                    }
                }
            }
            _ => {}
        }
    }
    pub fn publish(
        &self,
        app: &AppEnv,
        channel: &str,
        payload: &ChannelPublishPayload,
    ) -> Response {
        if !valid_channel(channel) || payload.r#type.len() > 1024 {
            return Response::Rejected;
        }
        let Some(application) = self.apps.get(app) else {
            return Response::Rejected;
        };
        let Ok(message) = application.store.publish_ephemeral(channel, payload) else {
            return Response::Rejected;
        };
        let event = ChannelEvent {
            id: message.event_id.clone().expect("ephemeral event identity"),
            channel: channel.into(),
            payload: payload.clone(),
            ephemeral: true,
            lifecycle: None,
            created_at_unix_ms: None,
        };
        if let Ok(bytes) = wire::encode(&Frame::Publication {
            app: app.clone(),
            publication: ChannelPublication::Event(event),
        }) {
            for peer in self.peers.lock().values() {
                peer.send_ephemeral_bytes(bytes.clone());
            }
        }
        Self::notify(application, message.clone());
        Response::Published { message }
    }
    pub fn remote(&self, app: &AppEnv, event: &ChannelEvent) -> io::Result<()> {
        let Some(application) = self.apps.get(app) else {
            return Ok(());
        };
        if !event.ephemeral || !valid_channel(&event.channel) || event.payload.r#type.len() > 1024 {
            return Err(error("invalid ephemeral channel event"));
        }
        if application
            .store
            .ingest_remote(event)
            .map_err(|_| error("cannot ingest ephemeral event"))?
            == RemoteIngest::Applied
        {
            Self::notify(application, tako_channels::ephemeral::message(event));
        }
        Ok(())
    }
    fn notify(application: &Application, message: ChannelMessage) {
        if let Ok(bytes) = wire::encode(&Notice::Notification {
            notification: Notification::Committed {
                message,
                ephemeral: true,
            },
        }) {
            application.subscribers.lock().retain(|subscriber| {
                if subscriber.closed() {
                    return false;
                }
                subscriber.send_ephemeral_bytes(bytes.clone());
                true
            });
        }
    }
}

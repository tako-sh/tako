mod presence;
use super::{
    MAX_APPS, error,
    ipc::{Notice, Notification, Reply, Request, Response},
    peer::Frame,
    wire,
};
use std::{collections::BTreeMap, io, path::PathBuf, sync::Arc};
use tako_channels::{ChannelMessage, ChannelPublication, ChannelStore, RemoteIngest};
use tako_network::{AppEnv, DeviceId};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use zeroize::Zeroizing;

pub(super) enum Command {
    Local {
        request: wire::Received<Request>,
        reply: oneshot::Sender<Reply>,
    },
    Attach {
        peer: DeviceId,
        generation: u64,
        sender: wire::Sender,
        reply: oneshot::Sender<bool>,
    },
    Remote {
        peer: DeviceId,
        generation: u64,
        frame: wire::Received<Frame>,
    },
}
#[derive(Clone, Default)]
pub(super) struct Control {
    pub generation: u64,
    pub losses: BTreeMap<DeviceId, u64>,
}
pub(super) async fn run(
    mut broker: Broker,
    mut commands: mpsc::Receiver<Command>,
    mut control: watch::Receiver<Control>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut seen = Control::default();
    let mut expiry = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break; } }
            changed = control.changed() => {
                if changed.is_err() { break; }
                let next = control.borrow_and_update().clone();
                if next.generation != seen.generation {
                    broker.peers.clear();
                    broker.generation = next.generation;
                } else {
                    for (id, count) in &next.losses {
                        if seen.losses.get(id) != Some(count) { broker.peers.remove(id); }
                    }
                }
                broker.fence().await?;
                seen = next;
            }
            _ = expiry.tick() => { broker.expire_presence(); }
            command = commands.recv() => match command {
                Some(command) => broker.handle(command).await?,
                None => break,
            }
        }
    }
    broker.peers.clear();
    broker.fence().await
}
pub(super) struct Application {
    pub store: Arc<ChannelStore>,
    postgres_url: Option<Zeroizing<String>>,
    publications: broadcast::Receiver<ChannelPublication>,
    subscribers: Vec<wire::Sender>,
    presence: super::presence::Leases,
}
pub(super) struct Broker {
    apps_root: PathBuf,
    pub apps: BTreeMap<AppEnv, Application>,
    pub peers: BTreeMap<DeviceId, wire::Sender>,
    pub generation: u64,
    notifications: wire::Budget,
    local: DeviceId,
}
impl Broker {
    pub fn new(apps_root: PathBuf, notifications: wire::Budget, local: DeviceId) -> Self {
        Self {
            apps_root,
            apps: BTreeMap::new(),
            peers: BTreeMap::new(),
            generation: 0,
            notifications,
            local,
        }
    }
    pub async fn handle(&mut self, command: Command) -> io::Result<()> {
        match command {
            Command::Local { request, reply } => {
                let result = self.local(request.value).await?;
                let _ = reply.send(result);
            }
            Command::Attach {
                peer,
                generation,
                sender,
                reply,
            } => {
                let accepted = generation == self.generation
                    && !self.peers.contains_key(&peer)
                    && !sender.closed()
                    && !reply.is_closed();
                if accepted {
                    self.peers.insert(peer, sender);
                }
                let _ = reply.send(accepted);
            }
            Command::Remote {
                peer,
                generation,
                frame,
            } => {
                if generation == self.generation
                    && self.peers.contains_key(&peer)
                    && self.remote(&peer, frame.value).await.is_err()
                {
                    self.peers.remove(&peer);
                    self.fence().await?;
                }
            }
        }
        Ok(())
    }
    async fn register(
        &mut self,
        app: AppEnv,
        postgres_url: Option<Zeroizing<String>>,
    ) -> io::Result<()> {
        if let Some(existing) = self.apps.get(&app) {
            return if existing.postgres_url == postgres_url {
                Ok(())
            } else {
                Err(error("channel backend changed during agent lifetime"))
            };
        }
        if self.apps.len() >= MAX_APPS {
            return Err(error("channel app limit reached"));
        }
        if postgres_url
            .as_ref()
            .is_some_and(|url| url.len() > 16 * 1024)
        {
            return Err(error("channel URL exceeds limit"));
        }
        let path = self
            .apps_root
            .join(app.as_str())
            .join("data/tako/channels.sqlite");
        let url = postgres_url.clone();
        let app_id = app.to_string();
        let store = tokio::task::spawn_blocking(move || {
            let store = if let Some(url) = url {
                ChannelStore::open_postgres(&url, &app_id)
            } else {
                std::fs::create_dir_all(path.parent().unwrap())
                    .map_err(|_| error("cannot create channel storage"))?;
                ChannelStore::open_sqlite(&path)
            }
            .map_err(|_| error("cannot open channel storage"))?;
            store
                .invalidate_replay()
                .map_err(|_| error("cannot fence channel replay"))?;
            Ok::<_, io::Error>(Arc::new(store))
        })
        .await
        .map_err(|_| error("channel storage worker failed"))??;
        let publications = store.publications();
        self.apps.insert(
            app,
            Application {
                store,
                postgres_url,
                publications,
                subscribers: Vec::new(),
                presence: Default::default(),
            },
        );
        Ok(())
    }
    pub(super) async fn local(&mut self, request: Request) -> io::Result<Reply> {
        let response = match request {
            Request::UpdatePresence {
                app,
                channel,
                member,
            } => {
                if self.update_presence(&app, channel, member).await.is_ok() {
                    Response::Ok
                } else {
                    Response::Rejected
                }
            }
            Request::Presence { app, channel } => match self.presence(&app, channel).await {
                Ok(members) => Response::Presence { members },
                Err(_) => Response::Rejected,
            },
            Request::RemovePresence {
                app,
                channel,
                connection_id,
            } => {
                if self
                    .remove_presence(&app, channel, connection_id)
                    .await
                    .is_ok()
                {
                    Response::Ok
                } else {
                    Response::Rejected
                }
            }
            Request::Register { app, postgres_url } => {
                if self.register(app, postgres_url).await.is_ok() {
                    Response::Ok
                } else {
                    Response::Rejected
                }
            }
            Request::Subscribe { app } => {
                // Leave connection capacity available for publishers and registration.
                for application in self.apps.values_mut() {
                    application.subscribers.retain(|sender| !sender.closed());
                }
                if self
                    .apps
                    .values()
                    .map(|a| a.subscribers.len())
                    .sum::<usize>()
                    >= super::MAX_SUBSCRIPTIONS
                {
                    return Ok(Reply::Response(Response::Rejected));
                }
                let Some(application) = self.apps.get_mut(&app) else {
                    return Ok(Reply::Response(Response::Rejected));
                };
                application.subscribers.retain(|sender| !sender.closed());
                if application.subscribers.len() >= super::MAX_CONNECTIONS {
                    return Ok(Reply::Response(Response::Rejected));
                }
                let (sender, receiver) = wire::queue(self.notifications.clone());
                application.subscribers.push(sender);
                return Ok(Reply::Subscription(receiver));
            }
            Request::SyncChannel { app, channel, auth } => {
                let Some(application) = self.apps.get(&app) else {
                    return Ok(Reply::Response(Response::Rejected));
                };
                if valid_channel(&channel)
                    && application
                        .store
                        .sync_channel_async(&channel, &auth)
                        .await
                        .is_ok()
                {
                    Response::Ok
                } else {
                    Response::Rejected
                }
            }
            Request::Publish {
                app,
                channel,
                payload,
                ephemeral,
            } => {
                if !valid_channel(&channel) || payload.r#type.len() > 1024 {
                    return Ok(Reply::Response(Response::Rejected));
                }
                let Some(application) = self.apps.get_mut(&app) else {
                    return Ok(Reply::Response(Response::Rejected));
                };
                let result = application
                    .store
                    .run(move |store| {
                        if ephemeral {
                            store.publish_ephemeral(&channel, &payload)
                        } else {
                            store.append(&channel, &payload)
                        }
                    })
                    .await;
                match result {
                    Ok(message) => {
                        // Consume each committed publication before executing the next command.
                        let publication = application.publications.try_recv();
                        self.committed(&app, &message, ephemeral, publication.ok())
                            .await?;
                        Response::Published { message }
                    }
                    Err(_) => Response::Rejected,
                }
            }
        };
        Ok(Reply::Response(response))
    }
    pub(super) async fn committed(
        &mut self,
        app: &AppEnv,
        message: &ChannelMessage,
        ephemeral: bool,
        publication: Option<ChannelPublication>,
    ) -> io::Result<()> {
        let mut loss = match publication {
            Some(publication) => !self.fanout(&Frame::Publication {
                app: app.clone(),
                publication,
            }),
            None => true,
        };
        loss |= !self.notify(
            app,
            Notification::Committed {
                message: message.clone(),
                ephemeral,
            },
        );
        if loss {
            self.fence().await?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub(super) fn postgres_seam(&mut self, app: &AppEnv) {
        // Use the real notification/fanout path without an external PostgreSQL service.
        self.apps.get_mut(app).unwrap().postgres_url =
            Some(Zeroizing::new("test credential".into()));
    }
    fn fanout(&mut self, frame: &Frame) -> bool {
        let Ok(bytes) = wire::encode(frame) else {
            return false;
        };
        let mut complete = true;
        self.peers.retain(|_, peer| {
            let sent = peer.send_bytes(bytes.clone()).is_ok();
            complete &= sent;
            sent
        });
        complete
    }
    fn notify(&mut self, app: &AppEnv, notification: Notification) -> bool {
        let Some(application) = self.apps.get_mut(app) else {
            return true;
        };
        let Ok(bytes) = wire::encode(&Notice::Notification { notification }) else {
            application.subscribers.clear();
            return false;
        };
        let mut complete = true;
        application.subscribers.retain(|subscriber| {
            if subscriber.closed() {
                return false;
            }
            let sent = subscriber.send_bytes(bytes.clone()).is_ok();
            complete &= sent;
            sent
        });
        complete
    }
    /// Persistence precedes notification. Failure stops the broker, never silently resumes replay.
    pub async fn fence(&mut self) -> io::Result<()> {
        let apps: Vec<_> = self.apps.keys().cloned().collect();
        for app in apps {
            self.fence_app(&app).await?;
            self.fanout(&Frame::Gap { app });
        }
        Ok(())
    }
    async fn fence_app(&mut self, app: &AppEnv) -> io::Result<()> {
        if let Some(application) = self.apps.get_mut(app) {
            application.presence.clear_remote(&self.local);
            application
                .store
                .run(|store| store.invalidate_replay())
                .await
                .map_err(|_| error("cannot fence channel replay"))?;
            self.notify(app, Notification::Gap);
        }
        Ok(())
    }
    pub(super) async fn remote(&mut self, peer: &DeviceId, frame: Frame) -> io::Result<()> {
        match frame {
            Frame::Publication { app, publication } => {
                let Some(application) = self.apps.get(&app) else {
                    return Ok(());
                };
                let notification = match publication {
                    ChannelPublication::Wake { channel } => {
                        if application.postgres_url.is_none() || !valid_channel(&channel) {
                            return Ok(());
                        }
                        application.store.wake(&channel);
                        Notification::Wake { channel }
                    }
                    ChannelPublication::Event(event) => {
                        if application.postgres_url.is_some() {
                            return Ok(());
                        }
                        if !valid_channel(&event.channel) || event.payload.r#type.len() > 1024 {
                            return Err(error("invalid channel event"));
                        }
                        let reset = application.store.resets(&event.channel);
                        let to_ingest = event.clone();
                        let result = application
                            .store
                            .run(move |store| store.ingest_remote(&to_ingest))
                            .await
                            .map_err(|_| error("cannot ingest channel event"))?;
                        if result == RemoteIngest::Duplicate {
                            return Ok(());
                        }
                        if reset.has_changed().unwrap_or(true) {
                            self.notify(&app, Notification::Gap);
                        }
                        if event.ephemeral {
                            Notification::Committed {
                                ephemeral: true,
                                message: ChannelMessage {
                                    id: String::new(),
                                    event_id: Some(event.id),
                                    channel: event.channel,
                                    r#type: event.payload.r#type,
                                    data: event.payload.data,
                                },
                            }
                        } else {
                            Notification::Wake {
                                channel: event.channel,
                            }
                        }
                    }
                };
                if !self.notify(&app, notification) {
                    self.fence().await?;
                }
            }
            Frame::Gap { app } => {
                self.fence_app(&app).await?;
            }
            Frame::Presence { app, publication } => {
                self.remote_presence(peer, &app, publication)?;
            }
            Frame::Keepalive => (),
        }
        Ok(())
    }
}
fn valid_channel(channel: &str) -> bool {
    !channel.is_empty() && channel.len() <= 1024
}

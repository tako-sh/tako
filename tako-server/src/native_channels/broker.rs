mod commands;
mod live;
mod presence;
#[cfg(test)]
use super::IO_TIMEOUT;
use super::{
    MAX_APPS, error,
    ipc::{Notice, Notification, Reply, Request, Response},
    peer::Frame,
    wire,
};
use crate::channels::ChannelTarget;
pub(super) use commands::{Commands, CommandsReceiver, commands};
use parking_lot::Mutex;
use std::{collections::BTreeMap, io, sync::Arc};
use tako_channels::{ChannelMessage, ChannelPublication, ChannelStore};
use tako_network::{AppEnv, DeviceId};
use tokio::sync::{broadcast, broadcast::error::TryRecvError, mpsc, oneshot, watch};

pub(super) enum Command {
    Local {
        request: wire::Received<Request>,
        reply: oneshot::Sender<(Reply, tokio::sync::OwnedSemaphorePermit)>,
        deadline: tokio::time::Instant,
        admission: tokio::sync::OwnedSemaphorePermit,
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
        session: Arc<()>,
        frame: wire::Received<Frame>,
    },
}
#[derive(Clone, Default)]
pub(super) struct Control {
    pub generation: u64,
    pub losses: BTreeMap<DeviceId, u64>,
}
pub(super) async fn run(
    broker: Broker,
    commands: CommandsReceiver,
    control: watch::Receiver<Control>,
    shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let live = broker.live.clone();
    let (stop, mut stopped) = watch::channel(false);
    let mut live_shutdown = shutdown.clone();
    let live_control = control.clone();
    let mut ephemeral = commands.ephemeral;
    let ordered = async {
        let result = run_ordered(broker, commands.durable, control, shutdown).await;
        stop.send_replace(true);
        result
    };
    let transient = async {
        loop {
            if *live_shutdown.borrow() || *stopped.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = live_shutdown.changed() => break,
                _ = stopped.changed() => break,
                command = ephemeral.recv() => match command {
                    Some(command) => live.lock().handle(command, live_control.borrow().generation),
                    None => break,
                },
            }
        }
    };
    // Do not cancel an in-flight durable commit on shutdown: the caller keeps
    // the exclusive broker lock until both workers have stopped.
    let (result, ()) = tokio::join!(ordered, transient);
    result
}

async fn run_ordered(
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
                    broker.peers.lock().clear();
                    broker.generation = next.generation;
                } else {
                    for (id, count) in &next.losses {
                        if seen.losses.get(id) != Some(count) { broker.peers.lock().remove(id); }
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
    broker.peers.lock().clear();
    broker.fence().await
}
pub(super) struct Application {
    pub store: Arc<ChannelStore>,
    pub(super) target: ChannelTarget,
    publications: broadcast::Receiver<ChannelPublication>,
    pub(super) subscribers: live::Subscribers,
    presence: super::presence::Leases,
    presence_queried: std::collections::BTreeSet<DeviceId>,
}
pub(super) struct Broker {
    pub apps: BTreeMap<AppEnv, Application>,
    pub peers: live::Peers,
    live: Arc<Mutex<live::Live>>,
    pub generation: u64,
    notifications: wire::Budget,
    local: DeviceId,
}
impl Broker {
    fn refresh_live(&self, app: &AppEnv) {
        if let Some(application) = self.apps.get(app) {
            self.live.lock().apps.insert(
                app.clone(),
                live::Application {
                    store: application.store.clone(),
                    subscribers: application.subscribers.clone(),
                },
            );
        }
    }
    pub fn new(notifications: wire::Budget, local: DeviceId) -> Self {
        let live = Arc::new(Mutex::new(live::Live::default()));
        let peers = live.lock().peers.clone();
        Self {
            apps: BTreeMap::new(),
            peers,
            live,
            generation: 0,
            notifications,
            local,
        }
    }
    pub async fn handle(&mut self, command: Command) -> io::Result<()> {
        match command {
            Command::Local {
                request,
                reply,
                deadline,
                admission,
            } => {
                if reply.is_closed() || deadline <= tokio::time::Instant::now() {
                    return Ok(());
                }
                let (request, _input) = request.into_parts();
                let result = self.local_until(request, deadline).await?;
                let _ = reply.send((result, admission));
            }
            Command::Attach {
                peer,
                generation,
                sender,
                reply,
            } => {
                let accepted = generation == self.generation
                    && !self.peers.lock().contains_key(&peer)
                    && !sender.closed()
                    && !reply.is_closed();
                if accepted {
                    self.peers.lock().insert(peer, sender);
                }
                let _ = reply.send(accepted);
            }
            Command::Remote {
                peer,
                generation,
                session,
                frame,
            } => {
                if generation == self.generation
                    && self
                        .peers
                        .lock()
                        .get(&peer)
                        .is_some_and(|sender| sender.is_session(&session))
                    && self.remote(&peer, frame.value).await.is_err()
                {
                    self.peers.lock().remove(&peer);
                    self.fence().await?;
                }
            }
        }
        Ok(())
    }
    async fn register(
        &mut self,
        app: AppEnv,
        target: ChannelTarget,
        deadline: tokio::time::Instant,
    ) -> io::Result<()> {
        let replacing = match self.apps.get(&app) {
            Some(existing) if existing.target == target => return Ok(()),
            Some(_) => true,
            None => false,
        };
        if !replacing && self.apps.len() >= MAX_APPS {
            return Err(error("channel app limit reached"));
        }
        target.validate().map_err(error)?;
        let config = target.config(&app);
        let open = tokio::task::spawn_blocking(move || {
            ChannelStore::open_config(config)
                .map(Arc::new)
                .map_err(|_| error("cannot open channel storage"))
        });
        // Commands run one at a time, so an unresponsive database must not hold
        // the broker past the caller's deadline: the worker finishes or fails on
        // its own, and other applications keep publishing meanwhile.
        let store = match tokio::time::timeout_at(deadline, open).await {
            Ok(joined) => joined.map_err(|_| error("channel storage worker failed"))??,
            Err(_) => return Err(error("channel storage did not open before the deadline")),
        };
        let publications = store.publications();
        if replacing {
            // A rotated credential or switched backend replaces the store beneath
            // the existing subscribers. Their readers reopen against the new
            // backend; the gap tells live streams to resync instead of continuing
            // replay from the previous one.
            let application = self.apps.get_mut(&app).expect("registered application");
            application.store = store;
            application.target = target;
            application.publications = publications;
            application.presence.clear_remote(&self.local);
            application.presence_queried.clear();
            self.refresh_live(&app);
            self.notify(&app, Notification::BackendChanged);
            self.apps[&app].subscribers.lock().clear();
            return Ok(());
        }
        self.apps.insert(
            app.clone(),
            Application {
                store,
                target,
                publications,
                subscribers: Arc::default(),
                presence: Default::default(),
                presence_queried: Default::default(),
            },
        );
        self.refresh_live(&app);
        Ok(())
    }
    #[cfg(test)]
    pub(super) async fn local(&mut self, request: Request) -> io::Result<Reply> {
        self.local_until(request, tokio::time::Instant::now() + IO_TIMEOUT)
            .await
    }
    pub(super) async fn local_until(
        &mut self,
        request: Request,
        deadline: tokio::time::Instant,
    ) -> io::Result<Reply> {
        if let Request::Publish {
            app,
            channel,
            payload,
            ephemeral: true,
            ..
        } = &request
        {
            return Ok(Reply::Response(
                self.live.lock().publish(app, channel, payload),
            ));
        }
        let response = match request {
            Request::SignInvocation { .. } => Response::Rejected,
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
            Request::Register { app, target } => {
                if self.register(app, target, deadline).await.is_ok() {
                    Response::Ok
                } else {
                    Response::Rejected
                }
            }
            Request::Subscribe { app } => {
                // Leave connection capacity available for publishers and registration.
                for application in self.apps.values_mut() {
                    application
                        .subscribers
                        .lock()
                        .retain(|sender| !sender.closed());
                }
                if self
                    .apps
                    .values()
                    .map(|a| a.subscribers.lock().len())
                    .sum::<usize>()
                    >= super::MAX_SUBSCRIPTIONS
                {
                    return Ok(Reply::Response(Response::Rejected));
                }
                let Some(application) = self.apps.get_mut(&app) else {
                    return Ok(Reply::Response(Response::Rejected));
                };
                application
                    .subscribers
                    .lock()
                    .retain(|sender| !sender.closed());
                if application.subscribers.lock().len() >= super::MAX_CONNECTIONS {
                    return Ok(Reply::Response(Response::Rejected));
                }
                let (sender, receiver) = wire::queue(self.notifications.clone());
                application.subscribers.lock().push(sender);
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
                lifecycle,
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
                        store.append_with_lifecycle(&channel, &payload, lifecycle.as_ref())
                    })
                    .await;
                match result {
                    Ok(message) => {
                        // Consume each committed publication before executing the next command.
                        let publication = application.publications.try_recv();
                        // Closing a closed stream returns its marker and commits nothing.
                        let unchanged = tako_channels::is_close_marker(&message)
                            && matches!(publication, Err(TryRecvError::Empty));
                        if !unchanged {
                            self.committed(&app, &message, ephemeral, publication.ok())
                                .await?;
                        }
                        Response::Published { message }
                    }
                    Err(tako_channels::ChannelError::StreamClosed) => Response::StreamClosed,
                    Err(tako_channels::ChannelError::StreamLimit) => Response::StreamLimit,
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
    fn fanout(&mut self, frame: &Frame) -> bool {
        let Ok(bytes) = wire::encode(frame) else {
            return false;
        };
        let mut complete = true;
        self.peers.lock().retain(|_, peer| {
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
            application.subscribers.lock().clear();
            return false;
        };
        let mut complete = true;
        application.subscribers.lock().retain(|subscriber| {
            if subscriber.closed() {
                return false;
            }
            let sent = subscriber.send_bytes(bytes.clone()).is_ok();
            complete &= sent;
            sent
        });
        complete
    }
    /// After lost notifications, live subscribers reload from the shared
    /// history, which keeps everything committed.
    pub async fn fence(&mut self) -> io::Result<()> {
        let apps: Vec<_> = self.apps.keys().cloned().collect();
        for app in apps {
            self.fence_app(&app);
            self.fanout(&Frame::Gap { app });
        }
        Ok(())
    }
    fn fence_app(&mut self, app: &AppEnv) {
        if let Some(application) = self.apps.get_mut(app) {
            application.presence.clear_remote(&self.local);
            application.presence_queried.clear();
            application.store.signal_gap(None);
            self.notify(app, Notification::Gap);
        }
    }
    pub(super) async fn remote(&mut self, peer: &DeviceId, frame: Frame) -> io::Result<()> {
        match frame {
            Frame::Publication { app, publication } => {
                if let ChannelPublication::Event(event) = &publication
                    && event.ephemeral
                {
                    return self.live.lock().remote(&app, event);
                }
                let Some(application) = self.apps.get(&app) else {
                    return Ok(());
                };
                // Durable history is shared: peers only wake its readers.
                let ChannelPublication::Wake { channel } = publication else {
                    return Ok(());
                };
                if !valid_channel(&channel) {
                    return Ok(());
                }
                application.store.wake(&channel);
                let notification = Notification::Wake { channel };
                if !self.notify(&app, notification) {
                    self.fence().await?;
                }
            }
            Frame::Gap { app } => {
                self.fence_app(&app);
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

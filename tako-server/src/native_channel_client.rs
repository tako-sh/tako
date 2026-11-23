//! Proxy-side reader with an agent-owned writer and one notification subscription.
mod blocking;
pub(crate) use blocking::BlockingPublisher;

use crate::channels::{ChannelTarget, DirectChannelStores};
use crate::native_channels::{Client, Notification, Subscription};
use async_trait::async_trait;
use std::{
    io,
    ops::Deref,
    sync::{Arc, Weak},
    time::Duration,
};
use tako_channels::PresenceMember;
use tako_channels::{
    ChannelAuthResponse, ChannelError, ChannelLifecycle, ChannelMessage, ChannelPublishPayload,
    ChannelStore, ChannelStoreConfig,
};
use tako_network::AppEnv;
use tokio::{sync::watch, task::JoinHandle};

const READY_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_RETRY: Duration = Duration::from_millis(100);
const MAX_RETRY: Duration = Duration::from_secs(5);

pub(crate) fn app_scope(app: &str) -> Result<AppEnv, ChannelError> {
    let app = if app.contains('/') {
        app.to_owned()
    } else {
        format!("{app}/production")
    };
    app.parse()
        .map_err(|_| ChannelError::BadRequest("invalid channel app scope".into()))
}
fn unavailable() -> ChannelError {
    ChannelError::AuthUnavailable
}

#[async_trait]
trait BrokerLink: Send + Sync {
    async fn register(&self, app: AppEnv, target: ChannelTarget) -> io::Result<()>;
    async fn subscribe(&self, app: AppEnv) -> io::Result<Subscription>;
    async fn sync(&self, app: AppEnv, channel: String, auth: ChannelAuthResponse)
    -> io::Result<()>;
    async fn publish(
        &self,
        app: AppEnv,
        channel: String,
        payload: ChannelPublishPayload,
        ephemeral: bool,
        lifecycle: Option<ChannelLifecycle>,
    ) -> io::Result<ChannelMessage>;
    async fn update_presence(
        &self,
        app: AppEnv,
        channel: String,
        member: PresenceMember,
    ) -> io::Result<()>;
    async fn presence(&self, app: AppEnv, channel: String) -> io::Result<Vec<PresenceMember>>;
    async fn remove_presence(
        &self,
        app: AppEnv,
        channel: String,
        connection_id: String,
    ) -> io::Result<()>;
}
#[async_trait]
impl BrokerLink for Client {
    async fn register(&self, app: AppEnv, target: ChannelTarget) -> io::Result<()> {
        self.register(app, target).await
    }
    async fn subscribe(&self, app: AppEnv) -> io::Result<Subscription> {
        self.subscribe(app).await
    }
    async fn sync(
        &self,
        app: AppEnv,
        channel: String,
        auth: ChannelAuthResponse,
    ) -> io::Result<()> {
        self.sync_channel(app, channel, auth).await
    }
    async fn publish(
        &self,
        app: AppEnv,
        channel: String,
        payload: ChannelPublishPayload,
        ephemeral: bool,
        lifecycle: Option<ChannelLifecycle>,
    ) -> io::Result<ChannelMessage> {
        self.publish(app, channel, payload, ephemeral, lifecycle)
            .await
    }
    async fn update_presence(
        &self,
        app: AppEnv,
        channel: String,
        member: PresenceMember,
    ) -> io::Result<()> {
        self.update_presence(app, channel, member).await
    }
    async fn presence(&self, app: AppEnv, channel: String) -> io::Result<Vec<PresenceMember>> {
        self.presence(app, channel).await
    }
    async fn remove_presence(
        &self,
        app: AppEnv,
        channel: String,
        connection_id: String,
    ) -> io::Result<()> {
        self.remove_presence(app, channel, connection_id).await
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Readiness {
    Ready,
    Disconnected,
}
struct Native {
    link: Arc<dyn BrokerLink>,
    app: AppEnv,
    readiness: watch::Receiver<Readiness>,
    task: JoinHandle<()>,
}
impl Drop for Native {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The existing proxy cache owns this wrapper. Tasks never keep the wrapper or reader alive.
pub(crate) struct ProxyChannelStore {
    store: Arc<ChannelStore>,
    native: Option<Native>,
    target: ChannelTarget,
    local_file: Option<crate::channels::LocalChannelFile>,
}
impl Deref for ProxyChannelStore {
    type Target = Arc<ChannelStore>;
    fn deref(&self) -> &Self::Target {
        &self.store
    }
}
impl ProxyChannelStore {
    pub(crate) async fn open(
        local_path: std::path::PathBuf,
        app: AppEnv,
        target: ChannelTarget,
        native: Option<Client>,
        direct: Arc<DirectChannelStores>,
    ) -> Result<Self, ChannelError> {
        if let Some(link) = native {
            return Self::open_with_link(local_path, app, target, Arc::new(link)).await;
        }
        let to_open = target.clone();
        let (store, local_file) =
            tokio::task::spawn_blocking(move || direct.open(local_path, &app, &to_open))
                .await
                .map_err(|_| ChannelError::Storage("channel store worker failed".into()))??;
        Ok(Self {
            store,
            local_file,
            target,
            native: None,
        })
    }
    async fn open_with_link(
        local_path: std::path::PathBuf,
        app: AppEnv,
        target: ChannelTarget,
        link: Arc<dyn BrokerLink>,
    ) -> Result<Self, ChannelError> {
        let config = target.config(local_path, &app);
        link.register(app.clone(), target.clone())
            .await
            .map_err(|_| unavailable())?;
        let (store, local_file) = tokio::task::spawn_blocking(move || {
            let path = match &config {
                ChannelStoreConfig::Sqlite { path } => Some(path.clone()),
                _ => None,
            };
            let previous_file = path
                .as_ref()
                .and_then(|path| crate::channels::LocalChannelFile::capture(path.clone()).ok());
            let store = ChannelStore::open_reader_config(config)?;
            if previous_file
                .as_ref()
                .is_some_and(|file| !file.is_current())
            {
                return Err(ChannelError::Storage(
                    "channel data changed while opening reader".into(),
                ));
            }
            let local_file = path
                .map(crate::channels::LocalChannelFile::capture)
                .transpose()
                .map_err(|_| ChannelError::Storage("cannot identify channel storage".into()))?;
            Ok::<_, ChannelError>((store, local_file))
        })
        .await
        .map_err(|_| ChannelError::Storage("channel store worker failed".into()))?
        .map_err(|_| ChannelError::Storage("cannot open channel storage".into()))?;
        let store = Arc::new(store);
        Self::subscribe(store, app, target, link, local_file).await
    }
    async fn subscribe(
        store: Arc<ChannelStore>,
        app: AppEnv,
        target: ChannelTarget,
        link: Arc<dyn BrokerLink>,
        local_file: Option<crate::channels::LocalChannelFile>,
    ) -> Result<Self, ChannelError> {
        // A subscriber is established before callers can capture replay heads.
        let subscription = link
            .subscribe(app.clone())
            .await
            .map_err(|_| unavailable())?;
        let (ready, readiness) = watch::channel(Readiness::Ready);
        let task = tokio::spawn(notifications(
            Arc::downgrade(&store),
            link.clone(),
            app.clone(),
            target.clone(),
            subscription,
            ready,
            local_file.clone(),
        ));
        Ok(Self {
            store,
            local_file,
            target,
            native: Some(Native {
                link,
                app,
                readiness,
                task,
            }),
        })
    }
    pub(crate) fn store(&self) -> &Arc<ChannelStore> {
        &self.store
    }
    pub(crate) fn is_native(&self) -> bool {
        self.native.is_some()
    }
    /// Stop the broker subscription now, before the last request drops this
    /// wrapper. Readiness then fails closed: the worker must not re-register the
    /// previous backend while a replacement reader owns the subscription.
    pub(crate) fn detach(&self) {
        if let Some(native) = &self.native {
            native.task.abort();
        }
    }
    pub(crate) fn check_backend(&self, target: &ChannelTarget) -> Result<(), ChannelError> {
        if self
            .local_file
            .as_ref()
            .is_some_and(|file| !file.is_current())
        {
            return Err(ChannelError::Storage(
                "channel data was replaced; reopen reader".into(),
            ));
        }
        if self.target != *target {
            return Err(ChannelError::Storage(
                "channel backend configuration changed; broker re-registration required".into(),
            ));
        }
        if self
            .native
            .as_ref()
            .is_some_and(|native| native.task.is_finished())
        {
            return Err(unavailable());
        }
        Ok(())
    }
    pub(crate) async fn ensure_ready(&self) -> Result<(), ChannelError> {
        if self
            .local_file
            .as_ref()
            .is_some_and(|file| !file.is_current())
        {
            return Err(unavailable());
        }
        let Some(native) = &self.native else {
            return Ok(());
        };
        let mut state = native.readiness.clone();
        tokio::time::timeout(READY_TIMEOUT, async {
            loop {
                if state.has_changed().is_err() {
                    return Err(unavailable());
                }
                if *state.borrow_and_update() == Readiness::Ready {
                    return Ok(());
                }
                state.changed().await.map_err(|_| unavailable())?;
            }
        })
        .await
        .map_err(|_| unavailable())?
    }
    pub(crate) async fn sync_channel_async(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        self.ensure_ready().await?;
        if let Some(native) = &self.native {
            native
                .link
                .sync(native.app.clone(), channel.into(), auth.clone())
                .await
                .map_err(|_| unavailable())
        } else {
            self.store.sync_channel_async(channel, auth).await
        }
    }
    pub(crate) async fn publish(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
        ephemeral: bool,
    ) -> Result<ChannelMessage, ChannelError> {
        self.ensure_ready().await?;
        if let Some(native) = &self.native {
            native
                .link
                .publish(
                    native.app.clone(),
                    channel.into(),
                    payload.clone(),
                    ephemeral,
                    None,
                )
                .await
                .map_err(|error| {
                    if crate::native_channels::is_stream_closed(&error) {
                        ChannelError::StreamClosed
                    } else {
                        unavailable()
                    }
                })
        } else {
            let channel = channel.to_owned();
            let payload = payload.clone();
            if ephemeral {
                return self.store.publish_ephemeral(&channel, &payload);
            }
            self.store
                .run(move |store| store.append(&channel, &payload))
                .await
        }
    }
}

#[async_trait]
impl tako_channels::ChannelWriter for ProxyChannelStore {
    async fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        self.sync_channel_async(channel, auth).await
    }
    async fn publish(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
        ephemeral: bool,
    ) -> Result<ChannelMessage, ChannelError> {
        self.publish(channel, payload, ephemeral).await
    }
    async fn update_presence(
        &self,
        channel: &str,
        member: &PresenceMember,
    ) -> Result<(), ChannelError> {
        self.ensure_ready().await?;
        if let Some(native) = &self.native {
            native
                .link
                .update_presence(native.app.clone(), channel.into(), member.clone())
                .await
                .map_err(|_| unavailable())
        } else {
            tako_channels::LocalChannelWriter::new(self.store.clone())
                .update_presence(channel, member)
                .await
        }
    }
    async fn presence(&self, channel: &str) -> Result<Vec<PresenceMember>, ChannelError> {
        self.ensure_ready().await?;
        if let Some(native) = &self.native {
            native
                .link
                .presence(native.app.clone(), channel.into())
                .await
                .map_err(|_| unavailable())
        } else {
            tako_channels::LocalChannelWriter::new(self.store.clone())
                .presence(channel)
                .await
        }
    }
    async fn remove_presence(
        &self,
        channel: &str,
        connection_id: &str,
    ) -> Result<(), ChannelError> {
        // Cleanup is best effort at the broker; a disconnected subscription must not
        // suppress a leave that can still reach the broker on a fresh IPC connection.
        if let Some(native) = &self.native {
            native
                .link
                .remove_presence(native.app.clone(), channel.into(), connection_id.into())
                .await
                .map_err(|_| unavailable())
        } else {
            tako_channels::LocalChannelWriter::new(self.store.clone())
                .remove_presence(channel, connection_id)
                .await
        }
    }
}

struct ResetOnDrop(Weak<ChannelStore>);
impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        if let Some(store) = self.0.upgrade() {
            store.signal_gap(None);
        }
    }
}
async fn notifications(
    store: Weak<ChannelStore>,
    link: Arc<dyn BrokerLink>,
    app: AppEnv,
    target: ChannelTarget,
    mut subscription: Subscription,
    ready: watch::Sender<Readiness>,
    local_file: Option<crate::channels::LocalChannelFile>,
) {
    let _reset = ResetOnDrop(store.clone());
    loop {
        while let Ok(notification) = subscription.recv().await {
            if local_file.as_ref().is_some_and(|file| !file.is_current()) {
                return;
            }
            let Some(reader) = store.upgrade() else {
                return;
            };
            match notification {
                Notification::Committed { message, ephemeral } => {
                    reader.notify_committed(&message, ephemeral)
                }
                Notification::Wake { channel } => reader.wake(&channel),
                Notification::Gap => reader.signal_gap(None),
                Notification::BackendChanged => return,
            }
        }
        ready.send_replace(Readiness::Disconnected);
        if let Some(reader) = store.upgrade() {
            reader.signal_gap(None);
        } else {
            return;
        }
        let mut delay = FIRST_RETRY;
        loop {
            tokio::time::sleep(delay).await;
            if local_file.as_ref().is_some_and(|file| !file.is_current()) {
                return;
            }
            if store.strong_count() == 0 {
                return;
            }
            let registered = link.register(app.clone(), target.clone()).await;
            if registered.is_ok()
                && let Ok(next) = link.subscribe(app.clone()).await
            {
                subscription = next;
                // Catch streams established in the interval between failure and its first reset.
                if let Some(reader) = store.upgrade() {
                    reader.signal_gap(None);
                } else {
                    return;
                }
                ready.send_replace(Readiness::Ready);
                break;
            }
            delay = (delay * 2).min(MAX_RETRY);
        }
    }
}

#[cfg(test)]
mod tests;

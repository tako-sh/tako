//! Proxy-side reader with an agent-owned writer and one notification subscription.
mod blocking;
pub(crate) use blocking::BlockingPublisher;

use crate::native_channels::{Client, Notification, Subscription};
use async_trait::async_trait;
use std::{
    io,
    ops::Deref,
    sync::{Arc, Weak},
    time::Duration,
};
use tako_channels::{
    ChannelAuthResponse, ChannelError, ChannelMessage, ChannelPublishPayload, ChannelStore,
    ChannelStoreConfig,
};
use tako_channels::PresenceMember;
use tako_network::AppEnv;
use tokio::{sync::watch, task::JoinHandle};
use zeroize::Zeroizing;

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
    async fn register(&self, app: AppEnv, postgres_url: Option<String>) -> io::Result<()>;
    async fn subscribe(&self, app: AppEnv) -> io::Result<Subscription>;
    async fn sync(&self, app: AppEnv, channel: String, auth: ChannelAuthResponse)
    -> io::Result<()>;
    async fn publish(
        &self,
        app: AppEnv,
        channel: String,
        payload: ChannelPublishPayload,
        ephemeral: bool,
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
    async fn register(&self, app: AppEnv, postgres_url: Option<String>) -> io::Result<()> {
        self.register(app, postgres_url).await
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
    ) -> io::Result<ChannelMessage> {
        self.publish(app, channel, payload, ephemeral).await
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
    postgres_url: Option<Zeroizing<String>>,
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
}
impl Deref for ProxyChannelStore {
    type Target = Arc<ChannelStore>;
    fn deref(&self) -> &Self::Target {
        &self.store
    }
}
impl ProxyChannelStore {
    pub(crate) async fn open(
        config: ChannelStoreConfig,
        app: AppEnv,
        native: Option<Client>,
    ) -> Result<Self, ChannelError> {
        let link = native.map(|client| Arc::new(client) as Arc<dyn BrokerLink>);
        Self::open_with_link(config, app, link).await
    }
    async fn open_with_link(
        mut config: ChannelStoreConfig,
        app: AppEnv,
        link: Option<Arc<dyn BrokerLink>>,
    ) -> Result<Self, ChannelError> {
        let postgres_url = match &mut config {
            ChannelStoreConfig::Postgres { url, app_id, .. } => {
                if link.is_some() {
                    *app_id = app.to_string();
                }
                Some(Zeroizing::new(url.clone()))
            }
            ChannelStoreConfig::Sqlite { .. } => None,
        };
        if let Some(link) = &link {
            link.register(
                app.clone(),
                postgres_url.as_deref().map(|url| url.to_owned()),
            )
            .await
            .map_err(|_| unavailable())?;
        }
        let reader_only = link.is_some();
        let store = tokio::task::spawn_blocking(move || {
            if reader_only {
                ChannelStore::open_reader_config(config)
            } else {
                ChannelStore::open_config(config)
            }
        })
        .await
        .map_err(|_| ChannelError::Storage("channel store worker failed".into()))?
        .map_err(|_| ChannelError::Storage("cannot open channel storage".into()))?;
        let store = Arc::new(store);
        if let Some(link) = link {
            Self::subscribe(store, app, postgres_url, link).await
        } else {
            Ok(Self {
                store,
                native: None,
            })
        }
    }
    async fn subscribe(
        store: Arc<ChannelStore>,
        app: AppEnv,
        postgres_url: Option<Zeroizing<String>>,
        link: Arc<dyn BrokerLink>,
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
            postgres_url.clone(),
            subscription,
            ready,
        ));
        Ok(Self {
            store,
            native: Some(Native {
                link,
                app,
                postgres_url,
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
    pub(crate) fn check_backend(&self, postgres_url: Option<&str>) -> Result<(), ChannelError> {
        if let Some(native) = &self.native
            && native.postgres_url.as_ref().map(|url| url.as_str()) != postgres_url
        {
            return Err(ChannelError::Storage(
                "channel backend configuration changed; broker re-registration required".into(),
            ));
        }
        Ok(())
    }
    pub(crate) async fn ensure_ready(&self) -> Result<(), ChannelError> {
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
                )
                .await
                .map_err(|_| unavailable())
        } else {
            let channel = channel.to_owned();
            let payload = payload.clone();
            self.store
                .run(move |store| {
                    if ephemeral {
                        store.publish_ephemeral(&channel, &payload)
                    } else {
                        store.append(&channel, &payload)
                    }
                })
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
    postgres_url: Option<Zeroizing<String>>,
    mut subscription: Subscription,
    ready: watch::Sender<Readiness>,
) {
    let _reset = ResetOnDrop(store.clone());
    loop {
        while let Ok(notification) = subscription.recv().await {
            let Some(reader) = store.upgrade() else {
                return;
            };
            match notification {
                Notification::Committed { message, ephemeral } => {
                    reader.notify_committed(&message, ephemeral)
                }
                Notification::Wake { channel } => reader.wake(&channel),
                Notification::Gap => reader.signal_gap(None),
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
            if store.strong_count() == 0 {
                return;
            }
            let registered = link
                .register(
                    app.clone(),
                    postgres_url.as_deref().map(|url| url.to_owned()),
                )
                .await;
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

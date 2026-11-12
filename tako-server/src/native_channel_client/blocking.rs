use super::{BrokerLink, app_scope};
use crate::native_channels::Client;
use std::{
    io,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use tako_channels::{
    ChannelError, ChannelLifecycle, ChannelMessage, ChannelPublishPayload, TrustedPublication,
};
use tako_network::AppEnv;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use zeroize::Zeroizing;

const MAX_PAYLOAD: usize = 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(20);
struct Publication {
    app: AppEnv,
    channel: String,
    payload: ChannelPublishPayload,
    lifecycle: Option<ChannelLifecycle>,
    postgres_url: Option<Zeroizing<String>>,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<ChannelMessage, String>>,
    _bytes: OwnedSemaphorePermit,
}

/// A bounded synchronous bridge for WorkflowManager's synchronous publisher closure.
/// The receiver thread owns no sender, so dropping the publisher terminates it.
#[derive(Clone)]
pub(crate) struct BlockingPublisher {
    sender: mpsc::SyncSender<Publication>,
    budget: Arc<Semaphore>,
}
impl BlockingPublisher {
    pub(crate) fn new(client: Client) -> io::Result<Self> {
        Self::with_link(Arc::new(client))
    }
    fn with_link(link: Arc<dyn BrokerLink>) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (sender, receiver) = mpsc::sync_channel::<Publication>(64);
        std::thread::Builder::new()
            .name("native-channel-publisher".into())
            .spawn(move || {
                while let Ok(publication) = receiver.recv() {
                    if publication.deadline <= Instant::now() {
                        let _ = publication.reply.send(Err(
                            "native channel publication expired before dispatch".into(),
                        ));
                        continue;
                    }
                    let remaining = publication
                        .deadline
                        .saturating_duration_since(Instant::now());
                    let result = runtime
                        .block_on(async {
                            tokio::time::timeout(remaining, send(&*link, &publication)).await
                        })
                        .unwrap_or_else(|_| {
                            Err("native channel publication acknowledgement timed out".into())
                        });
                    let _ = publication.reply.send(result);
                }
            })?;
        Ok(Self {
            sender,
            budget: Arc::new(Semaphore::new(8 * MAX_PAYLOAD)),
        })
    }
    /// Publish a trusted publication. The acknowledgement names the public
    /// channel, never the stream key the broker stores it under.
    pub(crate) fn publish(
        &self,
        app: &str,
        publication: &TrustedPublication,
        postgres_url: Option<String>,
    ) -> Result<serde_json::Value, String> {
        let deadline = Instant::now() + DEADLINE;
        let app = app_scope(app).map_err(|_| "invalid channel app scope")?;
        let postgres_url = postgres_url.map(Zeroizing::new);
        let channel = publication.stream.key().to_owned();
        if channel.len() > 1024
            || postgres_url
                .as_ref()
                .is_some_and(|url| url.len() > 16 * 1024)
        {
            return Err("native channel publication exceeds limits".into());
        }
        let encoded =
            serde_json::to_vec(&publication.payload).map_err(|_| "invalid channel payload")?;
        if encoded.len() > MAX_PAYLOAD {
            return Err("native channel publication exceeds limits".into());
        }
        let bytes = self
            .budget
            .clone()
            .try_acquire_many_owned((encoded.len() + 20 * 1024) as u32)
            .map_err(|_| "native channel publisher is busy")?;
        let (reply, receive) = mpsc::sync_channel(1);
        self.sender
            .try_send(Publication {
                app,
                channel,
                payload: publication.payload.clone(),
                lifecycle: publication.lifecycle.clone(),
                postgres_url,
                deadline,
                reply,
                _bytes: bytes,
            })
            .map_err(|_| "native channel publisher is unavailable or busy")?;
        let message = receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "native channel publication acknowledgement timed out".to_string())??;
        serde_json::to_value(publication.stream.public(message))
            .map_err(|_| "invalid native channel acknowledgement".into())
    }
}
async fn send(link: &dyn BrokerLink, publication: &Publication) -> Result<ChannelMessage, String> {
    link.register(
        publication.app.clone(),
        publication
            .postgres_url
            .as_deref()
            .map(|url| url.to_owned()),
    )
    .await
    .map_err(|_| "native channel registration rejected or unavailable")?;
    if let Some(lifecycle) = &publication.lifecycle {
        link.sync(
            publication.app.clone(),
            publication.channel.clone(),
            lifecycle.auth(),
        )
        .await
        .map_err(|_| "native channel lifecycle sync rejected or unavailable")?;
    }
    link.publish(
        publication.app.clone(),
        publication.channel.clone(),
        publication.payload.clone(),
        publication
            .lifecycle
            .as_ref()
            .is_some_and(|lifecycle| lifecycle.ephemeral),
    )
    .await
    .map_err(|error| {
        if crate::native_channels::is_stream_closed(&error) {
            ChannelError::StreamClosed.to_string()
        } else {
            "native channel publication rejected or acknowledgement lost".into()
        }
    })
}

#[cfg(test)]
mod tests;

use super::{BrokerLink, app_scope};
use crate::native_channels::Client;
use std::{
    io,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use tako_channels::{ChannelLifecycle, ChannelMessage, ChannelPublishPayload, TrustedPublication};
use tako_core::{ErrorCode, SocketError};
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
    reply: mpsc::SyncSender<Result<ChannelMessage, SocketError>>,
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
                        let _ = publication.reply.send(Err(busy(
                            "native channel publication expired before dispatch",
                        )));
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
                            Err(busy("native channel publication acknowledgement timed out"))
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
    ) -> Result<serde_json::Value, SocketError> {
        let deadline = Instant::now() + DEADLINE;
        let app = app_scope(app).map_err(|_| "invalid channel app scope")?;
        let postgres_url = postgres_url.map(Zeroizing::new);
        let channel = publication.stream.key().to_owned();
        if channel.len() > 1024
            || postgres_url
                .as_ref()
                .is_some_and(|url| url.len() > 16 * 1024)
        {
            return Err(too_large());
        }
        let encoded = serde_json::to_vec(&publication.payload)
            .map_err(|_| SocketError::new(ErrorCode::InvalidRequest, "invalid channel payload"))?;
        if encoded.len() > MAX_PAYLOAD {
            return Err(too_large());
        }
        let bytes = self
            .budget
            .clone()
            .try_acquire_many_owned((encoded.len() + 20 * 1024) as u32)
            .map_err(|_| busy("native channel publisher is busy"))?;
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
            .map_err(|_| busy("native channel publisher is unavailable or busy"))?;
        let message = receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| busy("native channel publication acknowledgement timed out"))??;
        serde_json::to_value(publication.stream.public(message))
            .map_err(|_| "invalid native channel acknowledgement".into())
    }
}
/// The publisher or broker cannot take the publication now; it may later.
fn busy(message: &str) -> SocketError {
    SocketError::new(ErrorCode::Unavailable, message)
}

fn too_large() -> SocketError {
    SocketError::new(
        ErrorCode::InvalidRequest,
        "native channel publication exceeds limits",
    )
}

async fn send(
    link: &dyn BrokerLink,
    publication: &Publication,
) -> Result<ChannelMessage, SocketError> {
    link.register(
        publication.app.clone(),
        publication
            .postgres_url
            .as_deref()
            .map(|url| url.to_owned()),
    )
    .await
    .map_err(|_| busy("native channel registration rejected or unavailable"))?;
    // The lifecycle travels with the publish: the broker applies it only once
    // the stream accepts the message.
    link.publish(
        publication.app.clone(),
        publication.channel.clone(),
        publication.payload.clone(),
        publication
            .lifecycle
            .as_ref()
            .is_some_and(|lifecycle| lifecycle.ephemeral),
        publication.lifecycle.clone(),
    )
    .await
    .map_err(
        |error| match crate::native_channels::publish_refusal(&error) {
            Some(refusal) => refusal.into(),
            None => "native channel publication rejected or acknowledgement lost".into(),
        },
    )
}

#[cfg(test)]
mod tests;

use super::{BrokerLink, app_scope};
use crate::native_channels::Client;
use std::{
    io,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use tako_channels::{ChannelAuthResponse, ChannelLifecycle, ChannelPublishPayload};
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
    reply: mpsc::SyncSender<Result<serde_json::Value, String>>,
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
    pub(crate) fn publish(
        &self,
        app: &str,
        channel: &str,
        payload: serde_json::Value,
        postgres_url: Option<String>,
    ) -> Result<serde_json::Value, String> {
        let deadline = Instant::now() + DEADLINE;
        let app = app_scope(app).map_err(|_| "invalid channel app scope")?;
        let postgres_url = postgres_url.map(Zeroizing::new);
        if channel.len() > 1024
            || postgres_url
                .as_ref()
                .is_some_and(|url| url.len() > 16 * 1024)
        {
            return Err("native channel publication exceeds limits".into());
        }
        let encoded = serde_json::to_vec(&payload).map_err(|_| "invalid channel payload")?;
        if encoded.len() > MAX_PAYLOAD {
            return Err("native channel publication exceeds limits".into());
        }
        let bytes = self
            .budget
            .clone()
            .try_acquire_many_owned((encoded.len() + 20 * 1024) as u32)
            .map_err(|_| "native channel publisher is busy")?;
        let channel = tako_channels::live_store::publication_channel(channel, &payload)
            .map_err(|_| "invalid channel publication")?;
        let lifecycle = payload
            .get("lifecycle")
            .cloned()
            .map(serde_json::from_value::<ChannelLifecycle>)
            .transpose()
            .map_err(|_| "invalid channel lifecycle")?;
        let payload = serde_json::from_value(payload).map_err(|_| "invalid channel payload")?;
        let (reply, receive) = mpsc::sync_channel(1);
        self.sender
            .try_send(Publication {
                app,
                channel,
                payload,
                lifecycle,
                postgres_url,
                deadline,
                reply,
                _bytes: bytes,
            })
            .map_err(|_| "native channel publisher is unavailable or busy")?;
        receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "native channel publication acknowledgement timed out".to_string())?
    }
}
async fn send(
    link: &dyn BrokerLink,
    publication: &Publication,
) -> Result<serde_json::Value, String> {
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
            ChannelAuthResponse {
                ok: true,
                subject: None,
                transport: None,
                ephemeral: lifecycle.ephemeral,
                replay_window_ms: lifecycle.replay_window_ms,
                inactivity_ttl_ms: lifecycle.inactivity_ttl_ms,
                keepalive_interval_ms: lifecycle.keepalive_interval_ms,
                max_connection_lifetime_ms: lifecycle.max_connection_lifetime_ms,
            },
        )
        .await
        .map_err(|_| "native channel lifecycle sync rejected or unavailable")?;
    }
    let message = link
        .publish(
            publication.app.clone(),
            publication.channel.clone(),
            publication.payload.clone(),
            publication
                .lifecycle
                .as_ref()
                .is_some_and(|lifecycle| lifecycle.ephemeral),
        )
        .await
        .map_err(|_| "native channel publication rejected or acknowledgement lost")?;
    serde_json::to_value(message).map_err(|_| "invalid native channel acknowledgement".into())
}

#[cfg(test)]
mod tests;

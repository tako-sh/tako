use std::sync::Arc;

use crate::{
    ChannelAuthResponse, ChannelError, ChannelMessage, ChannelPublishPayload, ChannelStore,
    PresenceMember,
};

#[async_trait::async_trait]
pub trait ChannelWriter: Send + Sync {
    async fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError>;
    async fn publish(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
        ephemeral: bool,
    ) -> Result<ChannelMessage, ChannelError>;
    async fn update_presence(
        &self,
        channel: &str,
        member: &PresenceMember,
    ) -> Result<(), ChannelError>;
    async fn presence(&self, channel: &str) -> Result<Vec<PresenceMember>, ChannelError>;
    async fn remove_presence(&self, channel: &str, connection_id: &str)
    -> Result<(), ChannelError>;
}

pub struct LocalChannelWriter(Arc<ChannelStore>);

impl LocalChannelWriter {
    pub fn new(store: Arc<ChannelStore>) -> Self {
        Self(store)
    }
}

#[async_trait::async_trait]
impl ChannelWriter for LocalChannelWriter {
    async fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        self.0.sync_channel_async(channel, auth).await
    }

    async fn publish(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
        ephemeral: bool,
    ) -> Result<ChannelMessage, ChannelError> {
        if ephemeral {
            return self.0.publish_ephemeral(channel, payload);
        }
        let channel = channel.to_owned();
        let payload = payload.clone();
        self.0
            .run(move |store| store.append(&channel, &payload))
            .await
    }

    // Presence lives in the store's memory: no database work to move off the executor.
    async fn update_presence(
        &self,
        channel: &str,
        member: &PresenceMember,
    ) -> Result<(), ChannelError> {
        self.0
            .update_presence(channel, member, crate::store::now_unix_ms())
    }

    async fn presence(&self, channel: &str) -> Result<Vec<PresenceMember>, ChannelError> {
        self.0.presence(channel, crate::store::now_unix_ms())
    }

    async fn remove_presence(
        &self,
        channel: &str,
        connection_id: &str,
    ) -> Result<(), ChannelError> {
        self.0.remove_presence(channel, connection_id)
    }
}

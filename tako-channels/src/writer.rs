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
        let channel = channel.to_owned();
        let payload = payload.clone();
        self.0
            .run(move |store| {
                if ephemeral {
                    store.publish_ephemeral(&channel, &payload)
                } else {
                    store.append(&channel, &payload)
                }
            })
            .await
    }

    async fn update_presence(
        &self,
        channel: &str,
        member: &PresenceMember,
    ) -> Result<(), ChannelError> {
        let channel = channel.to_owned();
        let member = member.clone();
        self.0
            .run(move |store| store.update_presence(&channel, &member, crate::store::now_unix_ms()))
            .await
    }

    async fn presence(&self, channel: &str) -> Result<Vec<PresenceMember>, ChannelError> {
        let channel = channel.to_owned();
        self.0
            .run(move |store| store.presence(&channel, crate::store::now_unix_ms()))
            .await
    }

    async fn remove_presence(
        &self,
        channel: &str,
        connection_id: &str,
    ) -> Result<(), ChannelError> {
        let channel = channel.to_owned();
        let connection_id = connection_id.to_owned();
        self.0
            .run(move |store| store.remove_presence(&channel, &connection_id))
            .await
    }
}

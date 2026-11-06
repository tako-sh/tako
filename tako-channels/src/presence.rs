use crate::{ChannelError, ChannelStore, ChannelWriter};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const PRESENCE_CHANNEL_PREFIX: &str = "tako.presence:";

/// Scope membership by the space and its validated connection params.
pub fn presence_scope(channel: &str, params: &serde_json::Value) -> String {
    let mut params = params.clone();
    params.sort_all_objects();
    let canonical = serde_json::json!([channel, params]).to_string();
    openssl::sha::sha256(canonical.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceMember {
    pub connection_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub data: serde_json::Value,
}

/// One authorized socket. Client frames can replace metadata but never identity.
pub struct PresenceSession {
    writer: Arc<dyn ChannelWriter>,
    channel: String,
    member: PresenceMember,
    joined: bool,
    last_snapshot: Option<Vec<PresenceMember>>,
}

impl PresenceSession {
    pub fn new(
        store: Arc<ChannelStore>,
        channel: &str,
        subject: Option<String>,
    ) -> Result<Self, ChannelError> {
        Self::new_with_writer(
            Arc::new(crate::LocalChannelWriter::new(store)),
            channel,
            subject,
        )
    }

    pub fn new_with_writer(
        writer: Arc<dyn ChannelWriter>,
        channel: &str,
        subject: Option<String>,
    ) -> Result<Self, ChannelError> {
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).map_err(|e| ChannelError::Storage(e.to_string()))?;
        Ok(Self {
            writer,
            channel: channel.into(),
            member: PresenceMember {
                connection_id: id.iter().map(|byte| format!("{byte:02x}")).collect(),
                subject,
                data: serde_json::Value::Null,
            },
            joined: false,
            last_snapshot: None,
        })
    }

    pub async fn update(&mut self, payload: &[u8]) -> Result<(), ChannelError> {
        #[derive(Deserialize)]
        struct Update {
            r#type: String,
            data: serde_json::Value,
        }
        let update: Update = serde_json::from_slice(payload)
            .map_err(|_| ChannelError::BadRequest("Invalid presence update".into()))?;
        if update.r#type != "tako.presence" {
            return Err(ChannelError::BadRequest(
                "Expected a presence update".into(),
            ));
        }
        let mut member = self.member.clone();
        member.data = update.data;
        self.writer.update_presence(&self.channel, &member).await?;
        self.member = member;
        self.joined = true;
        Ok(())
    }

    pub async fn snapshot(&mut self) -> Result<Option<String>, ChannelError> {
        if !self.joined {
            return Ok(None);
        }
        let members = self.writer.presence(&self.channel).await?;
        if self.last_snapshot.as_ref() == Some(&members) {
            return Ok(None);
        }
        let encoded = serde_json::json!({
            "type": "tako.presence", "connectionId": self.member.connection_id, "members": members,
        })
        .to_string();
        self.last_snapshot = Some(members);
        Ok(Some(encoded))
    }
}

impl Drop for PresenceSession {
    fn drop(&mut self) {
        if !self.joined {
            return;
        }
        let writer = self.writer.clone();
        let channel = self.channel.clone();
        let connection_id = self.member.connection_id.clone();
        // Also runs on I/O errors and task cancellation. A process crash is
        // covered by the lease, which only client heartbeats renew.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = writer.remove_presence(&channel, &connection_id).await {
                    tracing::warn!(%error, "presence cleanup failed");
                }
            });
        }
    }
}

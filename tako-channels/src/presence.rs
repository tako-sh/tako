use crate::{ChannelError, ChannelStore};
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
    store: Arc<ChannelStore>,
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
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).map_err(|e| ChannelError::Storage(e.to_string()))?;
        Ok(Self {
            store,
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
        let saved = member.clone();
        let channel = self.channel.clone();
        self.store
            .run(move |store| store.update_presence(&channel, &saved, crate::store::now_unix_ms()))
            .await?;
        self.member = member;
        self.joined = true;
        Ok(())
    }

    pub async fn snapshot(&mut self) -> Result<Option<String>, ChannelError> {
        if !self.joined {
            return Ok(None);
        }
        let channel = self.channel.clone();
        let members = self
            .store
            .run(move |store| store.presence(&channel, crate::store::now_unix_ms()))
            .await?;
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
        let store = self.store.clone();
        let channel = self.channel.clone();
        let connection_id = self.member.connection_id.clone();
        // Also runs on I/O errors and task cancellation. A process crash is
        // covered by the lease, which only client heartbeats renew.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(move || {
                if let Err(error) = store.remove_presence(&channel, &connection_id) {
                    tracing::warn!(%error, "presence cleanup failed");
                }
            });
        }
    }
}

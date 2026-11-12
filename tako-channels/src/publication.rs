//! Server-side publications arriving on the trusted internal socket.

use crate::live_store::{STORE_CHANNEL_PREFIX, publication_channel};
use crate::params::{ParamsError, validate_value};
use crate::stream::close_payload;
use crate::{
    CLOSE_MESSAGE_TYPE, ChannelAuthResponse, ChannelDefinitionMeta, ChannelError, ChannelLifecycle,
    ChannelMessage, ChannelPublishPayload, ChannelStore, ChannelStream, PRESENCE_CHANNEL_PREFIX,
};
use serde_json::Value;

/// One `channel_publish` command: the stream its params select, the message,
/// and the lifecycle its channel definition declares.
#[derive(Debug, Clone, PartialEq)]
pub struct TrustedPublication {
    pub stream: ChannelStream,
    pub payload: ChannelPublishPayload,
    pub lifecycle: Option<ChannelLifecycle>,
}

impl TrustedPublication {
    /// Stores choose their stream from the payload's `scopeParams`; channels
    /// choose it from the command's `params`.
    pub fn parse(channel: &str, params: &Value, payload: Value) -> Result<Self, ChannelError> {
        let stream = if channel.starts_with(STORE_CHANNEL_PREFIX) {
            ChannelStream::with_key(channel, publication_channel(channel, &payload)?)
        } else {
            ChannelStream::for_publish(channel, params)?
        };
        let lifecycle = payload
            .get("lifecycle")
            .cloned()
            .map(serde_json::from_value::<ChannelLifecycle>)
            .transpose()
            .map_err(|e| ChannelError::BadRequest(format!("invalid channel lifecycle: {e}")))?;
        let payload: ChannelPublishPayload = serde_json::from_value(payload)
            .map_err(|e| ChannelError::BadRequest(format!("invalid payload: {e}")))?;
        if payload.r#type == CLOSE_MESSAGE_TYPE {
            return Err(ChannelError::BadRequest(format!(
                "message type '{CLOSE_MESSAGE_TYPE}' is reserved: close the stream instead"
            )));
        }
        Ok(Self {
            stream,
            payload,
            lifecycle,
        })
    }

    /// One `channel_close` command: the terminal marker of the stream its
    /// params select. Only durable channel streams can be closed: an
    /// ephemeral stream retains nothing a marker could follow, and Stores and
    /// presence spaces are not message streams.
    pub fn close(
        channel: &str,
        params: &Value,
        lifecycle: Option<Value>,
    ) -> Result<Self, ChannelError> {
        if channel.starts_with(STORE_CHANNEL_PREFIX) || channel.starts_with(PRESENCE_CHANNEL_PREFIX)
        {
            return Err(ChannelError::BadRequest(
                "only channel streams can be closed".into(),
            ));
        }
        let stream = ChannelStream::for_publish(channel, params)?;
        let lifecycle = lifecycle
            .map(serde_json::from_value::<ChannelLifecycle>)
            .transpose()
            .map_err(|e| ChannelError::BadRequest(format!("invalid channel lifecycle: {e}")))?;
        let publication = Self {
            stream,
            payload: close_payload(),
            lifecycle,
        };
        if publication.ephemeral() {
            return Err(ephemeral_close());
        }
        Ok(publication)
    }

    /// Whether this publication closes its stream.
    pub fn is_close(&self) -> bool {
        self.payload.r#type == CLOSE_MESSAGE_TYPE
    }

    pub fn ephemeral(&self) -> bool {
        self.lifecycle
            .as_ref()
            .is_some_and(|lifecycle| lifecycle.ephemeral)
    }

    /// The declared lifecycle in the shape stores sync.
    pub fn lifecycle_auth(&self) -> Option<ChannelAuthResponse> {
        self.lifecycle.as_ref().map(ChannelLifecycle::auth)
    }
}

fn ephemeral_close() -> ChannelError {
    ChannelError::BadRequest("ephemeral channel streams cannot be closed".into())
}

/// Check a publication against the publisher's channel definition: params as
/// subscribe params are checked, and a close only on a durable channel.
pub fn validate_publication(
    definition: &ChannelDefinitionMeta,
    publication: &TrustedPublication,
    params: &Value,
) -> Result<(), ChannelError> {
    if publication.is_close() && definition.lifecycle.ephemeral {
        return Err(ephemeral_close());
    }
    validate_publish_params(definition, params)
}

/// Check publish params against the publisher's channel definition, as
/// subscribe params are checked, so a mistyped binding is rejected instead of
/// starting a stream nobody reads. Stores validate their own scope params.
pub fn validate_publish_params(
    definition: &ChannelDefinitionMeta,
    params: &Value,
) -> Result<(), ChannelError> {
    if definition.channel.starts_with(STORE_CHANNEL_PREFIX) {
        return Ok(());
    }
    validate_value(
        &definition.params_schema,
        &crate::stream::bound_params(params),
    )
    .map_err(|error| match error {
        ParamsError::Invalid(message) => {
            ChannelError::BadRequest(format!("invalid channel params: {message}"))
        }
        ParamsError::InvalidSchema(message) => {
            ChannelError::Storage(format!("invalid channel schema: {message}"))
        }
    })
}

impl ChannelStore {
    /// Apply a trusted publication to this store. The returned message names
    /// the public channel, never the stream key.
    pub fn publish_trusted(
        &self,
        publication: &TrustedPublication,
    ) -> Result<ChannelMessage, ChannelError> {
        let key = publication.stream.key();
        let message = if publication.ephemeral() {
            self.publish_ephemeral(key, &publication.payload)?
        } else {
            self.append_with_lifecycle(key, &publication.payload, publication.lifecycle.as_ref())?
        };
        Ok(publication.stream.public(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn definition(channel: &str) -> ChannelDefinitionMeta {
        serde_json::from_value(json!({
            "channel": channel,
            "paramsSchema": {
                "type": "object",
                "properties": {"room": {"type": "string", "minLength": 1}, "page": {"type": "integer"}},
                "required": ["room"],
                "additionalProperties": false
            },
            "auth": false
        }))
        .unwrap()
    }

    #[test]
    fn publication_binds_command_params_and_returns_the_public_channel() {
        let store = ChannelStore::open_in_memory().unwrap();
        let payload = json!({"type": "msg", "data": 1, "lifecycle": {"replayWindowMs": 1000}});
        let a = TrustedPublication::parse("chat", &json!({"room": "a"}), payload.clone()).unwrap();
        let b = TrustedPublication::parse("chat", &json!({"room": "b"}), payload.clone()).unwrap();
        assert_eq!(a.stream, ChannelStream::new("chat", &json!({"room": "a"})));
        assert_eq!(a.lifecycle.as_ref().unwrap().replay_window_ms, 1000);
        let published = store.publish_trusted(&a).unwrap();
        assert_eq!(published.channel, "chat");
        assert_eq!(
            store.read_after(a.stream.key(), None, 10).unwrap()[0].id,
            published.id
        );
        assert!(
            store
                .read_after(b.stream.key(), None, 10)
                .unwrap()
                .is_empty()
        );
        assert!(store.read_after("chat", None, 10).unwrap().is_empty());
        assert_eq!(
            store.raw_query_i64(
                "SELECT replay_window_ms FROM channel_metadata WHERE channel = ?1",
                [a.stream.key()]
            ),
            1000
        );
    }

    #[test]
    fn publication_rejects_untyped_params_and_stream_keys() {
        let payload = json!({"type": "msg", "data": 1});
        let key = ChannelStream::new("chat", &json!({"room": "a"}))
            .key()
            .to_owned();
        assert!(TrustedPublication::parse("chat", &json!(["a"]), payload.clone()).is_err());
        assert!(TrustedPublication::parse(&key, &json!({}), payload.clone()).is_err());
        assert!(TrustedPublication::parse("chat", &json!({}), json!({"data": 1})).is_err());
        assert!(
            TrustedPublication::parse(
                "chat",
                &json!({}),
                json!({"type": "msg", "data": 1, "lifecycle": {"ephemeral": true, "replayWindowMs": 5}})
            )
            .is_err()
        );
    }

    #[test]
    fn store_publications_keep_their_payload_scope() {
        let payload =
            json!({"type": "tako.store.invalidate", "data": {}, "scopeParams": {"chat": "a"}});
        let publication =
            TrustedPublication::parse("tako.store:messages", &Value::Null, payload).unwrap();
        assert_eq!(
            publication.stream.key(),
            crate::live_store::store_scope("tako.store:messages", &json!({"chat": "a"}))
        );
        assert_eq!(publication.stream.channel(), "tako.store:messages");
    }

    #[test]
    fn publish_params_follow_the_subscribe_schema() {
        let chat = definition("chat");
        for params in [json!({"room": "a"}), json!({"room": "a", "page": 2})] {
            validate_publish_params(&chat, &params).unwrap();
        }
        for params in [
            json!({}),
            Value::Null,
            json!({"rom": "a"}),
            json!({"room": ""}),
            json!({"room": "a", "page": "2"}),
        ] {
            assert!(
                matches!(
                    validate_publish_params(&chat, &params),
                    Err(ChannelError::BadRequest(message)) if message.starts_with("invalid channel params")
                ),
                "{params}"
            );
        }
        validate_publish_params(&definition("tako.store:messages"), &json!({"other": 1})).unwrap();
    }
}

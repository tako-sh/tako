//! One replay log, cursor space and fanout per (channel, validated params).

use crate::{ChannelError, ChannelMessage, ChannelPublishPayload, PRESENCE_CHANNEL_PREFIX};
use serde_json::Value;

/// Storage namespace of parameterized streams. Never a public channel name.
pub const STREAM_CHANNEL_PREFIX: &str = "tako.stream:";
const MAX_STREAM_PARAMS_BYTES: usize = 16 << 10;
/// Message type of a stream's durable terminal marker. Only a close appends
/// one; it is never published by apps or delivered as a message.
pub const CLOSE_MESSAGE_TYPE: &str = "tako.close";
/// Query keys that select where a reader starts. No channel param may use them.
pub const RESERVED_PARAM_NAMES: [&str; 2] = ["last_message_id", "from"];

/// The terminal marker a close appends.
pub(crate) fn close_payload() -> ChannelPublishPayload {
    ChannelPublishPayload {
        r#type: CLOSE_MESSAGE_TYPE.into(),
        data: Value::Null,
    }
}

/// Whether a stored or replicated message is a stream's terminal marker.
pub fn is_close_marker(message: &ChannelMessage) -> bool {
    message.r#type == CLOSE_MESSAGE_TYPE
}

/// Canonical scope of a wire name and its params. Object keys are sorted and
/// integral floats become integers: query coercion may produce 1.0 where an
/// SDK sends 1.
pub(crate) fn params_scope(channel: &str, params: &Value) -> String {
    fn normalize(value: &mut Value) {
        match value {
            Value::Object(object) => object.values_mut().for_each(normalize),
            Value::Array(values) => values.iter_mut().for_each(normalize),
            Value::Number(number) if number.is_f64() => {
                let value = number.as_f64().expect("JSON numbers are finite");
                if value.fract() == 0.0 {
                    if value >= i64::MIN as f64 && value < i64::MAX as f64 {
                        *number = (value as i64).into();
                    } else if value >= 0.0 && value < u64::MAX as f64 {
                        *number = (value as u64).into();
                    }
                }
            }
            _ => {}
        }
    }
    let mut params = params.clone();
    normalize(&mut params);
    crate::presence_scope(channel, &params)
}

/// Params as a subscriber's query string can express them: an object without
/// null entries. Publishers may spell an absent param as null.
pub(crate) fn bound_params(params: &Value) -> Value {
    match params {
        Value::Null => Value::Object(Default::default()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A channel stream: the public channel name and the opaque key that storage,
/// replay cursors, fanout and replication use for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelStream {
    channel: String,
    key: String,
}

impl ChannelStream {
    /// Bind validated params. A binding without params is the channel's single
    /// unscoped stream. Query strings cannot carry null, so a null param names
    /// the same stream as an absent one. Presence spaces keep one transport
    /// stream: their membership is scoped separately.
    pub fn new(channel: &str, params: &Value) -> Self {
        let params = bound_params(params);
        let unscoped = match &params {
            Value::Object(object) => object.is_empty(),
            _ => true,
        };
        let key = if unscoped || channel.starts_with(PRESENCE_CHANNEL_PREFIX) {
            channel.to_owned()
        } else {
            format!("{STREAM_CHANNEL_PREFIX}{}", params_scope(channel, &params))
        };
        Self {
            channel: channel.to_owned(),
            key,
        }
    }

    /// Bind params supplied by a server-side publisher.
    pub fn for_publish(channel: &str, params: &Value) -> Result<Self, ChannelError> {
        if reserved_stream_name(channel) {
            return Err(ChannelError::BadRequest("invalid channel name".into()));
        }
        let valid = match params {
            Value::Null => true,
            Value::Object(_) => {
                serde_json::to_vec(params).is_ok_and(|bytes| bytes.len() <= MAX_STREAM_PARAMS_BYTES)
            }
            _ => false,
        };
        if !valid {
            return Err(ChannelError::BadRequest(
                "channel params must be an object of at most 16 KiB".into(),
            ));
        }
        // Subscribers cannot send these as params, so no reader could bind
        // the stream a publish to them would start.
        if let Some(name) = RESERVED_PARAM_NAMES
            .iter()
            .find(|name| params.get(**name).is_some_and(|value| !value.is_null()))
        {
            return Err(ChannelError::BadRequest(format!(
                "channel params cannot use the reserved name '{name}'"
            )));
        }
        Ok(Self::new(channel, params))
    }

    /// A stream whose storage key was derived elsewhere, such as a Store scope.
    pub fn with_key(channel: &str, key: String) -> Self {
        Self {
            channel: channel.to_owned(),
            key,
        }
    }

    /// The channel name clients and the app use.
    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// The storage key. Internal: never send it to clients.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Present a stored or fanned-out message under the public channel name.
    pub fn public(&self, mut message: ChannelMessage) -> ChannelMessage {
        if message.channel != self.channel {
            message.channel.clone_from(&self.channel);
        }
        message
    }
}

/// Stream keys are an internal namespace: no route or publisher may name one.
pub fn reserved_stream_name(channel: &str) -> bool {
    channel.starts_with(STREAM_CHANNEL_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bindings_without_params_share_the_unscoped_stream() {
        for params in [json!({}), Value::Null, json!({"room": null})] {
            let stream = ChannelStream::new("chat", &params);
            assert_eq!(stream.key(), "chat");
            assert_eq!(stream.channel(), "chat");
        }
    }

    #[test]
    fn params_select_distinct_opaque_streams() {
        let a = ChannelStream::new("chat", &json!({"room": "a"}));
        let b = ChannelStream::new("chat", &json!({"room": "b"}));
        let other = ChannelStream::new("notes", &json!({"room": "a"}));
        assert_ne!(a.key(), b.key());
        assert_ne!(a.key(), other.key());
        for stream in [&a, &b, &other] {
            assert!(reserved_stream_name(stream.key()));
            assert!(!stream.key().contains("room"));
            assert_eq!(
                stream.key().len(),
                STREAM_CHANNEL_PREFIX.len() + 64,
                "fixed-size key"
            );
        }
        assert_eq!(a.channel(), "chat");
    }

    #[test]
    fn canonical_params_ignore_key_order_null_and_integral_floats() {
        let stream = ChannelStream::new("chat", &json!({"a": 1, "b": 2}));
        for params in [
            json!({"b": 2, "a": 1}),
            json!({"a": 1.0, "b": 2.0}),
            json!({"a": 1, "b": 2, "c": null}),
        ] {
            assert_eq!(ChannelStream::new("chat", &params), stream, "{params}");
        }
        for params in [
            json!({"a": "1", "b": 2}),
            json!({"a": 1.5, "b": 2}),
            json!({"a": 1}),
        ] {
            assert_ne!(ChannelStream::new("chat", &params), stream, "{params}");
        }
    }

    #[test]
    fn stream_scope_matches_store_and_presence_canonicalization() {
        let params = json!({"b": 2.0, "a": {"y": 1.0, "x": [3.0]}});
        let sorted = json!({"a": {"x": [3], "y": 1}, "b": 2});
        assert_eq!(
            params_scope("chat", &params),
            crate::presence_scope("chat", &sorted)
        );
        assert_eq!(
            crate::live_store::store_scope("tako.store:chat", &params),
            format!(
                "{}{}",
                crate::live_store::STORE_CHANNEL_PREFIX,
                params_scope("tako.store:chat", &params)
            )
        );
    }

    #[test]
    fn presence_spaces_keep_one_transport_stream() {
        let stream = ChannelStream::new("tako.presence:room", &json!({"room": "a"}));
        assert_eq!(stream.key(), "tako.presence:room");
    }

    #[test]
    fn publishers_cannot_name_a_stream_key_or_send_untyped_params() {
        let key = ChannelStream::new("chat", &json!({"room": "a"}))
            .key()
            .to_owned();
        assert!(ChannelStream::for_publish(&key, &json!({})).is_err());
        for params in [json!([]), json!("a"), json!(1)] {
            assert!(ChannelStream::for_publish("chat", &params).is_err());
        }
        for name in RESERVED_PARAM_NAMES {
            assert!(ChannelStream::for_publish("chat", &json!({ name: "start" })).is_err());
        }
        let large = json!({"room": "a".repeat(MAX_STREAM_PARAMS_BYTES)});
        assert!(ChannelStream::for_publish("chat", &large).is_err());
        assert_eq!(
            ChannelStream::for_publish("chat", &Value::Null).unwrap(),
            ChannelStream::new("chat", &json!({}))
        );
        assert_eq!(
            ChannelStream::for_publish("chat", &json!({"room": "a"})).unwrap(),
            ChannelStream::new("chat", &json!({"room": "a"}))
        );
    }

    #[test]
    fn public_messages_never_carry_the_stream_key() {
        let stream = ChannelStream::new("chat", &json!({"room": "a"}));
        let message = stream.public(ChannelMessage {
            id: "1".into(),
            event_id: None,
            channel: stream.key().into(),
            r#type: "msg".into(),
            data: json!(1),
        });
        assert_eq!(message.channel, "chat");
        assert!(
            !serde_json::to_string(&message)
                .unwrap()
                .contains("tako.stream")
        );
    }
}

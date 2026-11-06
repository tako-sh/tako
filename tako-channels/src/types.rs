use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_REPLAY_WINDOW_MS: u64 = 10 * 60 * 1000;
pub(crate) const DEFAULT_INACTIVITY_TTL_MS: u64 = 0;
pub(crate) const DEFAULT_KEEPALIVE_INTERVAL_MS: u64 = 25 * 1000;
pub(crate) const DEFAULT_MAX_CONNECTION_LIFETIME_MS: u64 = 2 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelOperation {
    Publish,
    Subscribe,
    Connect,
}

impl ChannelOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Publish => "publish",
            Self::Subscribe => "subscribe",
            Self::Connect => "connect",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRoute {
    pub channel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelPublishPayload {
    pub r#type: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelMessage {
    pub id: String,
    #[serde(default, rename = "eventId", skip_serializing_if = "Option::is_none")]
    pub event_id: Option<crate::ChannelEventId>,
    pub channel: String,
    pub r#type: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelHeaderValue {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    pub value: String,
}

impl ChannelHeaderValue {
    pub fn parse(raw: &str) -> Self {
        if let Some(idx) = raw.find(' ') {
            Self {
                scheme: Some(raw[..idx].to_string()),
                value: raw[idx + 1..].to_string(),
            }
        } else {
            Self {
                scheme: None,
                value: raw.to_string(),
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelAuthVerifyRequest {
    pub channel: String,
    pub operation: String,
    pub params: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<ChannelHeaderValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookie: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChannelTransport {
    Ws,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelAuthScheme {
    Required {
        header_name: Option<String>,
        cookie_name: Option<String>,
    },
    Public,
}

impl Serialize for ChannelAuthScheme {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Public => serializer.serialize_bool(false),
            Self::Required {
                header_name,
                cookie_name,
            } => {
                use serde::ser::SerializeMap;

                let len = usize::from(header_name.is_some()) + usize::from(cookie_name.is_some());
                let mut map = serializer.serialize_map(Some(len))?;
                if let Some(value) = header_name {
                    map.serialize_entry("headerName", value)?;
                }
                if let Some(value) = cookie_name {
                    map.serialize_entry("cookieName", value)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ChannelAuthScheme {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Bool(false) => Ok(Self::Public),
            serde_json::Value::Object(map) => Ok(Self::Required {
                header_name: map
                    .get("headerName")
                    .and_then(|value| value.as_str())
                    .map(String::from),
                cookie_name: map
                    .get("cookieName")
                    .and_then(|value| value.as_str())
                    .map(String::from),
            }),
            _ => Err(serde::de::Error::custom("expected false or object")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelDefinitionMeta {
    pub channel: String,
    #[serde(rename = "paramsSchema")]
    pub params_schema: serde_json::Value,
    pub auth: ChannelAuthScheme,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<ChannelTransport>,
    #[serde(flatten)]
    pub lifecycle: ChannelLifecycle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "ChannelLifecycleWire", into = "ChannelLifecycleWire")]
pub struct ChannelLifecycle {
    pub ephemeral: bool,
    pub replay_window_ms: u64,
    pub inactivity_ttl_ms: u64,
    pub keepalive_interval_ms: u64,
    pub max_connection_lifetime_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChannelLifecycleWire {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    ephemeral: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replay_window_ms: Option<u64>,
    #[serde(default = "default_inactivity_ttl_ms")]
    inactivity_ttl_ms: u64,
    #[serde(default = "default_keepalive_interval_ms")]
    keepalive_interval_ms: u64,
    #[serde(default = "default_max_connection_lifetime_ms")]
    max_connection_lifetime_ms: u64,
}

impl TryFrom<ChannelLifecycleWire> for ChannelLifecycle {
    type Error = &'static str;
    fn try_from(wire: ChannelLifecycleWire) -> Result<Self, Self::Error> {
        if wire.ephemeral && wire.replay_window_ms.is_some() {
            return Err("ephemeral channels cannot set replayWindowMs");
        }
        Ok(Self {
            ephemeral: wire.ephemeral,
            replay_window_ms: wire.replay_window_ms.unwrap_or(DEFAULT_REPLAY_WINDOW_MS),
            inactivity_ttl_ms: wire.inactivity_ttl_ms,
            keepalive_interval_ms: wire.keepalive_interval_ms,
            max_connection_lifetime_ms: wire.max_connection_lifetime_ms,
        })
    }
}

impl From<ChannelLifecycle> for ChannelLifecycleWire {
    fn from(value: ChannelLifecycle) -> Self {
        Self {
            ephemeral: value.ephemeral,
            replay_window_ms: (!value.ephemeral).then_some(value.replay_window_ms),
            inactivity_ttl_ms: value.inactivity_ttl_ms,
            keepalive_interval_ms: value.keepalive_interval_ms,
            max_connection_lifetime_ms: value.max_connection_lifetime_ms,
        }
    }
}

impl Default for ChannelLifecycle {
    fn default() -> Self {
        Self {
            ephemeral: false,
            replay_window_ms: DEFAULT_REPLAY_WINDOW_MS,
            inactivity_ttl_ms: DEFAULT_INACTIVITY_TTL_MS,
            keepalive_interval_ms: DEFAULT_KEEPALIVE_INTERVAL_MS,
            max_connection_lifetime_ms: DEFAULT_MAX_CONNECTION_LIFETIME_MS,
        }
    }
}

impl ChannelDefinitionMeta {
    pub fn lifecycle_auth(&self) -> ChannelAuthResponse {
        ChannelAuthResponse {
            ok: true,
            subject: None,
            transport: self.transport.clone(),
            ephemeral: self.lifecycle.ephemeral,
            replay_window_ms: self.lifecycle.replay_window_ms,
            inactivity_ttl_ms: self.lifecycle.inactivity_ttl_ms,
            keepalive_interval_ms: self.lifecycle.keepalive_interval_ms,
            max_connection_lifetime_ms: self.lifecycle.max_connection_lifetime_ms,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(from = "ChannelAuthWire")]
pub struct ChannelAuthResponse {
    pub ephemeral: bool,
    pub ok: bool,
    pub subject: Option<String>,
    pub transport: Option<ChannelTransport>,
    pub replay_window_ms: u64,
    pub inactivity_ttl_ms: u64,
    pub keepalive_interval_ms: u64,
    pub max_connection_lifetime_ms: u64,
}

#[derive(Deserialize)]
struct ChannelAuthWire {
    ok: bool,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    transport: Option<ChannelTransport>,
    #[serde(flatten)]
    lifecycle: ChannelLifecycle,
}

impl From<ChannelAuthWire> for ChannelAuthResponse {
    fn from(wire: ChannelAuthWire) -> Self {
        Self {
            ok: wire.ok,
            subject: wire.subject,
            transport: wire.transport,
            ephemeral: wire.lifecycle.ephemeral,
            replay_window_ms: wire.lifecycle.replay_window_ms,
            inactivity_ttl_ms: wire.lifecycle.inactivity_ttl_ms,
            keepalive_interval_ms: wire.lifecycle.keepalive_interval_ms,
            max_connection_lifetime_ms: wire.lifecycle.max_connection_lifetime_ms,
        }
    }
}

fn default_inactivity_ttl_ms() -> u64 {
    DEFAULT_INACTIVITY_TTL_MS
}

fn default_keepalive_interval_ms() -> u64 {
    DEFAULT_KEEPALIVE_INTERVAL_MS
}

fn default_max_connection_lifetime_ms() -> u64 {
    DEFAULT_MAX_CONNECTION_LIFETIME_MS
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("invalid channel path")]
    InvalidPath,
    #[error("unsupported channel operation")]
    Unsupported,
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("channel not defined")]
    NotDefined,
    #[error("requested channel cursor is outside the replay window")]
    StaleCursor,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("channel auth unavailable")]
    AuthUnavailable,
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<rusqlite::Error> for ChannelError {
    fn from(e: rusqlite::Error) -> Self {
        ChannelError::Storage(e.to_string())
    }
}

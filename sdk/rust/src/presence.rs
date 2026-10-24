//! Presence definitions and authorization for Tako's managed WebSocket transport.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

pub const PRESENCE_CHANNEL_PREFIX: &str = "tako.presence:";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceMember<T = Value> {
    pub connection_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub data: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceSnapshot<T = Value> {
    pub r#type: String,
    pub connection_id: String,
    pub members: Vec<PresenceMember<T>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceHeaderValue {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PresenceVerifyInput {
    /// The public presence name in verifier callbacks; the wire name on internal requests.
    pub channel: String,
    pub operation: String,
    /// Params validated by the Tako proxy before authorization.
    pub params: Value,
    pub header: Option<PresenceHeaderValue>,
    pub cookie: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PresenceGrant {
    pub subject: Option<String>,
}

impl PresenceGrant {
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: Some(subject.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceAuth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookie_name: Option<String>,
}

impl Default for PresenceAuth {
    fn default() -> Self {
        Self {
            header_name: Some("authorization".into()),
            cookie_name: None,
        }
    }
}

type VerifyFuture = Pin<Box<dyn Future<Output = Option<PresenceGrant>> + Send>>;
type Verifier = Arc<dyn Fn(PresenceVerifyInput) -> VerifyFuture + Send + Sync>;

#[derive(Clone)]
pub struct PresenceDefinition {
    params_schema: Value,
    auth: Option<PresenceAuth>,
    verify: Option<Verifier>,
    keepalive_interval_ms: u64,
    max_connection_lifetime_ms: u64,
}

impl Default for PresenceDefinition {
    fn default() -> Self {
        Self {
            params_schema: json!({"type":"object"}),
            auth: None,
            verify: None,
            keepalive_interval_ms: 25_000,
            max_connection_lifetime_ms: 7_200_000,
        }
    }
}

impl PresenceDefinition {
    /// Create a public presence space. Add auth() to restrict membership visibility.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn params_schema(mut self, schema: Value) -> Self {
        self.params_schema = schema;
        self
    }

    /// Return None to deny, or a grant with the server-verified identity to allow.
    pub fn auth<F, Fut>(mut self, auth: PresenceAuth, verify: F) -> Self
    where
        F: Fn(PresenceVerifyInput) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<PresenceGrant>> + Send + 'static,
    {
        self.auth = Some(auth);
        self.verify = Some(Arc::new(move |input| Box::pin(verify(input))));
        self
    }

    pub fn keepalive_interval_ms(mut self, milliseconds: u64) -> Self {
        self.keepalive_interval_ms = milliseconds;
        self
    }

    /// Bound how long an authorized socket can remain open before reauthorizing.
    pub fn max_connection_lifetime_ms(mut self, milliseconds: u64) -> Self {
        self.max_connection_lifetime_ms = milliseconds;
        self
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceAuthorization {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub transport: &'static str,
    pub keepalive_interval_ms: u64,
    pub max_connection_lifetime_ms: u64,
}

#[derive(Default)]
pub struct PresenceRegistry {
    definitions: RwLock<BTreeMap<String, PresenceDefinition>>,
}

impl PresenceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or replace a space. Names are separate from ordinary channel names.
    pub fn register(&self, name: impl Into<String>, definition: PresenceDefinition) {
        self.definitions.write().unwrap().insert(
            format!("{PRESENCE_CHANNEL_PREFIX}{}", name.into()),
            definition,
        );
    }

    /// Metadata for the authenticated GET /channels/registry internal endpoint.
    pub fn metadata(&self) -> Vec<Value> {
        self.definitions.read().unwrap().iter().map(|(name, definition)| json!({
            "channel": name, "paramsSchema": definition.params_schema,
            "auth": definition.auth.as_ref().map_or(Value::Bool(false), |auth| serde_json::to_value(auth).unwrap()),
            "transport": "ws", "keepaliveIntervalMs": definition.keepalive_interval_ms,
            "maxConnectionLifetimeMs": definition.max_connection_lifetime_ms,
        })).collect()
    }

    /// Authorize an internal request. None means the wire name is not registered.
    pub async fn authorize(&self, mut input: PresenceVerifyInput) -> Option<PresenceAuthorization> {
        let definition = self
            .definitions
            .read()
            .unwrap()
            .get(&input.channel)
            .cloned()?;
        input.channel = input
            .channel
            .strip_prefix(PRESENCE_CHANNEL_PREFIX)?
            .to_owned();
        let grant = match &definition.verify {
            Some(verify) => verify(input).await,
            None => Some(PresenceGrant::default()),
        };
        Some(PresenceAuthorization {
            ok: grant.is_some(),
            subject: grant.and_then(|grant| grant.subject),
            transport: "ws",
            keepalive_interval_ms: definition.keepalive_interval_ms,
            max_connection_lifetime_ms: definition.max_connection_lifetime_ms,
        })
    }
}

#[cfg(test)]
mod tests;

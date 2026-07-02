use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::Duration;

use crate::{
    ChannelAuthResponse, ChannelAuthVerifyRequest, ChannelError, ChannelHeaderValue,
    ChannelOperation, ChannelPublishPayload, INTERNAL_CHANNEL_AUTH_PATH,
    INTERNAL_CHANNEL_DISPATCH_PATH,
};

/// Shared HTTP client for internal app requests. Auth runs on every channel
/// connect/publish, so reuse one client (and its connection pool) instead of
/// building a new one per request.
fn shared_client() -> Result<&'static reqwest::Client, ChannelError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| ChannelError::Storage(format!("build internal http client: {e}")))?;
    Ok(CLIENT.get_or_init(|| client))
}

/// Authorize a channel operation by calling the app's internal endpoint.
///
/// `endpoint` is the app's `host:port` (e.g. `127.0.0.1:3000`).
/// `internal_host` is the Host header for internal requests (e.g. `app.tako`).
/// `internal_token` is the shared secret for the internal token header.
#[allow(clippy::too_many_arguments)]
pub async fn authorize_channel_request(
    endpoint: &str,
    internal_host: &str,
    internal_token_header: &str,
    internal_token: &str,
    operation: ChannelOperation,
    channel: &str,
    params: serde_json::Value,
    header: Option<ChannelHeaderValue>,
    cookie: Option<String>,
) -> Result<ChannelAuthResponse, ChannelError> {
    let response = shared_client()?
        .post(format!("http://{endpoint}{INTERNAL_CHANNEL_AUTH_PATH}"))
        .header("Host", internal_host)
        .header(internal_token_header, internal_token)
        .json(&ChannelAuthVerifyRequest {
            channel: channel.to_string(),
            operation: operation.as_str().to_string(),
            params,
            header,
            cookie,
        })
        .send()
        .await
        .map_err(|_| ChannelError::AuthUnavailable)?;

    match response.status().as_u16() {
        200 => response
            .json::<ChannelAuthResponse>()
            .await
            .map_err(|e| ChannelError::BadRequest(format!("invalid auth response: {e}"))),
        403 => Err(ChannelError::Forbidden),
        404 => Err(ChannelError::NotDefined),
        405 => Ok(ChannelAuthResponse::denied_with_defaults()),
        _ => Err(ChannelError::AuthUnavailable),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelDispatchRequest {
    pub channel: String,
    pub frame: ChannelPublishPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum ChannelDispatchResponse {
    Fanout {
        data: serde_json::Value,
    },
    Drop {
        #[serde(default)]
        error: Option<String>,
    },
    Reject {
        reason: String,
    },
}

/// Dispatch a client-initiated WS frame through the app's declared
/// per-channel handler. Returns the action to take: fanout the returned
/// data, drop the message, or reject (reason-coded) the connection.
pub async fn dispatch_channel_message(
    endpoint: &str,
    internal_host: &str,
    internal_token_header: &str,
    internal_token: &str,
    request: ChannelDispatchRequest,
) -> Result<ChannelDispatchResponse, ChannelError> {
    let response = shared_client()?
        .post(format!("http://{endpoint}{INTERNAL_CHANNEL_DISPATCH_PATH}"))
        .header("Host", internal_host)
        .header(internal_token_header, internal_token)
        .json(&request)
        .send()
        .await
        .map_err(|_| ChannelError::AuthUnavailable)?;

    match response.status().as_u16() {
        200 => response
            .json::<ChannelDispatchResponse>()
            .await
            .map_err(|e| ChannelError::BadRequest(format!("invalid dispatch response: {e}"))),
        403 => Err(ChannelError::Forbidden),
        404 => Err(ChannelError::NotDefined),
        _ => Err(ChannelError::AuthUnavailable),
    }
}

use std::sync::OnceLock;
use std::time::Duration;

use crate::{
    ChannelAuthResponse, ChannelAuthVerifyRequest, ChannelError, ChannelHeaderValue,
    ChannelOperation, INTERNAL_CHANNEL_AUTH_PATH,
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

    match map_channel_auth_http_status(response.status().as_u16()) {
        Ok(()) => {
            let auth = response
                .json::<ChannelAuthResponse>()
                .await
                .map_err(|e| ChannelError::BadRequest(format!("invalid auth response: {e}")))?;
            accept_channel_auth(auth)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn map_channel_auth_http_status(status: u16) -> Result<(), ChannelError> {
    match status {
        200 => Ok(()),
        403 | 405 => Err(ChannelError::Forbidden),
        404 => Err(ChannelError::NotDefined),
        _ => Err(ChannelError::AuthUnavailable),
    }
}

pub(crate) fn accept_channel_auth(
    auth: ChannelAuthResponse,
) -> Result<ChannelAuthResponse, ChannelError> {
    if auth.ok {
        Ok(auth)
    } else {
        Err(ChannelError::Forbidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denied_http_statuses_are_errors() {
        assert!(matches!(
            map_channel_auth_http_status(403),
            Err(ChannelError::Forbidden)
        ));
        assert!(matches!(
            map_channel_auth_http_status(405),
            Err(ChannelError::Forbidden)
        ));
        assert!(matches!(
            map_channel_auth_http_status(404),
            Err(ChannelError::NotDefined)
        ));
        assert!(map_channel_auth_http_status(200).is_ok());
    }

    #[test]
    fn denied_auth_body_is_forbidden() {
        assert!(matches!(
            accept_channel_auth(ChannelAuthResponse {
                ok: false,
                subject: None,
                transport: None,
                ephemeral: false,
                replay_window_ms: 0,
                inactivity_ttl_ms: 0,
                keepalive_interval_ms: 0,
                max_connection_lifetime_ms: 0,
            }),
            Err(ChannelError::Forbidden)
        ));
    }
}

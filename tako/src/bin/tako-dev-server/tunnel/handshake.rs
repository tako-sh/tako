use super::{TUNNEL_CONNECT_TIMEOUT, TunnelCloseReason, TunnelConnectError};
use crate::tunnel_wire::{MAX_WIRE_MESSAGE, TUNNEL_PROTOCOL};
use reqwest::header::HeaderValue;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        client::IntoClientRequest, handshake::derive_accept_key, http::StatusCode, protocol::Role,
    },
};

pub(super) async fn connect_tunnel_socket(
    connect_url: &str,
) -> Result<WebSocketStream<reqwest::Upgraded>, TunnelConnectError> {
    // Use the same transport as registration: it honors HTTP(S)_PROXY,
    // ALL_PROXY, NO_PROXY, and the platform certificate store. Tungstenite's
    // connect_async opens a direct TCP socket and bypasses those settings.
    let client = reqwest::Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(TUNNEL_CONNECT_TIMEOUT)
        .build()
        .map_err(|error| TunnelConnectError::Retry(error.to_string()))?;
    tokio::time::timeout(
        TUNNEL_CONNECT_TIMEOUT,
        connect_tunnel_socket_with_client(connect_url, &client),
    )
    .await
    .map_err(|_| TunnelConnectError::Retry("timed out connecting tunnel websocket".to_string()))?
}

pub(super) async fn connect_tunnel_socket_with_client(
    connect_url: &str,
    client: &reqwest::Client,
) -> Result<WebSocketStream<reqwest::Upgraded>, TunnelConnectError> {
    let mut request = connect_url
        .into_client_request()
        .map_err(|error| TunnelConnectError::Retry(error.to_string()))?;
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static(TUNNEL_PROTOCOL),
    );
    let accept_key = derive_accept_key(request.headers()["sec-websocket-key"].as_bytes());
    let mut url = reqwest::Url::parse(connect_url)
        .map_err(|error| TunnelConnectError::Retry(error.to_string()))?;
    let scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        _ => {
            return Err(TunnelConnectError::Retry(
                "invalid tunnel websocket scheme".into(),
            ));
        }
    };
    url.set_scheme(scheme)
        .map_err(|_| TunnelConnectError::Retry("invalid tunnel websocket URL".into()))?;
    let response = client
        .get(url)
        .headers(request.headers().clone())
        .send()
        .await
        .map_err(|error| {
            // The connection URL contains a session credential.
            TunnelConnectError::Retry(format!(
                "failed to connect tunnel websocket: {}",
                error.without_url()
            ))
        })?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let status = response.status();
        let body = read_tunnel_error_body(response).await;
        return Err(classify_tunnel_connect_error(status, body.as_deref()));
    }
    validate_tunnel_upgrade(response.headers(), &accept_key)?;
    let stream = response.upgrade().await.map_err(|error| {
        TunnelConnectError::Retry(format!(
            "failed to upgrade tunnel websocket: {}",
            error.without_url()
        ))
    })?;
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_WIRE_MESSAGE))
        .max_frame_size(Some(MAX_WIRE_MESSAGE));
    Ok(WebSocketStream::from_raw_socket(stream, Role::Client, Some(config)).await)
}

// The body is only mined for a short JSON error message, and reading it to the
// end would buffer however much a misbehaving relay or intermediary streams
// within the connect timeout.
const MAX_TUNNEL_ERROR_BODY: usize = 64 * 1024;

async fn read_tunnel_error_body(mut response: reqwest::Response) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    while body.len() < MAX_TUNNEL_ERROR_BODY {
        match response.chunk().await {
            Ok(Some(chunk)) => body
                .extend_from_slice(&chunk[..chunk.len().min(MAX_TUNNEL_ERROR_BODY - body.len())]),
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    Some(body)
}

pub(super) fn validate_tunnel_upgrade(
    headers: &reqwest::header::HeaderMap,
    accept_key: &str,
) -> Result<(), TunnelConnectError> {
    let upgrade = headers.get("upgrade").and_then(|value| value.to_str().ok());
    let connection_upgrade = headers.get_all("connection").iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
    });
    if !upgrade.is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        || !connection_upgrade
        || headers
            .get("sec-websocket-accept")
            .is_none_or(|value| value != accept_key)
        || headers.contains_key("sec-websocket-extensions")
    {
        return Err(TunnelConnectError::Retry(
            "invalid tunnel websocket upgrade response".into(),
        ));
    }
    if headers
        .get("sec-websocket-protocol")
        .is_none_or(|value| value != TUNNEL_PROTOCOL)
    {
        return Err(TunnelConnectError::Retry(
            "tunnel protocol mismatch; update Tako and the relay together".into(),
        ));
    }
    Ok(())
}

pub(super) fn classify_tunnel_connect_error(
    status: StatusCode,
    body: Option<&[u8]>,
) -> TunnelConnectError {
    let reason = match status {
        StatusCode::UNAUTHORIZED => TunnelCloseReason::Replaced,
        StatusCode::TOO_MANY_REQUESTS => TunnelCloseReason::LimitExceeded,
        _ => {
            return TunnelConnectError::Retry(format!(
                "tunnel service refused connection ({status})"
            ));
        }
    };
    TunnelConnectError::Terminal {
        reason,
        message: tunnel_http_error_message(status, body),
    }
}

fn tunnel_http_error_message(status: StatusCode, body: Option<&[u8]>) -> String {
    let message = body
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok())
        .and_then(|json| {
            json.get("error")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        });
    match message {
        Some(message) => format!("tunnel service refused connection ({status}): {message}"),
        None => format!("tunnel service refused connection ({status})"),
    }
}

//! Request-side channel authorization shared by the production proxy and
//! `tako dev`: which credentials a request forwards to the app's authorize
//! endpoint, the `tako.auth` first WebSocket frame, and the HTTP mapping of
//! channel errors.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use pingora_core::prelude::*;
use pingora_http::RequestHeader;
use pingora_proxy::Session;

use crate::close_codes::ChannelCloseCode;
use crate::websocket::{
    WebSocketFrameReader, parse_first_frame, websocket_close_frame, websocket_pong_frame,
};
use crate::{
    ChannelAuthResponse, ChannelAuthScheme, ChannelError, ChannelHeaderValue, ChannelOperation,
    ChannelTransport, ReplayFrom,
};

/// How long a header-authenticated socket may take to send its `tako.auth` frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// The app instance whose internal authorize endpoint decides a request.
#[derive(Debug, Clone)]
pub struct AppAuthorizer {
    /// The app's `host:port`.
    pub endpoint: String,
    /// Host header for internal requests (e.g. `app.tako`).
    pub internal_host: String,
    pub internal_token_header: &'static str,
    pub internal_token: String,
}

impl AppAuthorizer {
    pub async fn authorize(
        &self,
        operation: ChannelOperation,
        channel: &str,
        params: serde_json::Value,
        header: Option<ChannelHeaderValue>,
        cookie: Option<String>,
    ) -> Result<ChannelAuthResponse, ChannelError> {
        crate::authorize_channel_request(
            &self.endpoint,
            &self.internal_host,
            self.internal_token_header,
            &self.internal_token,
            operation,
            channel,
            params,
            header,
            cookie,
        )
        .await
    }
}

/// Credentials a channel request forwards to the app.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestCredentials {
    pub header: Option<ChannelHeaderValue>,
    pub cookie: Option<String>,
    /// A header-authenticated socket without the header sends it in its first
    /// `tako.auth` frame; authorization then runs after the upgrade.
    pub first_frame: bool,
}

/// Read the credentials the channel's auth scheme declares. Fails with
/// `Forbidden` for a cross-origin cookie socket and `Unauthorized` when
/// declared credentials are missing and cannot arrive in a first frame.
pub fn request_credentials(
    request: &RequestHeader,
    scheme: &ChannelAuthScheme,
    websocket: bool,
    is_https: bool,
) -> Result<RequestCredentials, ChannelError> {
    let (header, cookie) = extract_credentials(&request_headers_to_map(request), scheme);
    if websocket && cookie.is_some() && !websocket_origin_matches(request, is_https) {
        return Err(ChannelError::Forbidden);
    }
    let first_frame = websocket && auth_scheme_requires_header(scheme) && header.is_none();
    if !first_frame
        && auth_scheme_requires_declared_credentials(scheme)
        && header.is_none()
        && cookie.is_none()
    {
        return Err(ChannelError::Unauthorized);
    }
    Ok(RequestCredentials {
        header,
        cookie,
        first_frame,
    })
}

/// A socket is served only when the app granted the WebSocket transport.
pub fn require_websocket_grant(auth: &ChannelAuthResponse) -> Result<(), ChannelError> {
    if auth.transport == Some(ChannelTransport::Ws) {
        Ok(())
    } else {
        Err(ChannelError::Unsupported)
    }
}

/// Read the `tako.auth` frame of an upgraded socket and authorize it with the
/// app. Returns `None` after closing the socket with the matching close code.
/// An auth-frame cursor wins over the query's cursor or `from`.
pub async fn authorize_first_frame(
    session: &mut Session,
    reader: &mut WebSocketFrameReader,
    app: &AppAuthorizer,
    channel: &str,
    params: serde_json::Value,
    cookie: Option<String>,
    requested: ReplayFrom,
) -> Result<Option<(ChannelAuthResponse, ReplayFrom)>> {
    let Some(frame) = read_first_text_frame(session, reader, FIRST_FRAME_TIMEOUT).await? else {
        return close(session, ChannelCloseCode::AuthFrameMissing).await;
    };
    let Some(parsed) = String::from_utf8(frame)
        .ok()
        .and_then(|text| parse_first_frame(&text).ok())
    else {
        return close(session, ChannelCloseCode::AuthFrameMalformed).await;
    };
    let auth = match app
        .authorize(
            ChannelOperation::Connect,
            channel,
            params,
            parsed.header_value,
            cookie,
        )
        .await
    {
        Ok(auth) if require_websocket_grant(&auth).is_ok() => auth,
        Err(ChannelError::NotDefined) => {
            return close(session, ChannelCloseCode::ChannelUnknown).await;
        }
        Ok(_) | Err(_) => return close(session, ChannelCloseCode::VerifyRejected).await,
    };
    Ok(Some((auth, requested.or_cursor(parsed.last_message_id))))
}

async fn close<T>(session: &mut Session, code: ChannelCloseCode) -> Result<Option<T>> {
    session
        .write_response_body(
            Some(Bytes::from(websocket_close_frame(
                code.ws_close_code(),
                code.name(),
            ))),
            true,
        )
        .await?;
    Ok(None)
}

async fn read_first_text_frame(
    session: &mut Session,
    reader: &mut WebSocketFrameReader,
    timeout: Duration,
) -> Result<Option<Vec<u8>>> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        enum FirstFrameAction {
            Read(Option<Bytes>),
            Timeout,
        }

        let action = {
            let mut sleep = std::pin::pin!(tokio::time::sleep_until(deadline));
            let mut read = std::pin::pin!(session.as_downstream_mut().read_body_or_idle(false));
            tokio::select! {
                body = &mut read => FirstFrameAction::Read(body?),
                _ = &mut sleep => FirstFrameAction::Timeout,
            }
        };

        match action {
            FirstFrameAction::Timeout | FirstFrameAction::Read(None) => return Ok(None),
            FirstFrameAction::Read(Some(chunk)) => {
                reader.extend(&chunk);
                while let Some(frame) = reader.next_frame().map_err(|error| {
                    Error::explain(
                        ErrorType::InvalidHTTPHeader,
                        format!("Invalid websocket frame: {error}"),
                    )
                })? {
                    match frame.opcode {
                        0x1 => return Ok(Some(frame.payload)),
                        0x8 => return Ok(None),
                        0x9 => {
                            session
                                .write_response_body(
                                    Some(Bytes::from(websocket_pong_frame(&frame.payload))),
                                    false,
                                )
                                .await?;
                        }
                        0xA => {}
                        _ => return Ok(None),
                    }
                }
            }
        }
    }
}

/// HTTP status and JSON body for a channel request that fails before serving.
pub fn error_response(error: &ChannelError) -> (u16, serde_json::Value) {
    let (status, message) = match error {
        ChannelError::Unauthorized => (401, "Unauthorized"),
        ChannelError::Forbidden => (403, "Forbidden"),
        ChannelError::NotDefined => (404, "Channel not defined"),
        ChannelError::StaleCursor => (410, "Channel cursor is outside the replay window"),
        ChannelError::StreamClosed => (409, "Channel stream is closed"),
        ChannelError::BadRequest(message) => {
            return (400, serde_json::json!({ "error": message }));
        }
        ChannelError::Unsupported => (400, "Channel transport is not enabled"),
        ChannelError::InvalidPath => (404, "Not found"),
        ChannelError::AuthUnavailable => (503, "Service Unavailable"),
        ChannelError::Storage(_) => (500, "Internal Server Error"),
    };
    (status, serde_json::json!({ "error": message }))
}

/// Cookie credentials are ambient browser authority. Require an explicit,
/// same-origin handshake before forwarding them to the app's auth callback.
pub fn websocket_origin_matches(request: &RequestHeader, is_https: bool) -> bool {
    let mut origins = request.headers.get_all("origin").iter();
    let Some(origin) = origins.next().and_then(|value| value.to_str().ok()) else {
        return false;
    };
    if origins.next().is_some() {
        return false;
    }
    let Ok(origin) = reqwest::Url::parse(origin) else {
        return false;
    };
    if !matches!(origin.scheme(), "http" | "https")
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return false;
    }
    let scheme = if is_https { "https" } else { "http" };
    let host = request
        .uri
        .authority()
        .map(|authority| authority.as_str())
        .or_else(|| request.headers.get("host").and_then(|h| h.to_str().ok()))
        .unwrap_or("");
    let Ok(expected) = reqwest::Url::parse(&format!("{scheme}://{host}")) else {
        return false;
    };
    origin.origin() == expected.origin()
}

pub fn auth_scheme_requires_declared_credentials(scheme: &ChannelAuthScheme) -> bool {
    matches!(
        scheme,
        ChannelAuthScheme::Required {
            header_name: Some(_),
            cookie_name: _
        } | ChannelAuthScheme::Required {
            header_name: _,
            cookie_name: Some(_)
        }
    )
}

pub fn auth_scheme_requires_header(scheme: &ChannelAuthScheme) -> bool {
    matches!(
        scheme,
        ChannelAuthScheme::Required {
            header_name: Some(_),
            ..
        }
    )
}

pub fn request_headers_to_map(request: &RequestHeader) -> HashMap<String, String> {
    request
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect()
}

pub fn extract_credentials(
    headers: &HashMap<String, String>,
    scheme: &ChannelAuthScheme,
) -> (Option<ChannelHeaderValue>, Option<String>) {
    let ChannelAuthScheme::Required {
        header_name,
        cookie_name,
    } = scheme
    else {
        return (None, None);
    };

    let header = header_name
        .as_ref()
        .and_then(|name| headers.get(&name.to_ascii_lowercase()))
        .map(|raw| ChannelHeaderValue::parse(raw));

    let cookie = cookie_name.as_ref().and_then(|name| {
        let raw = headers.get("cookie")?;
        raw.split(';').find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then(|| value.to_string())
        })
    });

    (header, cookie)
}

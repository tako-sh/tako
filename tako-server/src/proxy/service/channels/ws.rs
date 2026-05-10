use crate::channels::{ChannelAuthResponse, ChannelError, ChannelOperation, ChannelStore};
use crate::channels_ws::{
    WebSocketFrameReader, build_websocket_upgrade_response, parse_first_frame,
    parse_publish_payload, websocket_close_frame, websocket_ping_frame, websocket_pong_frame,
    websocket_text_frame,
};
use crate::proxy::TakoProxy;
use bytes::Bytes;
use pingora_core::prelude::*;
use pingora_proxy::Session;
use std::time::Duration;
use tako_channels::close_codes::ChannelCloseCode;

pub(super) enum ChannelWebSocketAuth {
    Authorized(ChannelAuthResponse),
    FirstFrame {
        endpoint: String,
        internal_host: String,
        internal_token: String,
        params: serde_json::Value,
        cookie: Option<String>,
    },
}

impl TakoProxy {
    async fn write_websocket_close(
        &self,
        session: &mut Session,
        code: ChannelCloseCode,
        end: bool,
    ) -> Result<bool> {
        session
            .write_response_body(
                Some(Bytes::from(websocket_close_frame(
                    code.ws_close_code(),
                    code.name(),
                ))),
                end,
            )
            .await?;
        Ok(true)
    }

    pub(super) async fn write_channel_websocket(
        &self,
        session: &mut Session,
        store: &ChannelStore,
        channel: &str,
        query_after: Option<i64>,
        auth_mode: ChannelWebSocketAuth,
    ) -> Result<bool> {
        if !session.as_downstream().is_upgrade_req() {
            return self
                .write_json_response(
                    session,
                    400,
                    &serde_json::json!({ "error": "WebSocket upgrade required" }),
                )
                .await;
        }

        let header = match build_websocket_upgrade_response(session.req_header()) {
            Ok(header) => header,
            Err(error) => return self.write_channel_error(session, error).await,
        };
        session
            .write_response_header(Box::new(header), false)
            .await?;

        let mut reader = WebSocketFrameReader::default();
        let (auth, requested_after) = match auth_mode {
            ChannelWebSocketAuth::Authorized(auth) => (auth, query_after),
            ChannelWebSocketAuth::FirstFrame {
                endpoint,
                internal_host,
                internal_token,
                params,
                cookie,
            } => {
                let frame = match read_first_websocket_text_frame(
                    session,
                    &mut reader,
                    Duration::from_secs(5),
                )
                .await?
                {
                    Some(frame) => frame,
                    None => {
                        return self
                            .write_websocket_close(
                                session,
                                ChannelCloseCode::AuthFrameMissing,
                                true,
                            )
                            .await;
                    }
                };

                let text = match String::from_utf8(frame) {
                    Ok(text) => text,
                    Err(_) => {
                        return self
                            .write_websocket_close(
                                session,
                                ChannelCloseCode::AuthFrameMalformed,
                                true,
                            )
                            .await;
                    }
                };

                let parsed = match parse_first_frame(&text) {
                    Ok(parsed) => parsed,
                    Err(_) => {
                        return self
                            .write_websocket_close(
                                session,
                                ChannelCloseCode::AuthFrameMalformed,
                                true,
                            )
                            .await;
                    }
                };

                let auth = match tako_channels::authorize_channel_request(
                    &endpoint,
                    &internal_host,
                    crate::instances::INTERNAL_TOKEN_HEADER,
                    &internal_token,
                    ChannelOperation::Connect,
                    channel,
                    params,
                    parsed.header_value,
                    cookie,
                )
                .await
                {
                    Ok(auth) => auth,
                    Err(ChannelError::NotDefined) => {
                        return self
                            .write_websocket_close(session, ChannelCloseCode::ChannelUnknown, true)
                            .await;
                    }
                    Err(_) => {
                        return self
                            .write_websocket_close(session, ChannelCloseCode::VerifyRejected, true)
                            .await;
                    }
                };

                (auth, parsed.last_message_id.or(query_after))
            }
        };

        if let Err(error) = store.sync_channel(channel, &auth) {
            tracing::error!("failed to sync websocket channel metadata: {error}");
            return self
                .write_websocket_close(session, ChannelCloseCode::VerifyRejected, true)
                .await;
        }

        let mut after = match store.replay_cursor(channel, requested_after) {
            Ok(after) => after,
            Err(ChannelError::StaleCursor) => {
                return self
                    .write_websocket_close(session, ChannelCloseCode::ReplayTooOld, true)
                    .await;
            }
            Err(error) => {
                tracing::error!("failed to resolve websocket replay cursor: {error}");
                return self
                    .write_websocket_close(session, ChannelCloseCode::VerifyRejected, true)
                    .await;
            }
        };

        let keepalive_interval = Duration::from_millis(auth.keepalive_interval_ms.max(1));
        let max_connection_lifetime = Duration::from_millis(auth.max_connection_lifetime_ms.max(1));
        let started_at = tokio::time::Instant::now();
        let mut next_ping = started_at + keepalive_interval;

        loop {
            let messages = store.read_after(channel, after, 100).map_err(|error| {
                Error::explain(
                    ErrorType::InternalError,
                    format!("Failed to read channel replay: {error}"),
                )
            })?;

            if !messages.is_empty() {
                for message in messages {
                    let encoded = serde_json::to_string(&message).map_err(|error| {
                        Error::explain(
                            ErrorType::InternalError,
                            format!("Failed to encode websocket payload: {error}"),
                        )
                    })?;
                    session
                        .write_response_body(
                            Some(Bytes::from(websocket_text_frame(&encoded))),
                            false,
                        )
                        .await?;
                    after = Some(
                        message
                            .id
                            .parse::<i64>()
                            .expect("channel ids are always numeric"),
                    );
                }
                next_ping = tokio::time::Instant::now() + keepalive_interval;
            }

            if started_at.elapsed() >= max_connection_lifetime {
                session
                    .write_response_body(
                        Some(Bytes::from(websocket_close_frame(
                            1000,
                            "connection expired",
                        ))),
                        true,
                    )
                    .await?;
                return Ok(true);
            }

            let ping_deadline = next_ping;
            let sleep_until = std::cmp::min(ping_deadline, started_at + max_connection_lifetime);
            enum WebSocketAction {
                Read(Option<Bytes>),
                Tick,
            }

            let action = {
                let mut sleep = std::pin::pin!(tokio::time::sleep_until(sleep_until));
                let mut read = std::pin::pin!(session.as_downstream_mut().read_body_or_idle(true));
                tokio::select! {
                    body = &mut read => WebSocketAction::Read(body?),
                    _ = &mut sleep => WebSocketAction::Tick,
                }
            };

            match action {
                WebSocketAction::Read(Some(chunk)) => {
                    reader.extend(&chunk);
                    while let Some(frame) = reader.next_frame().map_err(|error| {
                        Error::explain(
                            ErrorType::InvalidHTTPHeader,
                            format!("Invalid websocket frame: {error}"),
                        )
                    })? {
                        match frame.opcode {
                            0x1 => {
                                if is_websocket_auth_payload(&frame.payload) {
                                    continue;
                                }
                                let payload =
                                    parse_publish_payload(&frame.payload).map_err(|error| {
                                        Error::explain(
                                            ErrorType::InvalidHTTPHeader,
                                            format!("Invalid websocket publish payload: {error}"),
                                        )
                                    })?;
                                store.append(channel, &payload).map_err(|error| {
                                    Error::explain(
                                        ErrorType::InternalError,
                                        format!(
                                            "Failed to append websocket channel payload: {error}"
                                        ),
                                    )
                                })?;
                            }
                            0x8 => {
                                session
                                    .write_response_body(
                                        Some(Bytes::from(websocket_close_frame(1000, "closing"))),
                                        true,
                                    )
                                    .await?;
                                return Ok(true);
                            }
                            0x9 => {
                                session
                                    .write_response_body(
                                        Some(Bytes::from(websocket_pong_frame(&frame.payload))),
                                        false,
                                    )
                                    .await?;
                            }
                            0xA => {}
                            _ => {
                                session
                                    .write_response_body(
                                        Some(Bytes::from(websocket_close_frame(
                                            1003,
                                            "unsupported frame",
                                        ))),
                                        true,
                                    )
                                    .await?;
                                return Ok(true);
                            }
                        }
                    }
                    next_ping = tokio::time::Instant::now() + keepalive_interval;
                }
                WebSocketAction::Read(None) => return Ok(true),
                WebSocketAction::Tick => {
                    if tokio::time::Instant::now() >= next_ping {
                        session
                            .write_response_body(
                                Some(Bytes::from(websocket_ping_frame(b""))),
                                false,
                            )
                            .await?;
                        next_ping = tokio::time::Instant::now() + keepalive_interval;
                    }
                }
            }
        }
    }
}

async fn read_first_websocket_text_frame(
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
            let mut read = std::pin::pin!(session.as_downstream_mut().read_body_or_idle(true));
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

pub(super) fn is_websocket_auth_payload(payload: &[u8]) -> bool {
    std::str::from_utf8(payload)
        .ok()
        .is_some_and(|text| parse_first_frame(text).is_ok())
}

use super::*;
use crate::{ChannelAuthResponse, ChannelStore, PRESENCE_CHANNEL_PREFIX, PresenceSession};
use bytes::Bytes;
use pingora_core::prelude::*;
use pingora_proxy::Session;
use std::{sync::Arc, time::Duration};

async fn write_close(
    session: &mut Session,
    code: crate::ChannelCloseCode,
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

/// Serve an upgraded, authorized channel socket in production or local dev.
pub async fn serve(
    session: &mut Session,
    store: &Arc<ChannelStore>,
    channel: &str,
    requested_after: Option<i64>,
    auth: ChannelAuthResponse,
    params: serde_json::Value,
    mut reader: WebSocketFrameReader,
) -> Result<bool> {
    if channel.starts_with(crate::live_store::STORE_CHANNEL_PREFIX) {
        return write_close(session, crate::ChannelCloseCode::VerifyRejected, true).await;
    }
    if let Err(error) = store.sync_channel_async(channel, &auth).await {
        tracing::error!("failed to sync websocket channel metadata: {error}");
        return write_close(session, crate::ChannelCloseCode::VerifyRejected, true).await;
    }

    let mut presence = if channel.starts_with(PRESENCE_CHANNEL_PREFIX) {
        let scope = crate::presence_scope(channel, &params);
        let presence = PresenceSession::new(store.clone(), &scope, auth.subject.clone())
            .map_err(|e| Error::explain(ErrorType::InternalError, e.to_string()))?;
        session
            .write_response_body(
                Some(Bytes::from(websocket_text_frame(
                    r#"{"type":"tako.presence.ready"}"#,
                ))),
                false,
            )
            .await?;
        Some(presence)
    } else {
        None
    };
    let mut after = match store.replay_cursor_async(channel, requested_after).await {
        Ok(after) => after,
        Err(ChannelError::StaleCursor) => {
            return write_close(session, crate::ChannelCloseCode::ReplayTooOld, true).await;
        }
        Err(error) => {
            tracing::error!("failed to resolve websocket replay cursor: {error}");
            return write_close(session, crate::ChannelCloseCode::VerifyRejected, true).await;
        }
    };

    let keepalive_interval = Duration::from_millis(auth.keepalive_interval_ms.max(1));
    let max_connection_lifetime = Duration::from_millis(auth.max_connection_lifetime_ms.max(1));
    let started_at = tokio::time::Instant::now();
    let mut next_ping = started_at + keepalive_interval;
    let mut changes = store.changes();
    let mut read_replay = presence.is_none();
    let mut buffered = true;

    loop {
        let next_presence = tokio::time::Instant::now() + Duration::from_secs(1);
        if let Some(presence) = &mut presence {
            changes.borrow_and_update();
            if let Some(snapshot) = presence
                .snapshot()
                .await
                .map_err(|e| Error::explain(ErrorType::InternalError, e.to_string()))?
            {
                session
                    .write_response_body(Some(Bytes::from(websocket_text_frame(&snapshot))), false)
                    .await?;
            }
        }
        let messages = if read_replay {
            changes.borrow_and_update();
            store
                .read_after_async(channel, after, 100)
                .await
                .map_err(|error| {
                    Error::explain(
                        ErrorType::InternalError,
                        format!("Failed to read channel replay: {error}"),
                    )
                })?
        } else {
            Vec::new()
        };
        read_replay = messages.len() == 100;

        if !messages.is_empty() {
            for message in messages {
                let encoded = serde_json::to_string(&message).map_err(|error| {
                    Error::explain(
                        ErrorType::InternalError,
                        format!("Failed to encode websocket payload: {error}"),
                    )
                })?;
                session
                    .write_response_body(Some(Bytes::from(websocket_text_frame(&encoded))), false)
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

        if read_replay {
            continue;
        }
        let mut sleep_until = next_ping.min(started_at + max_connection_lifetime);
        if presence.is_some() {
            sleep_until = sleep_until.min(next_presence);
        }
        enum WebSocketAction {
            Read(Option<Bytes>),
            Tick,
            Changed,
        }

        let action = if buffered {
            buffered = false;
            WebSocketAction::Read(Some(Bytes::new()))
        } else {
            let mut sleep = std::pin::pin!(tokio::time::sleep_until(sleep_until));
            let mut read = std::pin::pin!(session.as_downstream_mut().read_body_or_idle(false));
            tokio::select! {
                body = &mut read => WebSocketAction::Read(body?),
                _ = &mut sleep => WebSocketAction::Tick,
                _ = changes.changed() => WebSocketAction::Changed,
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
                            if let Some(presence) = &mut presence {
                                if presence.update(&frame.payload).await.is_err() {
                                    session
                                        .write_response_body(
                                            Some(Bytes::from(websocket_close_frame(
                                                4400,
                                                "invalid presence update",
                                            ))),
                                            true,
                                        )
                                        .await?;
                                    return Ok(true);
                                }
                                continue;
                            }
                            let payload =
                                parse_publish_payload(&frame.payload).map_err(|error| {
                                    Error::explain(
                                        ErrorType::InvalidHTTPHeader,
                                        format!("Invalid websocket publish payload: {error}"),
                                    )
                                })?;
                            store
                                .append_async(channel, &payload)
                                .await
                                .map_err(|error| {
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
            WebSocketAction::Changed => read_replay = presence.is_none(),
            WebSocketAction::Tick => {
                if tokio::time::Instant::now() >= next_ping {
                    session
                        .write_response_body(Some(Bytes::from(websocket_ping_frame(b""))), false)
                        .await?;
                    next_ping = tokio::time::Instant::now() + keepalive_interval;
                }
            }
        }
    }
}
